//! Where a Pine Script® int may hold a fraction, and where it may not.
//!
//! In Pine Script, `int / int` is statically int, but an uneven quotient keeps
//! its fraction, and statically-int arithmetic on it carries the fraction
//! further. PineWASM keeps the fraction of a fractional int wherever it is
//! read as a number, and uses the whole part where an integer is required.
//!
//! This module fixes that split once, for every engine, with two tables:
//!
//! * [`classify`] says how an instruction treats a fractional operand as a
//!   number: [`Use::Arithmetic`], [`Use::Forward`], [`Use::Numeric`],
//!   [`Use::Offset`] and [`Use::Render`] take it with its fraction.
//!   [`Use::Other`] is everything not known to. A backend that carries
//!   fractions in float code must treat `Other` as "cannot carry" (fail
//!   closed).
//! * [`reads_whole_part`] is the positive list of operands that can only be
//!   a whole number: an index, a size, a count, a length, a precision, a
//!   table coordinate, a calendar argument, an element of an int container,
//!   and (the [`STORED_READS_WHOLE`] group) an int field, an `array.from`
//!   element and a drawing coordinate.
//!
//! [`truncate_for_integer_consumers`] inserts an explicit `int(...)` after a
//! fractional producer that only operands on the positive list read, so the
//! interpreter and compiled code never meet a fraction there, and both charge
//! the same instructions. An operand on neither list is left alone: it
//! receives the value unchanged. An unlisted integer-only consumer therefore
//! meets a float and raises a type error, never a silently truncated number.
//!
//! The rewrite runs on the stackifier's patched output, before the budget
//! reference is taken and before the optimization passes, so the reference
//! and the optimized code both contain the inserted casts and the validator's
//! "the reference optimizes to this code" check is unaffected. It is the
//! identity on code without `PINE_INT_DIV` ([`leaves_unchanged`]).
//!
//! Design of this module:
//!
//! * [`truncate_for_integer_consumers`] truncates only for the operands that
//!   [`reads_whole_part`] lists. An operand that [`classify`] does not know
//!   (`Use::Other`) is left alone, so it keeps its fraction. `float(x)` is
//!   [`Use::Numeric`] and `log.*` is [`Use::Render`]: both read the fraction.
//! * [`truncate_for_integer_consumers`] rewrites a code vector and its spans
//!   in place and returns an [`Outcome`]. No program exists yet when the
//!   rewrite runs, and it must run before the optimizer.
//! * [`fractional`] and [`Flow::build`] take a `&CsilProgram`;
//!   [`fractional_code`] and [`Flow::build_code`] are the same analyses over
//!   bare code.
//! * [`Use::Stored`] groups the consumers that write a value into an object
//!   field or into an `array.from` element. A match on [`Use`] needs an arm
//!   for it, which a float-carrying backend treats like `Other`.

use crate::flow::Flow;
use crate::{CollectionFunc, CsilInstr, CsilProgram, TaFunc, TaStateDecl};
use pinewasm_hir::{CollectionAbstract, PureFunc, TickerFunc};
use pinewasm_span::Span;
use std::sync::atomic::{AtomicU64, Ordering};

/// How an instruction treats an operand that may be a fractional int.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Int arithmetic: with one fractional operand the result is fractional.
    Arithmetic,
    /// The result takes its value operands' type (`?:`, `nz`, `math.abs`,
    /// `math.min`, `math.max`, `math.sign`). `first` is the first value
    /// operand; the ones before it are not numeric.
    Forward { first: usize },
    /// Reads an int or a float alike and yields something that cannot hold
    /// a fraction: comparisons, the float conversion, the rounding casts,
    /// `na()`, `bool()`.
    Numeric,
    /// A history offset: a fractional operand truncates toward zero.
    Offset,
    /// Formats the value as text, fraction included.
    Render,
    /// Writes the value into an object field (a user-defined type's field or
    /// a drawing's coordinate) or into the elements `array.from` builds. An
    /// int-typed field or element takes the whole part while
    /// [`STORED_READS_WHOLE`] is set. A float-carrying backend treats this
    /// like [`Use::Other`].
    Stored,
    /// Not known to take a fractional int as a number. A float-carrying
    /// backend cannot carry a fraction into it. New opcodes land here. This
    /// class alone does not make the rewrite truncate: see
    /// [`reads_whole_part`].
    Other,
}

/// How `instr` treats fractional-int operands.
pub fn classify(instr: &CsilInstr) -> Use {
    match instr {
        CsilInstr::IntAdd
        | CsilInstr::IntSub
        | CsilInstr::IntMul
        | CsilInstr::IntRem
        | CsilInstr::IntNeg
        | CsilInstr::PineIntDiv => Use::Arithmetic,
        CsilInstr::Select => Use::Forward { first: 1 },
        CsilInstr::CallPure {
            func:
                PureFunc::Nz
                | PureFunc::MathAbs
                | PureFunc::MathMin
                | PureFunc::MathMax
                | PureFunc::MathSign,
            ..
        } => Use::Forward { first: 0 },
        CsilInstr::CallPure {
            func:
                PureFunc::CastInt
                | PureFunc::CastFloat
                | PureFunc::MathFloor
                | PureFunc::MathCeil
                | PureFunc::MathRound
                | PureFunc::Na
                | PureFunc::CastBool,
            argc: 1,
        }
        | CsilInstr::CmpGt
        | CsilInstr::CmpGe
        | CsilInstr::CmpLt
        | CsilInstr::CmpLe
        | CsilInstr::CmpEq
        | CsilInstr::CmpNe
        | CsilInstr::IntToF64 => Use::Numeric,
        CsilInstr::ReadBuiltinDynamic { .. } | CsilInstr::ReadUserSlotDynamic { .. } => Use::Offset,
        CsilInstr::CallPure {
            func: PureFunc::StrTostring | PureFunc::StrFormat,
            ..
        }
        | CsilInstr::Log { .. } => Use::Render,
        CsilInstr::NewObject { .. }
        | CsilInstr::SetField { .. }
        | CsilInstr::CallCollection {
            func: CollectionFunc::ArrayFrom,
            ..
        } => Use::Stored,
        _ => Use::Other,
    }
}

/// Whether the group [`Use::Stored`] reads the whole part: an int field of a
/// user-defined type or a drawing coordinate (`NEW_OBJECT`, `SET_FIELD`) and
/// an int element built by `array.from`.
///
/// Without the whole-part rewrite, the interpreter would store the fraction in
/// these places and read it back. This module stores the whole part in these
/// places. Setting this to `false` restores the fraction for the whole group;
/// nothing else decides it.
pub const STORED_READS_WHOLE: bool = true;

/// Whether operand `operand` of `instr` (0 is the first one pushed, `arity`
/// the number it pops) can only be a whole number, so that a fractional int
/// reaching it is given `int(...)`.
///
/// This is a positive list. An operand that is not on it is left alone. The
/// matches are exhaustive on purpose: a new opcode or pure function does not
/// compile until it has an arm here. A new collection function gets `int`
/// operands (indexes, sizes, counts) truncated from the kinds it declares,
/// and its element operand left alone until it is named.
///
/// A float-typed destination never appears here: the type checker inserts
/// `INT_TO_F64` in front of it, so a statically-int value that reaches an
/// operand directly is being used as an int.
pub fn reads_whole_part(instr: &CsilInstr, operand: usize, arity: usize) -> bool {
    match instr {
        // An int field or a drawing coordinate; a float field is converted
        // before it gets here, and the other field kinds are not numbers.
        CsilInstr::NewObject { .. } => STORED_READS_WHOLE,
        // (handle, value).
        CsilInstr::SetField { .. } => STORED_READS_WHOLE && operand == 1,
        CsilInstr::CallCollection { func, argc } => {
            collection_reads_whole_part(*func, *argc, operand, arity)
        }
        CsilInstr::CallPure { func, .. } => pure_reads_whole_part(*func, operand),
        // (table, column, row, attribute values...): the cell coordinates.
        CsilInstr::SetCell { .. } => operand == 1 || operand == 2,
        // (table, start column, start row, end column, end row).
        CsilInstr::ClearCells | CsilInstr::MergeCells => operand >= 1,
        // A run-time window length.
        CsilInstr::WindowMeanDynamic { .. }
        | CsilInstr::WindowExtremumDynamic { .. }
        | CsilInstr::WindowStatisticDynamic { .. } => true,
        // (source, length): the source is a float.
        CsilInstr::EmaSimpleDynamic { .. } => operand == 1,

        // Take the value as a number, fraction included ([`classify`]).
        CsilInstr::IntAdd
        | CsilInstr::IntSub
        | CsilInstr::IntMul
        | CsilInstr::IntRem
        | CsilInstr::IntNeg
        | CsilInstr::PineIntDiv
        | CsilInstr::Select
        | CsilInstr::CmpGt
        | CsilInstr::CmpGe
        | CsilInstr::CmpLt
        | CsilInstr::CmpLe
        | CsilInstr::CmpEq
        | CsilInstr::CmpNe
        | CsilInstr::IntToF64
        | CsilInstr::Log { .. } => false,
        // A history offset: the interpreter truncates it toward zero itself.
        CsilInstr::ReadBuiltinDynamic { .. } | CsilInstr::ReadUserSlotDynamic { .. } => false,
        // Truncating integer division: never emitted from source, and its
        // operands are whole numbers. Left alone.
        CsilInstr::IntDiv => false,
        // A TA call's operands are sources, and a length is a declaration
        // parameter rather than an operand. Most sources are floats and
        // arrive converted; the int forms the checker picks for an int
        // source (`ta.median`, `ta.mode`, `ta.range`) take it unconverted.
        // Either way a source is a number, not a whole-number setting. Left
        // alone.
        CsilInstr::CallTa { .. } => false,
        // Float, bool, string, color or handle operands.
        CsilInstr::F64Add
        | CsilInstr::F64Sub
        | CsilInstr::F64Mul
        | CsilInstr::F64Div
        | CsilInstr::F64Rem
        | CsilInstr::F64Neg
        | CsilInstr::BoolAnd
        | CsilInstr::BoolOr
        | CsilInstr::BoolNot
        | CsilInstr::StrConcat
        | CsilInstr::PlotF64 { .. }
        | CsilInstr::PlotStyledF64 { .. }
        | CsilInstr::VisualColor { .. }
        | CsilInstr::FillColor { .. }
        | CsilInstr::RuntimeError
        | CsilInstr::FootprintRowByPrice
        | CsilInstr::CallPivotPointLevels { .. }
        | CsilInstr::CopyObject { .. }
        | CsilInstr::GetField { .. }
        | CsilInstr::RegisterDrawing { .. }
        | CsilInstr::DeleteDrawing { .. }
        | CsilInstr::Alert
        | CsilInstr::AlertCondition
        | CsilInstr::JumpIfFalse { .. }
        | CsilInstr::JumpIfExact { .. } => false,
        // Strategy orders: quantities and prices are floats; the few int
        // settings are not decided here. Left alone.
        CsilInstr::Order { .. } => false,
        // Plumbing: these forward a value and are not consumers.
        CsilInstr::Dup | CsilInstr::Pop | CsilInstr::StoreUserSlotCurrent { .. } => false,
        // No operand.
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
        | CsilInstr::WindowMean { .. }
        | CsilInstr::ReadDrawingsAll { .. }
        | CsilInstr::Jump { .. }
        | CsilInstr::JumpIfSlotWritten { .. }
        | CsilInstr::Return => false,
    }
}

/// [`reads_whole_part`] for a collection call: an operand the function
/// declares as an int (an index, a size, a count, a type tag) or as an
/// element of a function on [`elem_reads_whole_part`]'s list. A
/// statically-int value that reaches an element operand directly is going
/// into an int container; for a float container it arrives converted.
/// `array.from` belongs to the [`Use::Stored`] group.
fn collection_reads_whole_part(
    func: CollectionFunc,
    argc: u8,
    operand: usize,
    arity: usize,
) -> bool {
    // `operand_reqs` lists the top of the stack first.
    let reqs = func.operand_reqs(argc);
    let Some(req) = arity
        .checked_sub(operand + 1)
        .and_then(|from_top| reqs.get(from_top))
    else {
        return false;
    };
    match req {
        CollectionAbstract::Int => true,
        CollectionAbstract::Elem => elem_reads_whole_part(func),
        CollectionAbstract::Ref
        | CollectionAbstract::Str
        | CollectionAbstract::Bool
        | CollectionAbstract::Float
        | CollectionAbstract::Void => false,
    }
}

/// Whether a statically-int value passed as this function's element operand
/// is stored in, or looked up in, an int container. A function not named
/// here leaves the value alone; `matrix.sum`/`diff`/`mult` take their scalar
/// as a number and are deliberately absent.
fn elem_reads_whole_part(func: CollectionFunc) -> bool {
    match func {
        CollectionFunc::ArrayFrom => STORED_READS_WHOLE,
        // The initial value of a new container.
        CollectionFunc::ArrayNewFloat
        | CollectionFunc::ArrayNewInt
        | CollectionFunc::ArrayNewBool
        | CollectionFunc::ArrayNewString
        | CollectionFunc::ArrayNewColor
        | CollectionFunc::ArrayNewObject
        | CollectionFunc::MatrixNew
        // A value written into the container.
        | CollectionFunc::ArrayPush
        | CollectionFunc::ArrayUnshift
        | CollectionFunc::ArraySet
        | CollectionFunc::ArrayInsert
        | CollectionFunc::ArrayFill
        | CollectionFunc::MatrixSet
        | CollectionFunc::MatrixFill
        | CollectionFunc::MapPut
        // A value or key looked up in it.
        | CollectionFunc::ArrayIncludes
        | CollectionFunc::ArrayIndexOf
        | CollectionFunc::ArrayLastIndexOf
        | CollectionFunc::ArrayBinarySearch
        | CollectionFunc::ArrayBinarySearchLeftmost
        | CollectionFunc::ArrayBinarySearchRightmost
        | CollectionFunc::MapGet
        | CollectionFunc::MapContains
        | CollectionFunc::MapRemove => true,
        _ => false,
    }
}

/// [`reads_whole_part`] for a `ticker.*` call. A count is whole; a box size
/// or price amount may be a fraction and is formatted into the ticker id as
/// it is.
fn ticker_reads_whole_part(func: TickerFunc, operand: usize) -> bool {
    match func {
        // Operand 1 is a line count. The registry declares it `int`, so the
        // only fraction that reaches it is an `int / int` quotient.
        TickerFunc::Linebreak => operand == 1,
        // Operand 4 is the reversal, a number of boxes, declared `int` like
        // the line count. Operand 3 is a box size or ATR length, int-or-float,
        // and is left alone.
        TickerFunc::Pointfigure => operand == 4,
        // Renko's operand 2 is a box size; Kagi's operand 1 is a price amount.
        TickerFunc::Renko | TickerFunc::Kagi => false,
        // Strings only.
        TickerFunc::New
        | TickerFunc::Modify
        | TickerFunc::Heikinashi
        | TickerFunc::Standard
        | TickerFunc::Inherit => false,
    }
}

/// [`reads_whole_part`] for a pure call.
fn pure_reads_whole_part(func: PureFunc, operand: usize) -> bool {
    match func {
        // Operand 1 is the precision.
        PureFunc::MathRound => operand == 1,
        // Operand 2 is the seed.
        PureFunc::MathRandom => operand == 2,
        // Operands 1 and up are the begin and end positions.
        PureFunc::StrSubstring => operand >= 1,
        // Operand 3 is the occurrence.
        PureFunc::StrReplace => operand == 3,
        // Operand 1 is the repeat count.
        PureFunc::StrRepeat => operand == 1,
        // Operand 0 is a time in epoch milliseconds for the time-formatting
        // and calendar forms.
        PureFunc::StrFormatTime | PureFunc::WeekOfYear | PureFunc::Calendar(_) => operand == 0,
        // Operand 0 is the trade number for the trade accessors.
        PureFunc::StrategyClosedTrade(_) | PureFunc::StrategyOpenTrade(_) => operand == 0,
        // Operand 0 is the number of seconds.
        PureFunc::TimeframeFromSeconds => operand == 0,
        // Operands 3 and 4 are the bars-back offsets.
        PureFunc::TimeOpen | PureFunc::TimeClose => operand == 3 || operand == 4,
        // All six operands are whole numbers.
        PureFunc::Timestamp => true,
        // Operand 0 is the timezone; operands 1 and up are whole numbers.
        PureFunc::TimestampTz => operand >= 1,

        // Take the value as a number, fraction included ([`classify`]).
        PureFunc::Na
        | PureFunc::Nz
        | PureFunc::MathAbs
        | PureFunc::MathSign
        | PureFunc::MathFloor
        | PureFunc::MathCeil
        | PureFunc::MathMin
        | PureFunc::MathMax
        | PureFunc::StrTostring
        | PureFunc::StrFormat
        | PureFunc::CastInt
        | PureFunc::CastFloat
        | PureFunc::CastBool => false,
        // Float parameters, which arrive converted.
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
        | PureFunc::MathRoundToMintick
        | PureFunc::ColorNew
        | PureFunc::ColorRgb
        | PureFunc::ColorFromGradient
        | PureFunc::StrategyConvertToAccount
        | PureFunc::StrategyConvertToSymbol
        | PureFunc::StrategyDefaultEntryQty => false,
        PureFunc::Ticker(func) => ticker_reads_whole_part(func, operand),
        // String, color or symbol parameters only.
        PureFunc::StrTonumber
        | PureFunc::StrLength
        | PureFunc::StrReplaceAll
        | PureFunc::StrContains
        | PureFunc::StrStartswith
        | PureFunc::StrEndswith
        | PureFunc::StrPos
        | PureFunc::StrUpper
        | PureFunc::StrLower
        | PureFunc::StrTrim
        | PureFunc::StrMatch
        | PureFunc::ColorR
        | PureFunc::ColorG
        | PureFunc::ColorB
        | PureFunc::ColorT
        | PureFunc::ColorRFloat
        | PureFunc::ColorGFloat
        | PureFunc::ColorBFloat
        | PureFunc::ColorTFloat
        | PureFunc::TimeframeInSeconds
        | PureFunc::CastColor
        | PureFunc::SyminfoPrefix
        | PureFunc::SyminfoTicker => false,
    }
}

/// The series slot an instruction reads as a value, if any.
fn slot_read(instr: &CsilInstr) -> Option<usize> {
    match instr {
        CsilInstr::ReadUserSlot { slot, .. } | CsilInstr::ReadUserSlotDynamic { slot } => {
            Some(*slot as usize)
        }
        _ => None,
    }
}

/// Which instructions' results, and which slots, may hold a fractional int.
pub struct Fractional {
    pub nodes: Vec<bool>,
    pub slots: Vec<bool>,
}

/// Every value a `PineIntDiv` result can reach while staying statically int:
/// through int arithmetic, the forwarding calls, and series slots.
///
/// `seed` says which quotients start the search (all of them for the front
/// end; a backend that lowers some shapes exactly leaves those out).
pub fn fractional(
    program: &CsilProgram,
    flow: &Flow,
    seed: impl Fn(usize) -> bool,
) -> Option<Fractional> {
    fractional_code(&program.main().code, flow, seed)
}

/// [`fractional`] for code that is not yet part of a program; `flow` must be
/// the flow of `code`.
pub fn fractional_code(
    code: &[CsilInstr],
    flow: &Flow,
    seed: impl Fn(usize) -> bool,
) -> Option<Fractional> {
    propagate(code, flow, seed, None)
}

/// Whether the int form `func` hands a fractional source value back as its
/// result: `ta.valuewhen` and `fixnan` return a captured source value,
/// `ta.mode` one of the window's values and `ta.range` a difference of two,
/// and the interpreter's decoder keeps the fraction for these four.
/// In PineWASM, the `MedianInt` form returns the whole part, so it is not one;
/// no other int TA form returns its source.
pub fn ta_result_carries_fraction(func: TaFunc) -> bool {
    matches!(
        func,
        TaFunc::ValueWhenInt | TaFunc::FixnanInt | TaFunc::ModeInt | TaFunc::RangeInt
    )
}

/// [`fractional_code`], and with `ta_states` also through the int TA forms
/// that return a fractional source value ([`ta_result_carries_fraction`]): a
/// `CALL_TA` of one whose operand may be fractional (as an int or as its
/// `INT_TO_F64` image), and a run-time-length window statistic of one over a
/// slot that may hold such a value. Only the whole-part
/// rewrite follows a fraction through them; [`fractional`] does not, so a
/// backend's view of what it can carry is unchanged.
fn propagate(
    code: &[CsilInstr],
    flow: &Flow,
    seed: impl Fn(usize) -> bool,
    ta_states: Option<&[TaStateDecl]>,
) -> Option<Fractional> {
    let mut nodes = vec![false; code.len()];
    let mut slots = vec![false; flow.slots.len()];
    // The float image of a fractional int: lowering hands a TA source to the
    // engine through `INT_TO_F64`, directly or by way of a hidden slot.
    // Tracked only to recognise such a source; an image is a float, not a
    // fractional int, and is never itself truncated.
    let mut images = vec![false; code.len()];
    let mut image_slots = vec![false; flow.slots.len()];
    loop {
        let mut changed = false;
        for (pc, instr) in code.iter().enumerate() {
            if nodes[pc] || !*flow.reached.get(pc)? {
                continue;
            }
            let operands = flow.operands.get(pc)?;
            let touched = |range: &[crate::flow::Producers]| {
                range
                    .iter()
                    .any(|producers| producers.iter().any(|&p| nodes[p]))
            };
            if ta_states.is_some() && !images[pc] {
                let image = match instr {
                    CsilInstr::IntToF64 => touched(operands),
                    _ => match slot_read(instr) {
                        Some(slot) => *image_slots.get(slot)?,
                        None => false,
                    },
                };
                if image {
                    images[pc] = true;
                    changed = true;
                }
            }
            let source = |range: &[crate::flow::Producers]| {
                range
                    .iter()
                    .any(|producers| producers.iter().any(|&p| nodes[p] || images[p]))
            };
            let carries = match classify(instr) {
                Use::Arithmetic => {
                    (matches!(instr, CsilInstr::PineIntDiv) && seed(pc)) || touched(operands)
                }
                Use::Forward { first } => touched(operands.get(first..)?),
                _ => false,
            } || match slot_read(instr) {
                Some(slot) => *slots.get(slot)?,
                None => false,
            } || match (ta_states, instr) {
                (Some(states), CsilInstr::CallTa { state }) => {
                    states
                        .get(*state as usize)
                        .is_some_and(|decl| ta_result_carries_fraction(decl.func))
                        && source(operands)
                }
                (Some(_), CsilInstr::WindowStatisticDynamic { slot, func }) => {
                    ta_result_carries_fraction(*func)
                        && (*slots.get(*slot as usize)? || *image_slots.get(*slot as usize)?)
                }
                _ => false,
            };
            if carries {
                nodes[pc] = true;
                changed = true;
            }
        }
        for (slot, producers) in flow.slots.iter().enumerate() {
            if !slots[slot] && producers.iter().any(|&p| nodes[p]) {
                slots[slot] = true;
                changed = true;
            }
            if !image_slots[slot] && producers.iter().any(|&p| images[p]) {
                image_slots[slot] = true;
                changed = true;
            }
        }
        if !changed {
            return Some(Fractional { nodes, slots });
        }
    }
}

/// A sufficient condition for [`truncate_for_integer_consumers`] to leave
/// `code` exactly as it is: no `PINE_INT_DIV`, so no value can be a
/// fractional int.
pub fn leaves_unchanged(code: &[CsilInstr]) -> bool {
    !code.iter().any(|i| matches!(i, CsilInstr::PineIntDiv))
}

/// What [`truncate_for_integer_consumers`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing needed rewriting; the code is exactly as it was.
    Unchanged,
    /// This many `int(...)` casts were inserted.
    Rewritten { casts: usize },
    /// The code has a `PINE_INT_DIV`, but the provenance analysis could not
    /// describe it (or the spans do not match the code), so it is left as
    /// lowered: an integer-only consumer may meet a fraction, as it did
    /// before the rewrite existed. The stackifier's output is always
    /// analyzable; tests pin that this does not happen for compiled scripts.
    NotAnalyzed,
}

/// The producers whose result gets `int(...)`: one flag per instruction of
/// `code`. `Ok(None)` when no producer is flagged; `Err(())` when the code
/// has a `PINE_INT_DIV` and the provenance analysis cannot describe it.
///
/// The values considered are every `PINE_INT_DIV` result and what it reaches
/// while statically int, including through the int TA forms that return a
/// fractional source value ([`ta_result_carries_fraction`]).
///
/// A producer is flagged when some whole-part operand ([`reads_whole_part`])
/// reads it and nothing else does (another reader of any kind, or a slot
/// store): truncating it then cannot change any other reading. A value that
/// reaches a whole-part operand through a slot is truncated at the slot
/// read, which is a producer of its own.
#[allow(clippy::result_unit_err)]
pub fn whole_part_producers(
    code: &[CsilInstr],
    ta_states: &[TaStateDecl],
    slot_count: usize,
) -> Result<Option<Vec<bool>>, ()> {
    if leaves_unchanged(code) {
        return Ok(None);
    }
    let flow = Flow::build_code(code, ta_states, slot_count).ok_or(())?;
    let fractional = propagate(code, &flow, |_| true, Some(ta_states)).ok_or(())?;

    let mut wants_whole = vec![false; code.len()];
    let mut keeps_fraction = vec![false; code.len()];
    for (pc, instr) in code.iter().enumerate() {
        if !*flow.reached.get(pc).ok_or(())? {
            continue;
        }
        let operands = flow.operands.get(pc).ok_or(())?;
        for (index, producers) in operands.iter().enumerate() {
            let whole = reads_whole_part(instr, index, operands.len());
            for &producer in producers {
                if !fractional.nodes[producer] {
                    continue;
                }
                if whole {
                    wants_whole[producer] = true;
                } else {
                    keeps_fraction[producer] = true;
                }
            }
        }
    }
    for producers in &flow.slots {
        for &producer in producers {
            keeps_fraction[producer] = true;
        }
    }
    let truncate: Vec<bool> = wants_whole
        .iter()
        .zip(&keeps_fraction)
        .map(|(whole, fraction)| *whole && !*fraction)
        .collect();
    Ok(truncate.contains(&true).then_some(truncate))
}

/// How many times [`truncate_lowered`] met code it could not analyze.
static NOT_ANALYZED: AtomicU64 = AtomicU64::new(0);

/// The number of lowerings in this process whose whole-part rewrite was
/// skipped because the provenance analysis could not describe the code
/// ([`Outcome::NotAnalyzed`]). The stackifier's output is always analyzable,
/// so this stays zero for programs lowered from source; a nonzero count
/// means integer-only consumers in some program were left to meet a
/// fraction.
pub fn not_analyzed_count() -> u64 {
    NOT_ANALYZED.load(Ordering::Relaxed)
}

/// [`truncate_for_integer_consumers`] as lowering applies it: the outcome is
/// returned, and a skipped rewrite is counted in [`not_analyzed_count`]
/// instead of passing unnoticed.
pub(crate) fn truncate_lowered(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    ta_states: &[TaStateDecl],
    slot_count: usize,
) -> Outcome {
    let outcome = truncate_for_integer_consumers(code, spans, ta_states, slot_count);
    if outcome == Outcome::NotAnalyzed {
        NOT_ANALYZED.fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// Give every operand that can only be a whole number the whole part of a
/// fractional int, by inserting `int(...)` after the value's producer
/// ([`whole_part_producers`]). The inserted cast takes its producer's span,
/// and branch targets are relocated.
pub fn truncate_for_integer_consumers(
    code: &mut Vec<CsilInstr>,
    spans: &mut Vec<Span>,
    ta_states: &[TaStateDecl],
    slot_count: usize,
) -> Outcome {
    if leaves_unchanged(code) {
        return Outcome::Unchanged;
    }
    if code.len() != spans.len() {
        return Outcome::NotAnalyzed;
    }
    let truncate = match whole_part_producers(code, ta_states, slot_count) {
        Ok(Some(truncate)) => truncate,
        Ok(None) => return Outcome::Unchanged,
        Err(()) => return Outcome::NotAnalyzed,
    };
    let casts = truncate.iter().filter(|flag| **flag).count();
    let Ok(new_len) = u32::try_from(code.len() + casts) else {
        return Outcome::NotAnalyzed;
    };
    let mut new_code = Vec::with_capacity(new_len as usize);
    let mut new_spans = Vec::with_capacity(new_len as usize);
    let mut pc_map = Vec::with_capacity(code.len() + 1);
    for (pc, instr) in code.iter().enumerate() {
        pc_map.push(new_code.len() as u32);
        new_code.push(instr.clone());
        new_spans.push(spans[pc]);
        if truncate[pc] {
            new_code.push(CsilInstr::CallPure {
                func: PureFunc::CastInt,
                argc: 1,
            });
            new_spans.push(spans[pc]);
        }
    }
    pc_map.push(new_len);
    for instr in &mut new_code {
        if let CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } = instr
        {
            // `Flow::build_code` accepted the code, so every target is in
            // range.
            match pc_map.get(*target as usize) {
                Some(mapped) => *target = *mapped,
                None => return Outcome::NotAnalyzed,
            }
        }
    }
    *code = new_code;
    *spans = new_spans;
    Outcome::Rewritten { casts }
}
