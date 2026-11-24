//! Declared lanes: the tag analysis and the checker.
//!
//! A slot or an object field may declare a [`Lane`]: "every value stored here
//! is na or has exactly this runtime tag". [`check_lanes`] establishes every
//! declaration of a program or rejects the program; nothing is trusted, and a
//! declaration that fails is a validation error, never "no claim".
//! `pinewasm_csil::validate` (`validate_program`) runs it on every program
//! that declares anything and does nothing at all for a program that
//! declares nothing.
//!
//! The analysis is separate from `typed_stacks` and does not change the
//! meaning of `AbstractTy`. `AbstractTy::Int` means "statically int": the
//! result of `PINE_INT_DIV` is `Int` and may hold a float cell at run time, as PineWASM represents it.
//! `Tag::Is(Lane::I64)` means "the runtime cell is an integer cell".
//!
//! # Semantics
//!
//! The domain of one stack cell or place is [`Tag`]: `Bottom`, `NaOnly`,
//! `Is(lane)`, `Top`. For a fixed environment (a tag per slot and per field)
//! [`lane_stacks`] computes, per function, the least fixpoint of the transfer
//! [`lane_pushes`] over the control-flow graph. Nothing is rejected during
//! the walk: tags only grow, and the obligations are judged once on the
//! result, so the verdict does not depend on the visit order.
//!
//! * The **checker** fixes the environment from the declarations: a declared
//!   place reads as its lane, every other place reads as `Top`. It then
//!   requires every store into a declared place to be na or the declared
//!   lane (obligations 2 to 4), the declaration tables to be well formed
//!   (obligation 1), and, when any field is declared, every `SET_FIELD` to go
//!   through a receiver with a derived object type (obligation 5). The
//!   declarations are thereby an inductive invariant of the VM:
//!   a slot that feeds its own store (`n := n + 1`) checks, because the read
//!   is taken at the declared lane.
//! * [`solve_lanes`] finds declarations: it solves places and stacks
//!   together as one least fixpoint, every place starting at `Bottom` and
//!   growing to the join of its stores. Its result is a proposal; only the
//!   checker's verdict on it counts.
//!
//! Soundness statement: if [`check_lanes`] accepts, then in
//! every VM execution each value read from a declared slot, or
//! from a declared field of an object whose receiver the analysis typed, is
//! na or carries the declared tag; and every stack cell tagged `Is(lane)` at
//! the entry of an instruction holds na or a value of that lane.
//!
//! # Rows
//!
//! Each transfer row is checked by executing it:
//! `pinewasm_vm`'s `lane_rows` tests call the VM's own handlers for every
//! pure function, builtin series, collection function and TA result decoder
//! and compare the runtime cell with the row, and run whole programs with
//! every executed instruction's operand stack compared against
//! [`LaneFacts`]. A row the tests cannot execute is `Top`.
//!
//! # Style
//!
//! The kernel (`tag_join`, `pure_tag`, `lane_pushes`, `lane_stores`,
//! `lane_stacks`, `lane_events`) is written for extraction: index loops,
//! explicit tag comparisons, no closures, no iterator chains, no `f64` in any
//! type, and the program tables it reads passed in as plain vectors.
//! Everything from [`LaneError`] down is glue.

use super::{height_effect, height_successors, is_return, ta_arity_table, ValidationError};
use crate::{CsilInstr, CsilProgram};
pub use pinewasm_hir::TaResult;
use pinewasm_hir::{CollectionAbstract, InputKind, Lane, PureFunc};

/// Why a cell has no single runtime tag. Carried by [`Tag::Top`] so a
/// rejection, and the declaration writer's report, can name the root cause.
/// It never takes part in a tag comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// `PINE_INT_DIV`: in PineWASM's runtime an exact quotient is an integer
    /// cell, an uneven one a float cell.
    PineIntDiv,
    /// Two paths, or two stores, with different tags.
    Mixed,
    /// A collection element, or the placeholder a void collection call
    /// pushes.
    Collection,
    /// `GET_FIELD` through a receiver with no derived object type.
    UntypedReceiver,
    /// A place with no declaration (the checker's view of it).
    Undeclared,
    /// `CALL_TA` of `ta.valuewhen`, `fixnan`, `ta.mode` or `ta.range` over a
    /// statically int source: the decoder may return a float cell, so the
    /// tag is not fixed.
    FractionalIntTa,
    /// A `CALL_PURE` whose result tag the table does not fix.
    Pure,
    /// Any other value whose tag the table does not fix.
    Host,
    /// A place whose stores are all na (the writer's view of it).
    OnlyNa,
    /// A place with no reachable store (the writer's view of it).
    NeverStored,
    /// A field of a program whose field declarations obligation 5 dropped.
    FieldsDropped,
    /// `COPY_OBJECT` of a source that is not an object of the copied type.
    CopySource,
}

/// The tag lattice of one stack cell or place: `Bottom` (unreached, or a
/// place nothing stored into yet), `NaOnly`, `Is(lane)`, `Top`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Bottom,
    NaOnly,
    Is(Lane),
    Top(Why),
}

/// Whether two lanes are the same lane (`==` on `Lane`, spelled out).
fn same_lane(left: Lane, right: Lane) -> bool {
    match (left, right) {
        (Lane::F64, Lane::F64)
        | (Lane::I64, Lane::I64)
        | (Lane::Bool, Lane::Bool)
        | (Lane::Color, Lane::Color)
        | (Lane::Str, Lane::Str)
        | (Lane::Ref, Lane::Ref) => true,
        (Lane::Obj(a), Lane::Obj(b)) => a == b,
        _ => false,
    }
}

/// Whether two tags are the same lattice point. The cause carried by `Top`
/// is not compared.
pub fn same_tag(left: Tag, right: Tag) -> bool {
    match (left, right) {
        (Tag::Bottom, Tag::Bottom) | (Tag::NaOnly, Tag::NaOnly) | (Tag::Top(_), Tag::Top(_)) => {
            true
        }
        (Tag::Is(a), Tag::Is(b)) => same_lane(a, b),
        _ => false,
    }
}

/// The join. Two different lanes join to `Top`; `NaOnly` joined with
/// `Is(l)` is `Is(l)`. A `Top` keeps the cause it already has.
pub fn tag_join(left: Tag, right: Tag) -> Tag {
    match (left, right) {
        (Tag::Bottom, other) | (other, Tag::Bottom) => other,
        (Tag::Top(why), _) | (_, Tag::Top(why)) => Tag::Top(why),
        (Tag::NaOnly, other) | (other, Tag::NaOnly) => other,
        (Tag::Is(a), Tag::Is(b)) => {
            if same_lane(a, b) {
                Tag::Is(a)
            } else {
                Tag::Top(Why::Mixed)
            }
        }
    }
}

/// Whether a stored value satisfies a declared lane: it is na, or it has
/// exactly that tag. `Top` never does.
pub fn tag_fits(value: Tag, lane: Lane) -> bool {
    match value {
        Tag::Bottom | Tag::NaOnly => true,
        Tag::Is(found) => same_lane(found, lane),
        Tag::Top(_) => false,
    }
}

/// What a read of a place yields: a place nothing stored into reads na.
fn place_read(place: Tag) -> Tag {
    match place {
        Tag::Bottom => Tag::NaOnly,
        other => other,
    }
}

/// The tag of every place: the checker fills it from the declarations
/// (`Is(l)` when declared, `Top(Undeclared)` otherwise); the declaration
/// writer uses it as the unknowns of its fixpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneEnv {
    /// By slot number.
    pub slots: Vec<Tag>,
    /// By `type_id`, then `field_idx` (`field_count` entries each).
    pub fields: Vec<Vec<Tag>>,
}

/// The program tables the kernel reads, as plain vectors.
#[derive(Debug, Clone)]
pub struct LaneTables {
    /// `(operand_count, result_arity)` per TA state.
    pub ta_arity: Vec<(u32, u32)>,
    /// Result tag per TA state.
    pub ta_result: Vec<Tag>,
    /// `READ_INPUT` result tag per input.
    pub inputs: Vec<Tag>,
}

/// The tag of a `READ_INPUT`: `resolve_input` in the VM yields the
/// declared kind's cell (it widens an int override of a float input, and
/// refuses a float override of an int input), so each kind has one tag.
pub fn input_tag(kind: InputKind) -> Tag {
    match kind {
        InputKind::Int | InputKind::Time | InputKind::Enum => Tag::Is(Lane::I64),
        InputKind::Float | InputKind::Source | InputKind::Price => Tag::Is(Lane::F64),
        InputKind::Bool => Tag::Is(Lane::Bool),
        InputKind::String => Tag::Is(Lane::Str),
        InputKind::Color => Tag::Is(Lane::Color),
    }
}

/// The tag of a `CALL_TA` result. An int result is an integer cell
/// (`ta_site::decode` truncates) except for `ValueWhenInt`, `FixnanInt`,
/// `ModeInt` and `RangeInt`, whose decoder returns a float cell for a
/// fractional value from a statically int source. `MedianInt` truncates and
/// stays an integer cell.
pub fn ta_result_tag(func: pinewasm_hir::TaFunc) -> Tag {
    if matches!(
        func,
        pinewasm_hir::TaFunc::ValueWhenInt
            | pinewasm_hir::TaFunc::FixnanInt
            | pinewasm_hir::TaFunc::ModeInt
            | pinewasm_hir::TaFunc::RangeInt
    ) {
        return Tag::Top(Why::FractionalIntTa);
    }
    match func.engine_result() {
        TaResult::Float => Tag::Is(Lane::F64),
        TaResult::Int => Tag::Is(Lane::I64),
        TaResult::Bool => Tag::Is(Lane::Bool),
        TaResult::Color => Tag::Is(Lane::Color),
    }
}

/// The tables of `program` (glue, not kernel).
pub fn lane_tables(program: &CsilProgram) -> LaneTables {
    LaneTables {
        ta_arity: ta_arity_table(&program.ta_states),
        ta_result: program
            .ta_states
            .iter()
            .map(|state| ta_result_tag(state.func))
            .collect(),
        inputs: program
            .inputs
            .iter()
            .map(|input| input_tag(input.kind))
            .collect(),
    }
}

/// Whether an integer-arithmetic operand keeps the result an integer cell.
fn int_operand(tag: Tag) -> bool {
    tag_fits(tag, Lane::I64)
}

/// The cause to report when an operand makes a result `Top`.
fn cause_of(tag: Tag, fallback: Why) -> Why {
    match tag {
        Tag::Top(why) => why,
        _ => fallback,
    }
}

/// How `CALL_PURE` determines the cell kind of its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureRow {
    /// Always na or a cell of this lane, whatever the operands are.
    Fixed(Lane),
    /// The cell kind of operand 0 when that is an integer or a float cell
    /// (`math.abs`, `math.sign`).
    Operand,
    /// One of its two operands (`nz`).
    Either,
    /// An integer cell when every operand is one; a float cell when every
    /// operand is numeric; na when any operand is na (`math.min`,
    /// `math.max`).
    MinMax,
    /// No single cell kind, or a row the VM tests cannot execute.
    Unfixed,
}

/// The row of every pure function. The match is exhaustive on purpose: a new
/// function does not compile until its row is decided.
#[allow(clippy::match_same_arms)]
pub fn pure_row(func: PureFunc, argc: u8) -> PureRow {
    use PureRow::{Either, Fixed, MinMax, Operand, Unfixed};
    match func {
        // na(x) is a plain bool, never na.
        PureFunc::Na => Fixed(Lane::Bool),
        PureFunc::Nz => Either,
        PureFunc::MathAbs | PureFunc::MathSign => Operand,
        PureFunc::MathMin | PureFunc::MathMax => MinMax,
        // The identity on an integer cell, `f64_to_int` on a float cell.
        PureFunc::MathFloor | PureFunc::MathCeil | PureFunc::CastInt => Fixed(Lane::I64),
        // One argument rounds to an int; with a precision it stays a float.
        PureFunc::MathRound => {
            if argc == 1 {
                Fixed(Lane::I64)
            } else {
                Fixed(Lane::F64)
            }
        }
        PureFunc::MathSqrt
        | PureFunc::MathPow
        | PureFunc::MathExp
        | PureFunc::MathLog
        | PureFunc::MathLog10
        | PureFunc::MathSin
        | PureFunc::MathCos
        | PureFunc::MathTan
        | PureFunc::MathAsin
        | PureFunc::MathAcos
        | PureFunc::MathAtan
        | PureFunc::MathAvg
        | PureFunc::MathToDegrees
        | PureFunc::MathToRadians
        | PureFunc::MathRandom
        | PureFunc::MathRoundToMintick
        | PureFunc::CastFloat
        | PureFunc::StrTonumber
        | PureFunc::ColorRFloat
        | PureFunc::ColorGFloat
        | PureFunc::ColorBFloat
        | PureFunc::ColorTFloat => Fixed(Lane::F64),
        PureFunc::StrTostring
        | PureFunc::StrFormat
        | PureFunc::StrSubstring
        | PureFunc::StrReplace
        | PureFunc::StrReplaceAll
        | PureFunc::StrUpper
        | PureFunc::StrLower
        | PureFunc::StrTrim
        | PureFunc::StrRepeat
        | PureFunc::StrMatch
        | PureFunc::StrFormatTime
        | PureFunc::TimeframeFromSeconds
        | PureFunc::Ticker(_)
        | PureFunc::SyminfoPrefix
        | PureFunc::SyminfoTicker => Fixed(Lane::Str),
        PureFunc::StrContains
        | PureFunc::StrStartswith
        | PureFunc::StrEndswith
        | PureFunc::CastBool => Fixed(Lane::Bool),
        PureFunc::StrLength
        | PureFunc::StrPos
        | PureFunc::ColorR
        | PureFunc::ColorG
        | PureFunc::ColorB
        | PureFunc::ColorT
        | PureFunc::TimeframeInSeconds
        | PureFunc::TimeOpen
        | PureFunc::TimeClose
        | PureFunc::Timestamp
        | PureFunc::TimestampTz
        | PureFunc::WeekOfYear
        | PureFunc::Calendar(_) => Fixed(Lane::I64),
        PureFunc::ColorNew
        | PureFunc::ColorRgb
        | PureFunc::ColorFromGradient
        | PureFunc::CastColor => Fixed(Lane::Color),
        PureFunc::StrategyConvertToAccount | PureFunc::StrategyConvertToSymbol => Fixed(Lane::F64),
        // Not executed by the VM row tests (they need strategy trade records).
        PureFunc::StrategyClosedTrade(_)
        | PureFunc::StrategyOpenTrade(_)
        | PureFunc::StrategyDefaultEntryQty => Unfixed,
    }
}

/// Result tag of `CALL_PURE` from the operand tags (`popped[0]` is the old
/// top of stack): [`pure_row`] applied to the operands.
pub fn pure_tag(func: PureFunc, argc: u8, popped: &[Tag]) -> Tag {
    match pure_row(func, argc) {
        PureRow::Fixed(lane) => Tag::Is(lane),
        PureRow::Unfixed => Tag::Top(Why::Pure),
        PureRow::Operand => {
            if popped.is_empty() {
                Tag::Top(Why::Pure)
            } else {
                match popped[0] {
                    Tag::Is(Lane::I64) => Tag::Is(Lane::I64),
                    Tag::Is(Lane::F64) => Tag::Is(Lane::F64),
                    Tag::NaOnly | Tag::Bottom => Tag::NaOnly,
                    other => Tag::Top(cause_of(other, Why::Pure)),
                }
            }
        }
        PureRow::Either => {
            if popped.len() < 2 {
                Tag::Top(Why::Pure)
            } else {
                tag_join(popped[0], popped[1])
            }
        }
        PureRow::MinMax => {
            let mut all_int = true;
            let mut all_numeric = true;
            let mut any_na = false;
            let mut cause = Why::Pure;
            let mut i = 0;
            while i < popped.len() {
                match popped[i] {
                    Tag::Is(Lane::I64) => {}
                    Tag::NaOnly | Tag::Bottom => any_na = true,
                    Tag::Is(Lane::F64) => all_int = false,
                    other => {
                        all_int = false;
                        all_numeric = false;
                        cause = cause_of(other, cause);
                    }
                }
                i += 1;
            }
            if any_na {
                Tag::NaOnly
            } else if all_int {
                Tag::Is(Lane::I64)
            } else if all_numeric {
                Tag::Is(Lane::F64)
            } else {
                Tag::Top(cause)
            }
        }
    }
}

/// The tag of a builtin series read.
pub fn builtin_tag(source: pinewasm_hir::BuiltinSeries) -> Tag {
    // The one series the VM row tests cannot produce a value for.
    if matches!(
        source,
        pinewasm_hir::BuiltinSeries::StrategyMarginLiquidationPrice
    ) {
        Tag::Top(Why::Host)
    } else if source.is_bool() {
        Tag::Is(Lane::Bool)
    } else if source.is_int() {
        Tag::Is(Lane::I64)
    } else if source.is_string() {
        Tag::Is(Lane::Str)
    } else {
        Tag::Is(Lane::F64)
    }
}

/// The tag of a `CALL_COLLECTION` result, from the function's result class.
pub fn collection_tag(func: pinewasm_hir::CollectionFunc) -> Tag {
    match func.result() {
        CollectionAbstract::Ref => Tag::Is(Lane::Ref),
        CollectionAbstract::Int => Tag::Is(Lane::I64),
        CollectionAbstract::Bool => Tag::Is(Lane::Bool),
        CollectionAbstract::Str => Tag::Is(Lane::Str),
        CollectionAbstract::Float => Tag::Is(Lane::F64),
        CollectionAbstract::Elem | CollectionAbstract::Void => Tag::Top(Why::Collection),
    }
}

/// The tags `instr` pushes, in push order, from the popped operand tags
/// (`popped[0]` is the old top of stack). Always `pushes` entries.
#[allow(clippy::too_many_lines)]
pub fn lane_pushes(
    tables: &LaneTables,
    env: &LaneEnv,
    instr: &CsilInstr,
    popped: &[Tag],
    pushes: u32,
) -> Vec<Tag> {
    let mut pushed: Vec<Tag> = match instr {
        CsilInstr::PushNa => vec![Tag::NaOnly],
        CsilInstr::PushBool(_) => vec![Tag::Is(Lane::Bool)],
        CsilInstr::PushI64(_) => vec![Tag::Is(Lane::I64)],
        CsilInstr::PushF64(_) => vec![Tag::Is(Lane::F64)],
        CsilInstr::PushStr(_) => vec![Tag::Is(Lane::Str)],
        CsilInstr::PushColor { .. } => vec![Tag::Is(Lane::Color)],
        CsilInstr::Dup => {
            if popped.is_empty() {
                Vec::new()
            } else {
                vec![popped[0], popped[0]]
            }
        }
        CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
            vec![builtin_tag(*source)]
        }
        CsilInstr::ReadUserSlot { slot, .. } | CsilInstr::ReadUserSlotDynamic { slot } => {
            let index = *slot as usize;
            vec![if index < env.slots.len() {
                place_read(env.slots[index])
            } else {
                Tag::Top(Why::Host)
            }]
        }
        CsilInstr::ReadInput { input } => {
            let index = *input as usize;
            vec![if index < tables.inputs.len() {
                tables.inputs[index]
            } else {
                Tag::Top(Why::Host)
            }]
        }
        CsilInstr::ReadSecurity { .. } | CsilInstr::ReadRequest { .. } => vec![Tag::Is(Lane::F64)],
        CsilInstr::ReadSecurityExpr { expr, .. } => vec![if expr.result_is_bool() {
            Tag::Is(Lane::Bool)
        } else {
            Tag::Is(Lane::F64)
        }],
        CsilInstr::BuildFootprint { .. }
        | CsilInstr::ReadSecurityLowerTf { .. }
        | CsilInstr::ReadPivotLevels { .. }
        | CsilInstr::CallPivotPointLevels { .. }
        | CsilInstr::ReadDrawingsAll { .. } => vec![Tag::Is(Lane::Ref)],
        CsilInstr::ReadTimeTf { .. } => vec![Tag::Is(Lane::I64)],
        // The checked integer operators stay exact on integer cells; a float
        // operand (a fractional quotient) makes the result a float.
        CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul | CsilInstr::IntRem => {
            if popped.len() < 2 {
                Vec::new()
            } else if int_operand(popped[0]) && int_operand(popped[1]) {
                vec![Tag::Is(Lane::I64)]
            } else {
                let cause = cause_of(popped[1], cause_of(popped[0], Why::Mixed));
                vec![Tag::Top(cause)]
            }
        }
        // `INT_DIV` pops two integer cells or stops the run.
        CsilInstr::IntDiv => vec![Tag::Is(Lane::I64)],
        CsilInstr::PineIntDiv => vec![Tag::Top(Why::PineIntDiv)],
        CsilInstr::IntNeg => {
            if popped.is_empty() {
                Vec::new()
            } else if int_operand(popped[0]) {
                vec![Tag::Is(Lane::I64)]
            } else {
                vec![Tag::Top(cause_of(popped[0], Why::Mixed))]
            }
        }
        CsilInstr::F64Add
        | CsilInstr::F64Sub
        | CsilInstr::F64Mul
        | CsilInstr::F64Div
        | CsilInstr::F64Rem
        | CsilInstr::F64Neg
        | CsilInstr::IntToF64
        | CsilInstr::WindowMean { .. }
        | CsilInstr::WindowMeanDynamic { .. }
        | CsilInstr::WindowExtremumDynamic { .. }
        | CsilInstr::EmaSimpleDynamic { .. } => vec![Tag::Is(Lane::F64)],
        CsilInstr::WindowStatisticDynamic { func, .. } => vec![match func.engine_result() {
            TaResult::Int => Tag::Top(Why::Host),
            _ => Tag::Is(Lane::F64),
        }],
        CsilInstr::CallPure { func, argc } => vec![pure_tag(*func, *argc, popped)],
        CsilInstr::CallCollection { func, .. } => vec![collection_tag(*func)],
        CsilInstr::NewObject { type_id, .. } => vec![Tag::Is(Lane::Obj(*type_id))],
        // The copy is retagged with the instruction's `type_id` whatever the
        // source is, so only a source of that same type keeps the claim.
        CsilInstr::CopyObject { type_id } => {
            if popped.is_empty() {
                Vec::new()
            } else {
                match popped[0] {
                    Tag::Is(Lane::Obj(source)) if source == *type_id => {
                        vec![Tag::Is(Lane::Obj(*type_id))]
                    }
                    other => vec![Tag::Top(cause_of(other, Why::CopySource))],
                }
            }
        }
        CsilInstr::GetField { field_idx } => {
            if popped.is_empty() {
                Vec::new()
            } else {
                match popped[0] {
                    Tag::Is(Lane::Obj(type_id)) => {
                        let t = type_id as usize;
                        let i = *field_idx as usize;
                        if t < env.fields.len() && i < env.fields[t].len() {
                            vec![place_read(env.fields[t][i])]
                        } else {
                            vec![Tag::Top(Why::Host)]
                        }
                    }
                    other => vec![Tag::Top(cause_of(other, Why::UntypedReceiver))],
                }
            }
        }
        // The same handle comes back.
        CsilInstr::RegisterDrawing { .. } => {
            if popped.is_empty() {
                Vec::new()
            } else {
                match popped[0] {
                    Tag::Is(Lane::Obj(type_id)) => vec![Tag::Is(Lane::Obj(type_id))],
                    Tag::Is(Lane::Ref) => vec![Tag::Is(Lane::Ref)],
                    other => vec![Tag::Top(cause_of(other, Why::Host))],
                }
            }
        }
        CsilInstr::CallTa { state } => {
            let index = *state as usize;
            let tag = if index < tables.ta_result.len() {
                tables.ta_result[index]
            } else {
                Tag::Top(Why::Host)
            };
            let mut values = Vec::new();
            let mut i = 0;
            while i < pushes {
                values.push(tag);
                i += 1;
            }
            values
        }
        // One of the two branch values (popped[0] is rhs, popped[1] is lhs).
        CsilInstr::Select => {
            if popped.len() < 2 {
                Vec::new()
            } else {
                vec![tag_join(popped[0], popped[1])]
            }
        }
        CsilInstr::CmpGt
        | CsilInstr::CmpGe
        | CsilInstr::CmpLt
        | CsilInstr::CmpLe
        | CsilInstr::CmpEq
        | CsilInstr::CmpNe
        | CsilInstr::BoolAnd
        | CsilInstr::BoolOr
        | CsilInstr::BoolNot => vec![Tag::Is(Lane::Bool)],
        CsilInstr::StrConcat => vec![Tag::Is(Lane::Str)],
        _ => Vec::new(),
    };
    // Anything the table does not name pushes `Top`.
    let mut i = pushed.len();
    while i < pushes as usize {
        pushed.push(Tag::Top(Why::Host));
        i += 1;
    }
    pushed
}

/// Where a store goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// `STORE_USER_SLOT_CURRENT { slot }`.
    Slot { slot: u32 },
    /// `SET_FIELD { field_idx }` through a receiver of derived type `type_id`.
    Field { type_id: u32, field_idx: u32 },
    /// Operand `field_idx` of `NEW_OBJECT { type_id }`.
    Constructor { type_id: u32, field_idx: u32 },
    /// `SET_FIELD` through a receiver with no derived object type.
    Untyped,
}

/// One store the program can execute, with the tag of the stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneStore {
    pub pc: usize,
    pub kind: StoreKind,
    pub value: Tag,
}

/// Appends the stores `instr` performs at `pc`, from its popped operand tags
/// (`popped[0]` is the old top of stack).
fn lane_stores(instr: &CsilInstr, pc: usize, popped: &[Tag], events: &mut Vec<LaneStore>) {
    match instr {
        CsilInstr::StoreUserSlotCurrent { slot } => {
            if !popped.is_empty() {
                events.push(LaneStore {
                    pc,
                    kind: StoreKind::Slot { slot: *slot },
                    value: popped[0],
                });
            }
        }
        // The value is on top, the receiver under it.
        CsilInstr::SetField { field_idx } => {
            if popped.len() >= 2 {
                let kind = match popped[1] {
                    Tag::Is(Lane::Obj(type_id)) => StoreKind::Field {
                        type_id,
                        field_idx: *field_idx,
                    },
                    _ => StoreKind::Untyped,
                };
                events.push(LaneStore {
                    pc,
                    kind,
                    value: popped[0],
                });
            }
        }
        // Field 0 is the deepest operand.
        CsilInstr::NewObject { type_id, .. } => {
            let count = popped.len();
            let mut i = 0;
            while i < count {
                #[allow(clippy::cast_possible_truncation)]
                let field_idx = i as u32;
                events.push(LaneStore {
                    pc,
                    kind: StoreKind::Constructor {
                        type_id: *type_id,
                        field_idx,
                    },
                    value: popped[count - 1 - i],
                });
                i += 1;
            }
        }
        _ => {}
    }
}

/// Why the kernel could not analyze a function. Each one means the program
/// did not pass `validate()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneErrKind {
    EmptyBody,
    StackEffect,
    Underflow,
    FallsOffEnd,
    MergeMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneErr {
    pub pc: usize,
    pub kind: LaneErrKind,
}

/// The operands `instr` pops from `stack`, top of stack first.
fn popped_operands(stack: &[Tag], pops: usize) -> Vec<Tag> {
    let mut popped = Vec::new();
    let mut k = 0;
    while k < pops {
        popped.push(stack[stack.len() - 1 - k]);
        k += 1;
    }
    popped
}

/// One transfer: pop the operands, push the result tags.
fn lane_step(
    tables: &LaneTables,
    env: &LaneEnv,
    instr: &CsilInstr,
    mut stack: Vec<Tag>,
) -> Result<Vec<Tag>, LaneErrKind> {
    let (pops, pushes) = match height_effect(instr, &tables.ta_arity) {
        Ok(effect) => effect,
        Err(_) => return Err(LaneErrKind::StackEffect),
    };
    let pops = pops as usize;
    if stack.len() < pops {
        return Err(LaneErrKind::Underflow);
    }
    let popped = popped_operands(&stack, pops);
    let mut k = 0;
    while k < pops {
        stack.pop();
        k += 1;
    }
    let pushed = lane_pushes(tables, env, instr, &popped, pushes);
    let mut i = 0;
    while i < pushed.len() {
        stack.push(pushed[i]);
        i += 1;
    }
    Ok(stack)
}

/// Whether two equal-height stacks carry the same tags.
fn same_stack(left: &[Tag], right: &[Tag]) -> bool {
    let mut i = 0;
    while i < left.len() && same_tag(left[i], right[i]) {
        i += 1;
    }
    i == left.len()
}

/// Records `stack` at `successor`, or joins it into the recorded stack and
/// re-queues `successor` when the join changes it.
fn visit_lane_successor(
    stacks: &mut [Option<Vec<Tag>>],
    worklist: &mut Vec<usize>,
    pc: usize,
    successor: usize,
    stack: &[Tag],
) -> Result<(), LaneErr> {
    if successor >= stacks.len() {
        return Err(LaneErr {
            pc,
            kind: LaneErrKind::FallsOffEnd,
        });
    }
    let recorded: Option<Vec<Tag>> = stacks[successor].clone();
    match recorded {
        None => {
            stacks[successor] = Some(stack.to_vec());
            worklist.push(successor);
            Ok(())
        }
        Some(existing) => {
            if existing.len() != stack.len() {
                return Err(LaneErr {
                    pc: successor,
                    kind: LaneErrKind::MergeMismatch,
                });
            }
            let mut merged = Vec::new();
            let mut i = 0;
            while i < existing.len() {
                merged.push(tag_join(existing[i], stack[i]));
                i += 1;
            }
            if !same_stack(&merged, &existing) {
                stacks[successor] = Some(merged);
                worklist.push(successor);
            }
            Ok(())
        }
    }
}

/// The tag of every stack cell at the entry of every reachable instruction
/// of one function (`None` = unreachable), as the fixpoint of the transfer
/// under `env`. Unlike `typed_stacks` nothing is rejected during the walk:
/// the tags only grow, and the obligations are judged on the result.
pub fn lane_stacks(
    tables: &LaneTables,
    env: &LaneEnv,
    code: &[CsilInstr],
) -> Result<Vec<Option<Vec<Tag>>>, LaneErr> {
    if code.is_empty() {
        return Err(LaneErr {
            pc: 0,
            kind: LaneErrKind::EmptyBody,
        });
    }
    let mut stacks: Vec<Option<Vec<Tag>>> = Vec::new();
    let mut i = 0;
    while i < code.len() {
        stacks.push(None);
        i += 1;
    }
    stacks[0] = Some(Vec::new());
    let mut worklist = Vec::new();
    worklist.push(0usize);
    while let Some(pc) = worklist.pop() {
        let recorded: Option<Vec<Tag>> = stacks[pc].clone();
        let Some(stack) = recorded else {
            continue;
        };
        let instr = &code[pc];
        let stack = match lane_step(tables, env, instr, stack) {
            Ok(stack) => stack,
            Err(kind) => return Err(LaneErr { pc, kind }),
        };
        if is_return(instr) {
            continue;
        }
        let (first, second) = height_successors(instr, pc);
        visit_lane_successor(&mut stacks, &mut worklist, pc, first, &stack)?;
        if let Some(fallthrough) = second {
            visit_lane_successor(&mut stacks, &mut worklist, pc, fallthrough, &stack)?;
        }
    }
    Ok(stacks)
}

/// Every store a reachable instruction performs, with the tag of the stored
/// value at the fixpoint `stacks`.
pub fn lane_events(
    tables: &LaneTables,
    code: &[CsilInstr],
    stacks: &[Option<Vec<Tag>>],
) -> Result<Vec<LaneStore>, LaneErr> {
    let mut events = Vec::new();
    let mut pc = 0;
    while pc < code.len() {
        let recorded: Option<Vec<Tag>> = stacks[pc].clone();
        if let Some(stack) = recorded {
            let pops = match height_effect(&code[pc], &tables.ta_arity) {
                Ok((pops, _)) => pops as usize,
                Err(_) => {
                    return Err(LaneErr {
                        pc,
                        kind: LaneErrKind::StackEffect,
                    })
                }
            };
            if stack.len() < pops {
                return Err(LaneErr {
                    pc,
                    kind: LaneErrKind::Underflow,
                });
            }
            let popped = popped_operands(&stack, pops);
            lane_stores(&code[pc], pc, &popped, &mut events);
        }
        pc += 1;
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// The checker (glue over the kernel).

/// A declaration that fails its check. It is a validation error; it is never
/// downgraded to "no claim".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaneError {
    /// Obligation 1.
    #[error("object_types[{type_id}]: {declared} field lanes for {field_count} fields")]
    FieldLaneCount {
        type_id: usize,
        declared: usize,
        field_count: u32,
    },
    /// Obligation 1.
    #[error("{place}: lane names object type {type_id}, which does not exist")]
    UnknownObjectType { place: String, type_id: u32 },
    /// The program did not pass `validate()`.
    #[error("function {function} at {pc:04}: not a validated program ({kind:?})")]
    Malformed {
        function: usize,
        pc: usize,
        kind: LaneErrKind,
    },
    /// Obligation 2.
    #[error("function {function} at {pc:04}: slot {slot} declares {declared:?}, stored value is {found:?}")]
    SlotStore {
        function: usize,
        pc: usize,
        slot: u32,
        declared: Lane,
        found: Tag,
    },
    /// Obligation 3.
    #[error("function {function} at {pc:04}: field {field_idx} of type {type_id} declares {declared:?}, stored value is {found:?}")]
    FieldStore {
        function: usize,
        pc: usize,
        type_id: u32,
        field_idx: u32,
        declared: Lane,
        found: Tag,
    },
    /// Obligation 4.
    #[error("function {function} at {pc:04}: field {field_idx} of type {type_id} declares {declared:?}, constructor operand is {found:?}")]
    Constructor {
        function: usize,
        pc: usize,
        type_id: u32,
        field_idx: u32,
        declared: Lane,
        found: Tag,
    },
    /// Obligation 5.
    #[error("function {function} at {pc:04}: SET_FIELD through a receiver with no derived object type, in a program that declares field lanes")]
    UntypedFieldStore { function: usize, pc: usize },
}

/// What [`check_lanes`] established: the tag of every stack cell at the
/// entry of every reachable instruction, per function (`None` =
/// unreachable). The bottom of the stack is index 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFacts {
    pub stacks: Vec<Vec<Option<Vec<Tag>>>>,
}

impl LaneFacts {
    /// The tag of the operand `depth` cells below the top of the stack at the
    /// entry of `pc` in `function` (0 = top).
    pub fn operand(&self, function: usize, pc: usize, depth: usize) -> Option<Tag> {
        let stack = self.stacks.get(function)?.get(pc)?.as_ref()?;
        let index = stack.len().checked_sub(depth + 1)?;
        Some(stack[index])
    }
}

/// Whether the program declares anything at all.
pub fn declares_lanes(program: &CsilProgram) -> bool {
    program.series_slots.iter().any(|slot| slot.lane.is_some())
        || program
            .object_types
            .iter()
            .any(|decl| !decl.field_lanes.is_empty())
}

/// The checker's environment: a declared place is its lane, every other
/// place is `Top`.
pub fn declared_env(program: &CsilProgram) -> LaneEnv {
    LaneEnv {
        slots: program
            .series_slots
            .iter()
            .map(|slot| slot.lane.map_or(Tag::Top(Why::Undeclared), Tag::Is))
            .collect(),
        fields: program
            .object_types
            .iter()
            .map(|decl| {
                (0..decl.field_count as usize)
                    .map(|index| {
                        decl.field_lanes
                            .get(index)
                            .copied()
                            .flatten()
                            .map_or(Tag::Top(Why::Undeclared), Tag::Is)
                    })
                    .collect()
            })
            .collect(),
    }
}

fn shape_errors(program: &CsilProgram) -> Vec<LaneError> {
    let mut errors = Vec::new();
    let type_count = program.object_types.len();
    let check_obj = |place: String, lane: Option<Lane>, errors: &mut Vec<LaneError>| {
        if let Some(Lane::Obj(type_id)) = lane {
            if type_id as usize >= type_count {
                errors.push(LaneError::UnknownObjectType { place, type_id });
            }
        }
    };
    for (index, slot) in program.series_slots.iter().enumerate() {
        check_obj(format!("series_slots[{index}]"), slot.lane, &mut errors);
    }
    for (type_id, decl) in program.object_types.iter().enumerate() {
        if !decl.field_lanes.is_empty() && decl.field_lanes.len() != decl.field_count as usize {
            errors.push(LaneError::FieldLaneCount {
                type_id,
                declared: decl.field_lanes.len(),
                field_count: decl.field_count,
            });
        }
        for (field, lane) in decl.field_lanes.iter().enumerate() {
            check_obj(
                format!("object_types[{type_id}].field_lanes[{field}]"),
                *lane,
                &mut errors,
            );
        }
    }
    errors
}

/// Check every declaration of `program` (obligations 1 to 5 of the module
/// documentation). A program with no declarations is always accepted.
///
/// Soundness statement: if this accepts, then in
/// every VM execution each value read from a declared slot, or
/// from a declared field of an object of the declaring type, is na or
/// carries the declared tag.
pub fn check_lanes(program: &CsilProgram) -> Result<LaneFacts, Vec<LaneError>> {
    let mut errors = shape_errors(program);
    if !errors.is_empty() {
        return Err(errors);
    }
    let tables = lane_tables(program);
    let env = declared_env(program);
    let declares_fields = program
        .object_types
        .iter()
        .any(|decl| decl.field_lanes.iter().any(Option::is_some));
    let declared_field = |type_id: u32, field_idx: u32| -> Option<Lane> {
        program
            .object_types
            .get(type_id as usize)?
            .field_lanes
            .get(field_idx as usize)
            .copied()
            .flatten()
    };
    let mut all_stacks = Vec::new();
    for (function, func) in program.functions.iter().enumerate() {
        let analyzed = lane_stacks(&tables, &env, &func.code).and_then(|stacks| {
            lane_events(&tables, &func.code, &stacks).map(|events| (stacks, events))
        });
        let (stacks, events) = match analyzed {
            Ok(result) => result,
            Err(error) => {
                errors.push(LaneError::Malformed {
                    function,
                    pc: error.pc,
                    kind: error.kind,
                });
                continue;
            }
        };
        for event in events {
            match event.kind {
                StoreKind::Slot { slot } => {
                    let declared = program
                        .series_slots
                        .get(slot as usize)
                        .and_then(|slot| slot.lane);
                    if let Some(declared) = declared {
                        if !tag_fits(event.value, declared) {
                            errors.push(LaneError::SlotStore {
                                function,
                                pc: event.pc,
                                slot,
                                declared,
                                found: event.value,
                            });
                        }
                    }
                }
                StoreKind::Field { type_id, field_idx } => {
                    if let Some(declared) = declared_field(type_id, field_idx) {
                        if !tag_fits(event.value, declared) {
                            errors.push(LaneError::FieldStore {
                                function,
                                pc: event.pc,
                                type_id,
                                field_idx,
                                declared,
                                found: event.value,
                            });
                        }
                    }
                }
                StoreKind::Constructor { type_id, field_idx } => {
                    if let Some(declared) = declared_field(type_id, field_idx) {
                        if !tag_fits(event.value, declared) {
                            errors.push(LaneError::Constructor {
                                function,
                                pc: event.pc,
                                type_id,
                                field_idx,
                                declared,
                                found: event.value,
                            });
                        }
                    }
                }
                StoreKind::Untyped => {
                    if declares_fields {
                        errors.push(LaneError::UntypedFieldStore {
                            function,
                            pc: event.pc,
                        });
                    }
                }
            }
        }
        all_stacks.push(stacks);
    }
    if errors.is_empty() {
        Ok(LaneFacts { stacks: all_stacks })
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Wiring into `validate()`.

/// The lane stage of `validate_program`: nothing for a program that declares
/// nothing, [`check_lanes`] otherwise, with each rejection rendered as a
/// [`ValidationError`]. It runs only on a program every earlier stage
/// accepted.
pub(crate) fn validate_declared(
    program: &CsilProgram,
) -> Result<Option<LaneFacts>, Vec<ValidationError>> {
    if !declares_lanes(program) {
        return Ok(None);
    }
    match check_lanes(program) {
        Ok(facts) => Ok(Some(facts)),
        Err(errors) => Err(errors
            .iter()
            .map(|error| render_lane_error(program, error))
            .collect()),
    }
}

fn render_lane_error(program: &CsilProgram, error: &LaneError) -> ValidationError {
    let name = |function: usize| {
        program
            .functions
            .get(function)
            .map_or_else(|| format!("#{function}"), |func| func.name.clone())
    };
    let at = |function: usize, pc: usize| ValidationError::AtInstruction {
        function: name(function),
        offset: u32::try_from(pc).unwrap_or(u32::MAX),
        message: format!("declared lanes: {error}"),
    };
    match error {
        LaneError::FieldLaneCount { .. } | LaneError::UnknownObjectType { .. } => {
            ValidationError::Metadata(format!("declared lanes: {error}"))
        }
        LaneError::Malformed { function, pc, .. }
        | LaneError::SlotStore { function, pc, .. }
        | LaneError::FieldStore { function, pc, .. }
        | LaneError::Constructor { function, pc, .. }
        | LaneError::UntypedFieldStore { function, pc } => at(*function, *pc),
    }
}

// ---------------------------------------------------------------------------
// Finding declarations: the joint fixpoint.

/// What [`solve_lanes`] decided for one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceOutcome {
    Declared(Lane),
    Undeclared(Why),
}

/// A declaration for every place that has one, with the reason for every
/// place that does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSolution {
    pub slots: Vec<PlaceOutcome>,
    /// By `type_id`, then `field_idx`.
    pub fields: Vec<Vec<PlaceOutcome>>,
    /// A `SET_FIELD` through a receiver with no derived object type exists,
    /// so no field is declared (obligation 5).
    pub fields_dropped: bool,
    /// Whole-program analysis rounds the fixpoint needed.
    pub rounds: u32,
}

fn outcome(tag: Tag) -> PlaceOutcome {
    match tag {
        Tag::Is(lane) => PlaceOutcome::Declared(lane),
        Tag::Top(why) => PlaceOutcome::Undeclared(why),
        Tag::NaOnly => PlaceOutcome::Undeclared(Why::OnlyNa),
        Tag::Bottom => PlaceOutcome::Undeclared(Why::NeverStored),
    }
}

/// One round: every store of the program joined into `env`, and whether any
/// `SET_FIELD` goes through an untyped receiver. `None` when the program is
/// not analyzable (it did not pass `validate()`).
fn join_stores(
    program: &CsilProgram,
    tables: &LaneTables,
    env: &LaneEnv,
    track_fields: bool,
) -> Option<(LaneEnv, bool)> {
    let mut next = env.clone();
    let mut escape = false;
    for func in &program.functions {
        let stacks = lane_stacks(tables, env, &func.code).ok()?;
        for event in lane_events(tables, &func.code, &stacks).ok()? {
            match event.kind {
                StoreKind::Slot { slot } => {
                    if let Some(cell) = next.slots.get_mut(slot as usize) {
                        *cell = tag_join(*cell, event.value);
                    }
                }
                StoreKind::Field { type_id, field_idx }
                | StoreKind::Constructor { type_id, field_idx } => {
                    if track_fields {
                        if let Some(cell) = next
                            .fields
                            .get_mut(type_id as usize)
                            .and_then(|fields| fields.get_mut(field_idx as usize))
                        {
                            *cell = tag_join(*cell, event.value);
                        }
                    }
                }
                StoreKind::Untyped => escape = true,
            }
        }
    }
    Some((next, escape))
}

fn same_env(left: &LaneEnv, right: &LaneEnv) -> bool {
    let same =
        |a: &[Tag], b: &[Tag]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_tag(*x, *y));
    same(&left.slots, &right.slots)
        && left.fields.len() == right.fields.len()
        && left
            .fields
            .iter()
            .zip(&right.fields)
            .all(|(a, b)| same(a, b))
}

/// The declarations the joint fixpoint supports, without changing the
/// program. Any declaration the program already carries is ignored.
///
/// Places and stacks are solved together: every place starts at `Bottom`
/// and grows to the join of its stores, and a read of a place yields the
/// place's current tag. Two repairs then make the result checkable, because
/// the checker reads an undeclared place as `Top`:
///
/// * a place left at `NaOnly` or `Bottom` has no lane to declare, so it is
///   raised to `Top` and the fixpoint is resumed;
/// * a program with an untyped `SET_FIELD` has every field raised to `Top`
///   (obligation 5) and the fixpoint is resumed.
///
/// Places only grow and the lattice is finite, so this terminates. The
/// result is a proposal: [`check_lanes`] on the program with it applied is
/// what establishes it.
pub fn solve_lanes(program: &CsilProgram) -> LaneSolution {
    let tables = lane_tables(program);
    let mut env = LaneEnv {
        slots: vec![Tag::Bottom; program.series_slots.len()],
        fields: program
            .object_types
            .iter()
            .map(|decl| vec![Tag::Bottom; decl.field_count as usize])
            .collect(),
    };
    let mut track_fields = true;
    let mut rounds = 0u32;
    loop {
        let escape = loop {
            rounds += 1;
            let Some((next, escape)) = join_stores(program, &tables, &env, track_fields) else {
                return LaneSolution {
                    slots: vec![PlaceOutcome::Undeclared(Why::Host); env.slots.len()],
                    fields: env
                        .fields
                        .iter()
                        .map(|fields| vec![PlaceOutcome::Undeclared(Why::Host); fields.len()])
                        .collect(),
                    fields_dropped: false,
                    rounds,
                };
            };
            if same_env(&next, &env) {
                break escape;
            }
            env = next;
        };
        if escape && track_fields {
            track_fields = false;
            for cell in env.fields.iter_mut().flatten() {
                *cell = Tag::Top(Why::FieldsDropped);
            }
            continue;
        }
        let mut raised = false;
        for cell in env.slots.iter_mut().chain(env.fields.iter_mut().flatten()) {
            match *cell {
                Tag::Bottom => {
                    *cell = Tag::Top(Why::NeverStored);
                    raised = true;
                }
                Tag::NaOnly => {
                    *cell = Tag::Top(Why::OnlyNa);
                    raised = true;
                }
                _ => {}
            }
        }
        if !raised {
            break;
        }
    }
    LaneSolution {
        slots: env.slots.iter().map(|tag| outcome(*tag)).collect(),
        fields: env
            .fields
            .iter()
            .map(|fields| fields.iter().map(|tag| outcome(*tag)).collect())
            .collect(),
        fields_dropped: !track_fields,
        rounds,
    }
}

/// Write `solution` into the program's declaration tables, replacing any
/// declaration already there. A type none of whose fields is declared keeps
/// an empty `field_lanes`.
pub fn apply_solution(program: &mut CsilProgram, solution: &LaneSolution) {
    let lane = |outcome: &PlaceOutcome| match outcome {
        PlaceOutcome::Declared(lane) => Some(*lane),
        PlaceOutcome::Undeclared(_) => None,
    };
    for (slot, outcome) in program.series_slots.iter_mut().zip(&solution.slots) {
        slot.lane = lane(outcome);
    }
    for (decl, fields) in program.object_types.iter_mut().zip(&solution.fields) {
        let lanes: Vec<Option<Lane>> = fields.iter().map(lane).collect();
        decl.field_lanes = if lanes.iter().any(Option::is_some) {
            lanes
        } else {
            Vec::new()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{
        validate as validate_without_lanes, validate_program as validate, validate_with_lane_facts,
    };

    const HEAD: &str = "//@version=6\nindicator(\"lanes\")\n";

    fn compiled(body: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(&format!("{HEAD}{body}"));
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics), "{body}");
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics), "{body}");
        let program = crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowering");
        validate(&program).expect("front-end output validates");
        program
    }

    /// `body` compiled, with every declaration the joint fixpoint supports.
    fn declared(body: &str) -> CsilProgram {
        let mut program = compiled(body);
        let solution = solve_lanes(&program);
        apply_solution(&mut program, &solution);
        program
    }

    fn slot(program: &CsilProgram, name: &str) -> usize {
        program
            .series_slots
            .iter()
            .position(|slot| slot.name == name)
            .unwrap_or_else(|| panic!("slot {name}"))
    }

    fn type_id(program: &CsilProgram, name: &str) -> usize {
        program
            .object_types
            .iter()
            .position(|decl| decl.name == name)
            .unwrap_or_else(|| panic!("type {name}"))
    }

    const FIELDS: &str =
        "type T\n    int n\n    int m\nvar T st = T.new(3, 0)\nst.m := st.n\nplot(st.m)\n";
    const ARRAY_OF_OBJECTS: &str = "type T\n    int n\nvar arr = array.new<T>()\nif bar_index == 0\n    arr.push(T.new(1))\no = arr.get(0)\no.n := o.n + 1\nplot(o.n)\n";

    #[test]
    fn join_is_the_lattice() {
        let i = Tag::Is(Lane::I64);
        let f = Tag::Is(Lane::F64);
        assert_eq!(tag_join(Tag::Bottom, i), i);
        assert_eq!(tag_join(Tag::NaOnly, i), i);
        assert_eq!(tag_join(i, Tag::NaOnly), i);
        assert_eq!(tag_join(i, i), i);
        assert!(matches!(tag_join(i, f), Tag::Top(Why::Mixed)));
        assert!(matches!(
            tag_join(Tag::Is(Lane::Obj(0)), Tag::Is(Lane::Obj(1))),
            Tag::Top(Why::Mixed)
        ));
        // A handle of a user type and "any other handle" are different lanes.
        assert!(matches!(
            tag_join(Tag::Is(Lane::Obj(0)), Tag::Is(Lane::Ref)),
            Tag::Top(Why::Mixed)
        ));
        assert!(matches!(
            tag_join(Tag::Top(Why::PineIntDiv), i),
            Tag::Top(Why::PineIntDiv)
        ));
        assert_eq!(tag_join(Tag::NaOnly, Tag::NaOnly), Tag::NaOnly);
    }

    #[test]
    fn top_never_satisfies_a_declaration() {
        assert!(tag_fits(Tag::NaOnly, Lane::I64));
        assert!(tag_fits(Tag::Is(Lane::I64), Lane::I64));
        assert!(!tag_fits(Tag::Is(Lane::F64), Lane::I64));
        assert!(!tag_fits(Tag::Top(Why::PineIntDiv), Lane::I64));
        assert!(!tag_fits(Tag::Is(Lane::Obj(1)), Lane::Obj(2)));
        assert!(!tag_fits(Tag::Is(Lane::Obj(1)), Lane::Ref));
    }

    fn push(instr: &CsilInstr, popped: &[Tag]) -> Tag {
        let tables = LaneTables {
            ta_arity: Vec::new(),
            ta_result: Vec::new(),
            inputs: Vec::new(),
        };
        let env = LaneEnv {
            slots: vec![Tag::Is(Lane::I64), Tag::Top(Why::Undeclared), Tag::Bottom],
            fields: vec![vec![Tag::Is(Lane::F64), Tag::Top(Why::Undeclared)]],
        };
        let pushed = lane_pushes(&tables, &env, instr, popped, 1);
        assert_eq!(pushed.len(), 1, "{instr:?}");
        pushed[0]
    }

    #[test]
    fn the_transfer_rows_that_are_not_calls() {
        let int = Tag::Is(Lane::I64);
        let float = Tag::Is(Lane::F64);
        let quotient = Tag::Top(Why::PineIntDiv);
        let is_top = |tag: Tag, why: Why| matches!(tag, Tag::Top(found) if found == why);

        // Integer arithmetic is an integer cell only on integer cells; a
        // float operand (a kept fractional quotient) makes it a float.
        for instr in [
            CsilInstr::IntAdd,
            CsilInstr::IntSub,
            CsilInstr::IntMul,
            CsilInstr::IntRem,
        ] {
            assert_eq!(push(&instr, &[int, int]), int);
            assert_eq!(push(&instr, &[Tag::NaOnly, int]), int);
            assert!(is_top(push(&instr, &[int, quotient]), Why::PineIntDiv));
            assert!(is_top(push(&instr, &[quotient, int]), Why::PineIntDiv));
            assert!(matches!(push(&instr, &[float, int]), Tag::Top(_)));
        }
        assert_eq!(push(&CsilInstr::IntNeg, &[int]), int);
        assert!(is_top(
            push(&CsilInstr::IntNeg, &[quotient]),
            Why::PineIntDiv
        ));
        // `INT_DIV` pops two integer cells or stops the run.
        assert_eq!(push(&CsilInstr::IntDiv, &[quotient, quotient]), int);
        assert!(is_top(
            push(&CsilInstr::PineIntDiv, &[int, int]),
            Why::PineIntDiv
        ));
        assert_eq!(push(&CsilInstr::IntToF64, &[quotient]), float);
        assert_eq!(push(&CsilInstr::PushNa, &[]), Tag::NaOnly);

        // SELECT is the join of its two values; DUP copies.
        assert_eq!(
            push(&CsilInstr::Select, &[int, Tag::NaOnly, Tag::Is(Lane::Bool)]),
            int
        );
        assert!(is_top(
            push(&CsilInstr::Select, &[int, float, Tag::Is(Lane::Bool)]),
            Why::Mixed
        ));
        let tables = LaneTables {
            ta_arity: Vec::new(),
            ta_result: Vec::new(),
            inputs: Vec::new(),
        };
        let env = LaneEnv {
            slots: Vec::new(),
            fields: Vec::new(),
        };
        assert_eq!(
            lane_pushes(&tables, &env, &CsilInstr::Dup, &[int], 2),
            vec![int, int]
        );

        // Places: a declared slot reads its lane, an undeclared one `Top`,
        // and a place nothing stored into reads na.
        let read = |slot| CsilInstr::ReadUserSlot { slot, offset: 0 };
        assert_eq!(push(&read(0), &[]), int);
        assert!(is_top(push(&read(1), &[]), Why::Undeclared));
        assert_eq!(push(&read(2), &[]), Tag::NaOnly);
        let object = Tag::Is(Lane::Obj(0));
        assert_eq!(
            push(&CsilInstr::GetField { field_idx: 0 }, &[object]),
            float
        );
        assert!(is_top(
            push(&CsilInstr::GetField { field_idx: 1 }, &[object]),
            Why::Undeclared
        ));
        assert!(is_top(
            push(&CsilInstr::GetField { field_idx: 0 }, &[Tag::Is(Lane::Ref)]),
            Why::UntypedReceiver
        ));

        // The copy is retagged with the instruction's type whatever the
        // source is, so only a source of that type keeps the claim.
        assert_eq!(
            push(&CsilInstr::CopyObject { type_id: 0 }, &[object]),
            object
        );
        assert!(is_top(
            push(&CsilInstr::CopyObject { type_id: 1 }, &[object]),
            Why::CopySource
        ));
        assert_eq!(
            push(
                &CsilInstr::NewObject {
                    type_id: 0,
                    field_count: 0
                },
                &[]
            ),
            object
        );
        // REGISTER_DRAWING returns the handle it was given.
        let register = CsilInstr::RegisterDrawing {
            kind: pinewasm_hir::DrawKind::all()[0],
        };
        assert_eq!(push(&register, &[object]), object);
        assert_eq!(push(&register, &[Tag::Is(Lane::Ref)]), Tag::Is(Lane::Ref));
        assert!(matches!(push(&register, &[int]), Tag::Top(_)));
    }

    #[test]
    fn nz_min_max_and_abs_follow_their_operands() {
        let int = Tag::Is(Lane::I64);
        let float = Tag::Is(Lane::F64);
        let quotient = Tag::Top(Why::PineIntDiv);
        assert_eq!(pure_tag(PureFunc::Nz, 2, &[int, Tag::NaOnly]), int);
        assert!(matches!(
            pure_tag(PureFunc::Nz, 2, &[int, float]),
            Tag::Top(_)
        ));
        assert_eq!(pure_tag(PureFunc::MathAbs, 1, &[int]), int);
        assert_eq!(pure_tag(PureFunc::MathAbs, 1, &[float]), float);
        assert!(matches!(
            pure_tag(PureFunc::MathSign, 1, &[quotient]),
            Tag::Top(Why::PineIntDiv)
        ));
        assert_eq!(pure_tag(PureFunc::MathMin, 2, &[int, int]), int);
        assert_eq!(pure_tag(PureFunc::MathMax, 3, &[int, float, int]), float);
        assert_eq!(
            pure_tag(PureFunc::MathMax, 2, &[int, Tag::NaOnly]),
            Tag::NaOnly
        );
        assert!(matches!(
            pure_tag(PureFunc::MathMin, 2, &[int, quotient]),
            Tag::Top(Why::PineIntDiv)
        ));
        // An integer-valued result whatever the operand is.
        assert_eq!(pure_tag(PureFunc::MathRound, 1, &[quotient]), int);
        assert_eq!(pure_tag(PureFunc::CastInt, 1, &[quotient]), int);
        assert_eq!(pure_tag(PureFunc::MathRound, 2, &[int, float]), float);
    }

    #[test]
    fn the_joint_fixpoint_declares_what_feeds_itself() {
        let program = declared(
            "var int n = 0\nn := n + 1\nvar float acc = 0.0\nacc := acc + close\n\
             int q = bar_index / 2\nvar float never = na\nplot(n + acc + q + nz(never))\n",
        );
        assert_eq!(
            program.series_slots[slot(&program, "n")].lane,
            Some(Lane::I64)
        );
        assert_eq!(
            program.series_slots[slot(&program, "acc")].lane,
            Some(Lane::F64)
        );
        // A kept integer quotient has no lane, and neither has a slot that
        // only ever holds na.
        assert_eq!(program.series_slots[slot(&program, "q")].lane, None);
        assert_eq!(program.series_slots[slot(&program, "never")].lane, None);
        let solution = solve_lanes(&program);
        assert_eq!(
            solution.slots[slot(&program, "q")],
            PlaceOutcome::Undeclared(Why::PineIntDiv)
        );
        assert_eq!(
            solution.slots[slot(&program, "never")],
            PlaceOutcome::Undeclared(Why::OnlyNa)
        );
        let facts = validate_with_lane_facts(&program).expect("checks");
        assert!(facts.is_some());
    }

    #[test]
    fn object_fields_are_declared_and_typed_through_the_receiver() {
        let program = declared(FIELDS);
        let t = type_id(&program, "T");
        assert_eq!(
            program.object_types[t].field_lanes,
            vec![Some(Lane::I64), Some(Lane::I64)]
        );
        #[allow(clippy::cast_possible_truncation)]
        let object = Lane::Obj(t as u32);
        assert_eq!(
            program.series_slots[slot(&program, "st")].lane,
            Some(object)
        );
        // The built-in layouts nothing constructs stay undeclared.
        assert!(program.object_types[0].field_lanes.is_empty());
        let facts = check_lanes(&program).expect("checks");
        // Every GET_FIELD of the program has a typed receiver.
        for (pc, instr) in program.main().code.iter().enumerate() {
            if matches!(instr, CsilInstr::GetField { .. }) {
                assert_eq!(facts.operand(0, pc, 0), Some(Tag::Is(object)), "pc {pc}");
            }
        }
    }

    #[test]
    fn an_untyped_field_store_leaves_every_field_undeclared() {
        let program = declared(ARRAY_OF_OBJECTS);
        assert!(solve_lanes(&program).fields_dropped);
        assert!(program
            .object_types
            .iter()
            .all(|decl| decl.field_lanes.is_empty()));
        validate(&program).expect("the slots that remain declared check");
    }

    fn rejection(program: &CsilProgram) -> Vec<ValidationError> {
        let errors = validate(program).expect_err("must be rejected");
        // The earlier stages accepted it: every error is a lane error.
        validate_without_lanes(program).expect("only the lane stage rejects");
        for error in &errors {
            assert!(error.to_string().contains("declared lanes: "), "{error}");
        }
        errors
    }

    #[test]
    fn a_declaration_that_fails_is_a_validation_error() {
        let good = declared(FIELDS);
        let t = type_id(&good, "T");
        let st = slot(&good, "st");
        validate(&good).expect("the solved declarations check");

        // Obligation 1: shape.
        let mut bad = good.clone();
        bad.object_types[t].field_lanes.pop();
        assert!(matches!(
            check_lanes(&bad).expect_err("count")[..],
            [LaneError::FieldLaneCount { .. }]
        ));
        assert!(matches!(
            rejection(&bad)[..],
            [ValidationError::Metadata(_)]
        ));
        let mut bad = good.clone();
        bad.series_slots[st].lane = Some(Lane::Obj(999));
        assert!(matches!(
            check_lanes(&bad).expect_err("type")[..],
            [LaneError::UnknownObjectType { .. }]
        ));
        rejection(&bad);

        // Obligation 2: a slot store.
        let mut bad = good.clone();
        bad.series_slots[st].lane = Some(Lane::F64);
        assert!(check_lanes(&bad)
            .expect_err("slot")
            .iter()
            .any(|error| matches!(error, LaneError::SlotStore { .. })));
        assert!(rejection(&bad)
            .iter()
            .all(|error| matches!(error, ValidationError::AtInstruction { .. })));

        // Obligations 3 and 4: a field store and a constructor operand.
        let mut bad = good.clone();
        bad.object_types[t].field_lanes[1] = Some(Lane::F64);
        let errors = check_lanes(&bad).expect_err("field");
        assert!(errors
            .iter()
            .any(|error| matches!(error, LaneError::FieldStore { .. })));
        assert!(errors
            .iter()
            .any(|error| matches!(error, LaneError::Constructor { .. })));
        rejection(&bad);

        // Obligation 5: an untyped store in a program that declares a field.
        let mut bad = declared(ARRAY_OF_OBJECTS);
        let t = type_id(&bad, "T");
        bad.object_types[t].field_lanes = vec![Some(Lane::I64)];
        assert!(check_lanes(&bad)
            .expect_err("escape")
            .iter()
            .any(|error| matches!(error, LaneError::UntypedFieldStore { .. })));
        rejection(&bad);

        // A kept quotient cannot be declared an integer cell.
        let mut bad = compiled("var int q = 0\nq := bar_index / 2\nplot(q + q[1])\n");
        let q = slot(&bad, "q");
        bad.series_slots[q].lane = Some(Lane::I64);
        let errors = check_lanes(&bad).expect_err("quotient");
        assert!(errors.iter().any(|error| matches!(
            error,
            LaneError::SlotStore {
                found: Tag::Top(Why::PineIntDiv),
                ..
            }
        )));
        // Nor a float cell: the checker rejects the quotient's tag for either lane.
        bad.series_slots[q].lane = Some(Lane::F64);
        assert!(check_lanes(&bad).is_err());
    }

    #[test]
    fn without_declarations_the_lane_stage_does_nothing() {
        for body in [
            FIELDS,
            ARRAY_OF_OBJECTS,
            "plot(close)\n",
            "int q = bar_index / 2\nplot(q)\n",
        ] {
            let program = compiled(body);
            assert!(!declares_lanes(&program));
            assert_eq!(validate_with_lane_facts(&program), Ok(None));
            // A program the earlier stages reject is rejected with exactly
            // their errors, in their order.
            let mut broken = program.clone();
            broken.functions[0].code.insert(0, CsilInstr::Pop);
            let span = broken.functions[0].source_map[0];
            broken.functions[0].source_map.insert(0, span);
            let before = validate_without_lanes(&broken).expect_err("underflow");
            assert_eq!(validate(&broken).expect_err("underflow"), before);
            assert_eq!(validate_with_lane_facts(&broken), Err(before));
        }
    }

    #[test]
    fn a_declared_program_rejected_earlier_reports_only_the_earlier_errors() {
        let mut program = declared(FIELDS);
        let st = slot(&program, "st");
        program.series_slots[st].lane = Some(Lane::F64);
        program.functions[0].code.insert(0, CsilInstr::Pop);
        let span = program.functions[0].source_map[0];
        program.functions[0].source_map.insert(0, span);
        let before = validate_without_lanes(&program).expect_err("underflow");
        assert_eq!(validate(&program), Err(before));
    }

    #[test]
    fn declarations_describe_the_code_that_runs_not_the_budget_reference() {
        // A compiler pass rewrites this program, so `main` keeps a copy of the
        // code it had before the pass as a budget reference. That copy is
        // validated without the lane stage.
        let program = declared(
            "f(int a) =>\n    int b = a + 1\n    b * 2\nint x = f(bar_index)\nint y = f(x)\nplot(x + y)\n",
        );
        assert!(program.functions[0].budget_reference.is_some());
        assert!(declares_lanes(&program));
        validate(&program).expect("declared program with a budget reference");
    }
}
