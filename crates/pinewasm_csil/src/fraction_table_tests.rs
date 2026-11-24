//! Every opcode, pure function and collection function against the two
//! tables of `fraction.rs`: [`classify`] and the positive whole-part list
//! [`reads_whole_part`]. The expected answer is written out per variant, and
//! every `match` here is exhaustive, so a new variant does not compile until
//! someone decides how it treats a fractional int.
//!
//! Reading a row: `2:Other[1]` means that with two operands the call is
//! class `Other` and operand 1 (0 is the first pushed) reads the whole part.
//! `[]` means no operand is truncated: the value is left as it is.

use crate::fraction::{classify, reads_whole_part, Use};
use crate::{
    BuiltinSeries, CalendarField, CellAttr, CollectionFunc, CsilInstr, DrawKind, OrderKind,
    PureFunc, TaFunc, TickerFunc, TradeAccessor,
};
use pinewasm_hir::LogLevel;

fn class(instr: &CsilInstr) -> String {
    match classify(instr) {
        Use::Arithmetic => "Arithmetic".to_string(),
        Use::Forward { first } => format!("Forward{first}"),
        Use::Numeric => "Numeric".to_string(),
        Use::Offset => "Offset".to_string(),
        Use::Render => "Render".to_string(),
        Use::Stored => "Stored".to_string(),
        Use::Other => "Other".to_string(),
    }
}

/// `arity:Class[whole operands]` for one instruction popping `arity` values.
fn describe(instr: &CsilInstr, arity: usize) -> String {
    let whole: Vec<String> = (0..arity)
        .filter(|operand| reads_whole_part(instr, *operand, arity))
        .map(|operand| operand.to_string())
        .collect();
    format!("{arity}:{}[{}]", class(instr), whole.join(","))
}

/// The description of a call at every operand count it accepts (at most 7).
fn describe_call(bounds: (u8, u8), call: impl Fn(u8) -> CsilInstr) -> String {
    (bounds.0..=bounds.1.min(7.max(bounds.0)))
        .map(|argc| describe(&call(argc), usize::from(argc)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn every_pure_function_has_a_decision() {
    let mut seen = 0;
    let mut func = next_purefunc(None);
    while let Some(current) = func {
        let actual = describe_call(current.argc_bounds(), |argc| CsilInstr::CallPure {
            func: current,
            argc,
        });
        if std::env::var_os("FRACTION_TABLE_PRINT").is_some() {
            println!("PureFunc\t{current:?}\t{actual}");
        } else {
            assert_eq!(actual, expected_purefunc(current), "{current:?}");
        }
        seen += 1;
        func = next_purefunc(func);
    }
    // Four float accessors are added to the 69 pure-function families.
    // Enumeration and the decision match must still cover every variant.
    assert_eq!(seen, 73);
}

#[test]
fn every_collection_function_has_a_decision() {
    let mut seen = 0;
    let mut func = next_collectionfunc(None);
    while let Some(current) = func {
        let actual = describe_call(current.argc_bounds(), |argc| CsilInstr::CallCollection {
            func: current,
            argc,
        });
        if std::env::var_os("FRACTION_TABLE_PRINT").is_some() {
            println!("CollectionFunc\t{current:?}\t{actual}");
        } else {
            assert_eq!(actual, expected_collectionfunc(current), "{current:?}");
        }
        seen += 1;
        func = next_collectionfunc(func);
    }
    assert_eq!(seen, 113);
}

/// One instruction of every variant that pops at least one value. The
/// variants that pop nothing have no operand to classify; `expected_instr`
/// still names each of them.
fn operand_taking_instructions() -> Vec<CsilInstr> {
    use CsilInstr as I;
    vec![
        I::WindowMeanDynamic { slot: 0 },
        I::WindowExtremumDynamic {
            slot: 0,
            highest: true,
        },
        I::WindowStatisticDynamic {
            slot: 0,
            func: TaFunc::Median,
        },
        I::EmaSimpleDynamic { slot: 0 },
        I::Dup,
        I::Pop,
        I::StoreUserSlotCurrent { slot: 0 },
        I::JumpIfFalse { target: 0 },
        I::JumpIfExact {
            on: true,
            target: 0,
        },
        I::PlotF64 { plot_slot: 0 },
        I::RuntimeError,
        I::PlotStyledF64 {
            plot_slot: 0,
            color_count: 1,
        },
        I::VisualColor {
            visual_slot: 0,
            color_count: 1,
        },
        I::FillColor {
            fill_slot: 0,
            color_count: 1,
        },
        I::ReadBuiltinDynamic {
            source: BuiltinSeries::Close,
        },
        I::ReadUserSlotDynamic { slot: 0 },
        I::IntNeg,
        I::F64Neg,
        I::IntToF64,
        I::BoolNot,
        I::IntAdd,
        I::IntSub,
        I::IntMul,
        I::IntDiv,
        I::PineIntDiv,
        I::IntRem,
        I::F64Add,
        I::F64Sub,
        I::F64Mul,
        I::F64Div,
        I::F64Rem,
        I::CmpGt,
        I::CmpGe,
        I::CmpLt,
        I::CmpLe,
        I::CmpEq,
        I::CmpNe,
        I::BoolAnd,
        I::BoolOr,
        I::FootprintRowByPrice,
        I::StrConcat,
        I::Select,
        I::CallPure {
            func: PureFunc::Na,
            argc: 1,
        },
        I::CallPivotPointLevels { argc: 3 },
        I::CallCollection {
            func: CollectionFunc::ArrayGet,
            argc: 2,
        },
        I::NewObject {
            type_id: 0,
            field_count: 3,
        },
        I::CopyObject { type_id: 0 },
        I::GetField { field_idx: 0 },
        I::SetField { field_idx: 0 },
        I::RegisterDrawing {
            kind: DrawKind::Line,
        },
        I::DeleteDrawing {
            kind: DrawKind::Line,
        },
        I::SetCell {
            attrs: vec![CellAttr::Text, CellAttr::Width],
        },
        I::ClearCells,
        I::MergeCells,
        I::Log {
            level: LogLevel::Info,
            argc: 2,
        },
        I::Alert,
        I::AlertCondition,
        I::Order {
            kind: OrderKind::Entry,
        },
        I::CallTa { state: 0 },
    ]
}

/// The number of `CsilInstr` variants that pop at least one value: the arms
/// of `expected_instr` other than its last one. Raise it with a new arm.
const OPERAND_TAKING_VARIANTS: usize = 59;

/// The expected description of each opcode. Exhaustive: a new opcode needs
/// an arm, and one that pops a value needs a sample above.
fn expected_instr(instr: &CsilInstr) -> &'static str {
    use CsilInstr as I;
    match instr {
        // A run-time window or EMA length is whole.
        I::WindowMeanDynamic { .. }
        | I::WindowExtremumDynamic { .. }
        | I::WindowStatisticDynamic { .. } => "1:Other[0]",
        I::EmaSimpleDynamic { .. } => "2:Other[1]",
        // Plumbing forwards the value; the rewrite never sees these as
        // consumers.
        I::Dup | I::Pop | I::StoreUserSlotCurrent { .. } => "1:Other[]",
        I::JumpIfFalse { .. } | I::JumpIfExact { .. } => "1:Other[]",
        I::PlotF64 { .. } | I::RuntimeError => "1:Other[]",
        I::PlotStyledF64 { .. } => "2:Other[]",
        I::VisualColor { .. } | I::FillColor { .. } => "1:Other[]",
        // A history offset truncates in the interpreter.
        I::ReadBuiltinDynamic { .. } | I::ReadUserSlotDynamic { .. } => "1:Offset[]",
        I::IntNeg => "1:Arithmetic[]",
        I::IntToF64 => "1:Numeric[]",
        I::F64Neg | I::BoolNot => "1:Other[]",
        I::IntAdd | I::IntSub | I::IntMul | I::PineIntDiv | I::IntRem => "2:Arithmetic[]",
        // The truncating integer division is not emitted from source.
        I::IntDiv => "2:Other[]",
        I::F64Add | I::F64Sub | I::F64Mul | I::F64Div | I::F64Rem => "2:Other[]",
        I::CmpGt | I::CmpGe | I::CmpLt | I::CmpLe | I::CmpEq | I::CmpNe => "2:Numeric[]",
        I::BoolAnd | I::BoolOr | I::FootprintRowByPrice | I::StrConcat => "2:Other[]",
        I::Select => "3:Forward1[]",
        // Decided per function in the two tables below.
        I::CallPure { .. } => "1:Numeric[]",
        I::CallCollection { .. } => "2:Other[1]",
        I::CallPivotPointLevels { .. } => "3:Other[]",
        // The stored group: every field of a constructor, the value of a
        // field assignment.
        I::NewObject { .. } => "3:Stored[0,1,2]",
        I::SetField { .. } => "2:Stored[1]",
        I::CopyObject { .. } | I::GetField { .. } => "1:Other[]",
        I::RegisterDrawing { .. } | I::DeleteDrawing { .. } => "1:Other[]",
        // (table, column, row, attribute values...).
        I::SetCell { .. } => "5:Other[1,2]",
        // (table, start column, start row, end column, end row).
        I::ClearCells | I::MergeCells => "5:Other[1,2,3,4]",
        I::Log { .. } => "2:Render[]",
        I::Alert => "2:Other[]",
        I::AlertCondition => "3:Other[]",
        I::Order { .. } => "10:Other[]",
        I::CallTa { .. } => "1:Other[]",
        // No operand.
        I::PushNa
        | I::PushBool(_)
        | I::PushI64(_)
        | I::PushF64(_)
        | I::PushStr(_)
        | I::PushColor { .. }
        | I::ReadBuiltin { .. }
        | I::ReadUserSlot { .. }
        | I::ReadInput { .. }
        | I::ReadSecurity { .. }
        | I::ReadSecurityExpr { .. }
        | I::ReadRequest { .. }
        | I::BuildFootprint { .. }
        | I::ReadSecurityLowerTf { .. }
        | I::ReadPivotLevels { .. }
        | I::ReadTimeTf { .. }
        | I::WindowMean { .. }
        | I::ReadDrawingsAll { .. }
        | I::Jump { .. }
        | I::JumpIfSlotWritten { .. }
        | I::Return => "no operand",
    }
}

#[test]
fn every_opcode_has_a_decision() {
    let samples = operand_taking_instructions();
    let mut variants = Vec::new();
    for instr in &samples {
        let arity = match instr.stack_effect_rule() {
            crate::StackEffectRule::Fixed { pops, .. } => pops as usize,
            crate::StackEffectRule::Payload { pops, .. } => pops,
            // `CallTa`: one source operand stands for any count.
            crate::StackEffectRule::TaState { .. } => 1,
        };
        assert!(arity > 0, "{instr:?} pops nothing");
        assert_eq!(describe(instr, arity), expected_instr(instr), "{instr:?}");
        let variant = std::mem::discriminant(instr);
        assert!(!variants.contains(&variant), "{instr:?} listed twice");
        variants.push(variant);
    }
    assert_eq!(variants.len(), OPERAND_TAKING_VARIANTS);
    // An opcode that pops nothing has nothing to decide.
    for instr in [CsilInstr::PushI64(1), CsilInstr::Return] {
        assert_eq!(expected_instr(&instr), "no operand");
        assert!(!reads_whole_part(&instr, 0, 0));
    }
}

#[test]
fn every_ticker_function_has_a_decision_per_operand() {
    // The numeric ticker parameters are all declared int-or-float, so each
    // is decided by what it means. A count reads the whole part; a price
    // amount (a box size, a kagi reversal) may be a fraction and is
    // formatted into the ticker id as it is.
    let decided = |func: TickerFunc| match func {
        // A line count.
        TickerFunc::Linebreak => "2:Other[1]",
        // The reversal is a number of boxes; `param` is a box size.
        TickerFunc::Pointfigure => "5:Other[4]",
        // A box size.
        TickerFunc::Renko => "3:Other[] 4:Other[] 5:Other[]",
        // A price amount.
        TickerFunc::Kagi => "2:Other[] 3:Other[]",
        // Strings only.
        TickerFunc::New => "2:Other[] 3:Other[] 4:Other[] 5:Other[] 6:Other[]",
        TickerFunc::Modify => "1:Other[] 2:Other[] 3:Other[] 4:Other[] 5:Other[]",
        TickerFunc::Heikinashi => "1:Other[]",
        TickerFunc::Standard => "0:Other[] 1:Other[]",
        TickerFunc::Inherit => "2:Other[]",
    };
    let arities = |func: TickerFunc| match func {
        TickerFunc::Linebreak | TickerFunc::Inherit => 2..=2u8,
        TickerFunc::Pointfigure => 5..=5,
        TickerFunc::Renko => 3..=5,
        TickerFunc::Kagi => 2..=3,
        TickerFunc::New => 2..=6,
        TickerFunc::Modify => 1..=5,
        TickerFunc::Heikinashi => 1..=1,
        TickerFunc::Standard => 0..=1,
    };
    for func in [
        TickerFunc::New,
        TickerFunc::Modify,
        TickerFunc::Heikinashi,
        TickerFunc::Kagi,
        TickerFunc::Linebreak,
        TickerFunc::Pointfigure,
        TickerFunc::Renko,
        TickerFunc::Standard,
        TickerFunc::Inherit,
    ] {
        let actual = arities(func)
            .map(|argc| {
                describe(
                    &CsilInstr::CallPure {
                        func: PureFunc::Ticker(func),
                        argc,
                    },
                    usize::from(argc),
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(actual, decided(func), "{func:?}");
    }
}

#[test]
fn no_operand_read_as_a_number_is_on_the_whole_part_list() {
    // The two tables must not contradict each other: an operand `classify`
    // says is read with its fraction is never truncated.
    let check = |instr: &CsilInstr, arity: usize| {
        let aware_from = match classify(instr) {
            Use::Arithmetic | Use::Numeric | Use::Offset | Use::Render => 0,
            Use::Forward { first } => first,
            Use::Stored | Use::Other => arity,
        };
        for operand in aware_from..arity {
            assert!(
                !reads_whole_part(instr, operand, arity),
                "{instr:?} operand {operand} of {arity}"
            );
        }
    };
    for instr in operand_taking_instructions() {
        for arity in 1..=7 {
            check(&instr, arity);
        }
    }
    let mut func = next_purefunc(None);
    while let Some(current) = func {
        for argc in 1..=7 {
            check(
                &CsilInstr::CallPure {
                    func: current,
                    argc,
                },
                usize::from(argc),
            );
        }
        func = next_purefunc(func);
    }
    let mut func = next_collectionfunc(None);
    while let Some(current) = func {
        for argc in 1..=7 {
            check(
                &CsilInstr::CallCollection {
                    func: current,
                    argc,
                },
                usize::from(argc),
            );
        }
        func = next_collectionfunc(func);
    }
}

fn next_purefunc(prev: Option<PureFunc>) -> Option<PureFunc> {
    Some(match prev {
        None => PureFunc::Na,
        Some(PureFunc::Na) => PureFunc::Nz,
        Some(PureFunc::Nz) => PureFunc::MathAbs,
        Some(PureFunc::MathAbs) => PureFunc::MathSign,
        Some(PureFunc::MathSign) => PureFunc::MathSqrt,
        Some(PureFunc::MathSqrt) => PureFunc::MathPow,
        Some(PureFunc::MathPow) => PureFunc::MathExp,
        Some(PureFunc::MathExp) => PureFunc::MathLog,
        Some(PureFunc::MathLog) => PureFunc::MathLog10,
        Some(PureFunc::MathLog10) => PureFunc::MathSin,
        Some(PureFunc::MathSin) => PureFunc::MathCos,
        Some(PureFunc::MathCos) => PureFunc::MathTan,
        Some(PureFunc::MathTan) => PureFunc::MathAsin,
        Some(PureFunc::MathAsin) => PureFunc::MathAcos,
        Some(PureFunc::MathAcos) => PureFunc::MathAtan,
        Some(PureFunc::MathAtan) => PureFunc::MathFloor,
        Some(PureFunc::MathFloor) => PureFunc::MathCeil,
        Some(PureFunc::MathCeil) => PureFunc::MathRound,
        Some(PureFunc::MathRound) => PureFunc::MathMin,
        Some(PureFunc::MathMin) => PureFunc::MathMax,
        Some(PureFunc::MathMax) => PureFunc::MathAvg,
        Some(PureFunc::MathAvg) => PureFunc::MathToDegrees,
        Some(PureFunc::MathToDegrees) => PureFunc::MathToRadians,
        Some(PureFunc::MathToRadians) => PureFunc::MathRandom,
        Some(PureFunc::MathRandom) => PureFunc::MathRoundToMintick,
        Some(PureFunc::MathRoundToMintick) => PureFunc::StrTostring,
        Some(PureFunc::StrTostring) => PureFunc::StrTonumber,
        Some(PureFunc::StrTonumber) => PureFunc::StrFormat,
        Some(PureFunc::StrFormat) => PureFunc::StrLength,
        Some(PureFunc::StrLength) => PureFunc::StrSubstring,
        Some(PureFunc::StrSubstring) => PureFunc::StrReplace,
        Some(PureFunc::StrReplace) => PureFunc::StrReplaceAll,
        Some(PureFunc::StrReplaceAll) => PureFunc::StrContains,
        Some(PureFunc::StrContains) => PureFunc::StrStartswith,
        Some(PureFunc::StrStartswith) => PureFunc::StrEndswith,
        Some(PureFunc::StrEndswith) => PureFunc::StrPos,
        Some(PureFunc::StrPos) => PureFunc::StrUpper,
        Some(PureFunc::StrUpper) => PureFunc::StrLower,
        Some(PureFunc::StrLower) => PureFunc::StrTrim,
        Some(PureFunc::StrTrim) => PureFunc::StrRepeat,
        Some(PureFunc::StrRepeat) => PureFunc::StrMatch,
        Some(PureFunc::StrMatch) => PureFunc::StrFormatTime,
        Some(PureFunc::StrFormatTime) => PureFunc::ColorNew,
        Some(PureFunc::ColorNew) => PureFunc::ColorRgb,
        Some(PureFunc::ColorRgb) => PureFunc::ColorFromGradient,
        Some(PureFunc::ColorFromGradient) => PureFunc::ColorR,
        Some(PureFunc::ColorR) => PureFunc::ColorG,
        Some(PureFunc::ColorG) => PureFunc::ColorB,
        Some(PureFunc::ColorB) => PureFunc::ColorT,
        Some(PureFunc::ColorT) => PureFunc::ColorRFloat,
        Some(PureFunc::ColorRFloat) => PureFunc::ColorGFloat,
        Some(PureFunc::ColorGFloat) => PureFunc::ColorBFloat,
        Some(PureFunc::ColorBFloat) => PureFunc::ColorTFloat,
        Some(PureFunc::ColorTFloat) => PureFunc::StrategyClosedTrade(TradeAccessor::EntryPrice),
        Some(PureFunc::StrategyClosedTrade(_)) => {
            PureFunc::StrategyOpenTrade(TradeAccessor::EntryPrice)
        }
        Some(PureFunc::StrategyOpenTrade(_)) => PureFunc::StrategyConvertToAccount,
        Some(PureFunc::StrategyConvertToAccount) => PureFunc::StrategyConvertToSymbol,
        Some(PureFunc::StrategyConvertToSymbol) => PureFunc::StrategyDefaultEntryQty,
        Some(PureFunc::StrategyDefaultEntryQty) => PureFunc::TimeframeInSeconds,
        Some(PureFunc::TimeframeInSeconds) => PureFunc::TimeframeFromSeconds,
        Some(PureFunc::TimeframeFromSeconds) => PureFunc::TimeOpen,
        Some(PureFunc::TimeOpen) => PureFunc::TimeClose,
        Some(PureFunc::TimeClose) => PureFunc::Timestamp,
        Some(PureFunc::Timestamp) => PureFunc::TimestampTz,
        Some(PureFunc::TimestampTz) => PureFunc::CastInt,
        Some(PureFunc::CastInt) => PureFunc::CastFloat,
        Some(PureFunc::CastFloat) => PureFunc::CastBool,
        Some(PureFunc::CastBool) => PureFunc::CastColor,
        Some(PureFunc::CastColor) => PureFunc::WeekOfYear,
        Some(PureFunc::WeekOfYear) => PureFunc::Calendar(CalendarField::Year),
        Some(PureFunc::Calendar(_)) => PureFunc::Ticker(TickerFunc::New),
        Some(PureFunc::Ticker(_)) => PureFunc::SyminfoPrefix,
        Some(PureFunc::SyminfoPrefix) => PureFunc::SyminfoTicker,
        Some(PureFunc::SyminfoTicker) => return None,
    })
}

fn next_collectionfunc(prev: Option<CollectionFunc>) -> Option<CollectionFunc> {
    Some(match prev {
        None => CollectionFunc::ArrayNewFloat,
        Some(CollectionFunc::ArrayNewFloat) => CollectionFunc::ArrayNewInt,
        Some(CollectionFunc::ArrayNewInt) => CollectionFunc::ArrayNewBool,
        Some(CollectionFunc::ArrayNewBool) => CollectionFunc::ArrayNewString,
        Some(CollectionFunc::ArrayNewString) => CollectionFunc::ArrayNewColor,
        Some(CollectionFunc::ArrayNewColor) => CollectionFunc::ArrayNewObject,
        Some(CollectionFunc::ArrayNewObject) => CollectionFunc::ArrayFrom,
        Some(CollectionFunc::ArrayFrom) => CollectionFunc::StrSplit,
        Some(CollectionFunc::StrSplit) => CollectionFunc::ArrayPush,
        Some(CollectionFunc::ArrayPush) => CollectionFunc::ArrayPop,
        Some(CollectionFunc::ArrayPop) => CollectionFunc::ArrayGet,
        Some(CollectionFunc::ArrayGet) => CollectionFunc::ArraySet,
        Some(CollectionFunc::ArraySet) => CollectionFunc::ArraySize,
        Some(CollectionFunc::ArraySize) => CollectionFunc::ArrayClear,
        Some(CollectionFunc::ArrayClear) => CollectionFunc::ArrayShift,
        Some(CollectionFunc::ArrayShift) => CollectionFunc::ArrayUnshift,
        Some(CollectionFunc::ArrayUnshift) => CollectionFunc::ArrayInsert,
        Some(CollectionFunc::ArrayInsert) => CollectionFunc::ArrayRemove,
        Some(CollectionFunc::ArrayRemove) => CollectionFunc::ArrayIncludes,
        Some(CollectionFunc::ArrayIncludes) => CollectionFunc::ArrayIndexOf,
        Some(CollectionFunc::ArrayIndexOf) => CollectionFunc::ArrayLastIndexOf,
        Some(CollectionFunc::ArrayLastIndexOf) => CollectionFunc::ArrayReverse,
        Some(CollectionFunc::ArrayReverse) => CollectionFunc::ArrayFirst,
        Some(CollectionFunc::ArrayFirst) => CollectionFunc::ArrayLast,
        Some(CollectionFunc::ArrayLast) => CollectionFunc::ArrayConcat,
        Some(CollectionFunc::ArrayConcat) => CollectionFunc::ArraySlice,
        Some(CollectionFunc::ArraySlice) => CollectionFunc::ArrayCopy,
        Some(CollectionFunc::ArrayCopy) => CollectionFunc::ArrayFill,
        Some(CollectionFunc::ArrayFill) => CollectionFunc::ArrayJoin,
        Some(CollectionFunc::ArrayJoin) => CollectionFunc::ArraySum,
        Some(CollectionFunc::ArraySum) => CollectionFunc::ArrayAvg,
        Some(CollectionFunc::ArrayAvg) => CollectionFunc::ArrayMin,
        Some(CollectionFunc::ArrayMin) => CollectionFunc::ArrayMax,
        Some(CollectionFunc::ArrayMax) => CollectionFunc::ArrayMedian,
        Some(CollectionFunc::ArrayMedian) => CollectionFunc::ArrayRange,
        Some(CollectionFunc::ArrayRange) => CollectionFunc::ArrayMode,
        Some(CollectionFunc::ArrayMode) => CollectionFunc::ArrayVariance,
        Some(CollectionFunc::ArrayVariance) => CollectionFunc::ArrayStdev,
        Some(CollectionFunc::ArrayStdev) => CollectionFunc::ArrayCovariance,
        Some(CollectionFunc::ArrayCovariance) => CollectionFunc::ArrayEvery,
        Some(CollectionFunc::ArrayEvery) => CollectionFunc::ArraySome,
        Some(CollectionFunc::ArraySome) => CollectionFunc::ArrayPercentRank,
        Some(CollectionFunc::ArrayPercentRank) => CollectionFunc::ArrayPercentileNearestRank,
        Some(CollectionFunc::ArrayPercentileNearestRank) => {
            CollectionFunc::ArrayPercentileLinearInterpolation
        }
        Some(CollectionFunc::ArrayPercentileLinearInterpolation) => {
            CollectionFunc::ArrayBinarySearch
        }
        Some(CollectionFunc::ArrayBinarySearch) => CollectionFunc::ArrayBinarySearchLeftmost,
        Some(CollectionFunc::ArrayBinarySearchLeftmost) => {
            CollectionFunc::ArrayBinarySearchRightmost
        }
        Some(CollectionFunc::ArrayBinarySearchRightmost) => CollectionFunc::ArrayAbs,
        Some(CollectionFunc::ArrayAbs) => CollectionFunc::ArrayStandardize,
        Some(CollectionFunc::ArrayStandardize) => CollectionFunc::ArraySort,
        Some(CollectionFunc::ArraySort) => CollectionFunc::ArraySortIndices,
        Some(CollectionFunc::ArraySortIndices) => CollectionFunc::MapNew,
        Some(CollectionFunc::MapNew) => CollectionFunc::MapPut,
        Some(CollectionFunc::MapPut) => CollectionFunc::MapGet,
        Some(CollectionFunc::MapGet) => CollectionFunc::MapContains,
        Some(CollectionFunc::MapContains) => CollectionFunc::MapRemove,
        Some(CollectionFunc::MapRemove) => CollectionFunc::MapSize,
        Some(CollectionFunc::MapSize) => CollectionFunc::MapClear,
        Some(CollectionFunc::MapClear) => CollectionFunc::MapKeys,
        Some(CollectionFunc::MapKeys) => CollectionFunc::MapValues,
        Some(CollectionFunc::MapValues) => CollectionFunc::MapCopy,
        Some(CollectionFunc::MapCopy) => CollectionFunc::MapPutAll,
        Some(CollectionFunc::MapPutAll) => CollectionFunc::MatrixNew,
        Some(CollectionFunc::MatrixNew) => CollectionFunc::MatrixGet,
        Some(CollectionFunc::MatrixGet) => CollectionFunc::MatrixSet,
        Some(CollectionFunc::MatrixSet) => CollectionFunc::MatrixRows,
        Some(CollectionFunc::MatrixRows) => CollectionFunc::MatrixColumns,
        Some(CollectionFunc::MatrixColumns) => CollectionFunc::MatrixElementsCount,
        Some(CollectionFunc::MatrixElementsCount) => CollectionFunc::MatrixFill,
        Some(CollectionFunc::MatrixFill) => CollectionFunc::MatrixCopy,
        Some(CollectionFunc::MatrixCopy) => CollectionFunc::MatrixAddRow,
        Some(CollectionFunc::MatrixAddRow) => CollectionFunc::MatrixAddCol,
        Some(CollectionFunc::MatrixAddCol) => CollectionFunc::MatrixRemoveRow,
        Some(CollectionFunc::MatrixRemoveRow) => CollectionFunc::MatrixRemoveCol,
        Some(CollectionFunc::MatrixRemoveCol) => CollectionFunc::MatrixSwapRows,
        Some(CollectionFunc::MatrixSwapRows) => CollectionFunc::MatrixSwapColumns,
        Some(CollectionFunc::MatrixSwapColumns) => CollectionFunc::MatrixReverse,
        Some(CollectionFunc::MatrixReverse) => CollectionFunc::MatrixTranspose,
        Some(CollectionFunc::MatrixTranspose) => CollectionFunc::MatrixReshape,
        Some(CollectionFunc::MatrixReshape) => CollectionFunc::MatrixSubmatrix,
        Some(CollectionFunc::MatrixSubmatrix) => CollectionFunc::MatrixConcat,
        Some(CollectionFunc::MatrixConcat) => CollectionFunc::MatrixSum,
        Some(CollectionFunc::MatrixSum) => CollectionFunc::MatrixDiff,
        Some(CollectionFunc::MatrixDiff) => CollectionFunc::MatrixMult,
        Some(CollectionFunc::MatrixMult) => CollectionFunc::MatrixDet,
        Some(CollectionFunc::MatrixDet) => CollectionFunc::MatrixInv,
        Some(CollectionFunc::MatrixInv) => CollectionFunc::MatrixTrace,
        Some(CollectionFunc::MatrixTrace) => CollectionFunc::MatrixPow,
        Some(CollectionFunc::MatrixPow) => CollectionFunc::MatrixKron,
        Some(CollectionFunc::MatrixKron) => CollectionFunc::MatrixAvg,
        Some(CollectionFunc::MatrixAvg) => CollectionFunc::MatrixMin,
        Some(CollectionFunc::MatrixMin) => CollectionFunc::MatrixMax,
        Some(CollectionFunc::MatrixMax) => CollectionFunc::MatrixMedian,
        Some(CollectionFunc::MatrixMedian) => CollectionFunc::MatrixMode,
        Some(CollectionFunc::MatrixMode) => CollectionFunc::MatrixIsSquare,
        Some(CollectionFunc::MatrixIsSquare) => CollectionFunc::MatrixIsIdentity,
        Some(CollectionFunc::MatrixIsIdentity) => CollectionFunc::MatrixIsDiagonal,
        Some(CollectionFunc::MatrixIsDiagonal) => CollectionFunc::MatrixIsSymmetric,
        Some(CollectionFunc::MatrixIsSymmetric) => CollectionFunc::MatrixIsAntisymmetric,
        Some(CollectionFunc::MatrixIsAntisymmetric) => CollectionFunc::MatrixIsTriangular,
        Some(CollectionFunc::MatrixIsTriangular) => CollectionFunc::MatrixIsStochastic,
        Some(CollectionFunc::MatrixIsStochastic) => CollectionFunc::MatrixIsBinary,
        Some(CollectionFunc::MatrixIsBinary) => CollectionFunc::MatrixIsZero,
        Some(CollectionFunc::MatrixIsZero) => CollectionFunc::MatrixIsAntidiagonal,
        Some(CollectionFunc::MatrixIsAntidiagonal) => CollectionFunc::MatrixRank,
        Some(CollectionFunc::MatrixRank) => CollectionFunc::MatrixRow,
        Some(CollectionFunc::MatrixRow) => CollectionFunc::MatrixCol,
        Some(CollectionFunc::MatrixCol) => CollectionFunc::MatrixEigenvalues,
        Some(CollectionFunc::MatrixEigenvalues) => CollectionFunc::MatrixEigenvectors,
        Some(CollectionFunc::MatrixEigenvectors) => CollectionFunc::MatrixPinv,
        Some(CollectionFunc::MatrixPinv) => CollectionFunc::MatrixSort,
        Some(CollectionFunc::MatrixSort) => CollectionFunc::InternalDistanceScanEligible,
        Some(CollectionFunc::InternalDistanceScanEligible) => CollectionFunc::InternalDistanceScan,
        Some(CollectionFunc::InternalDistanceScan) => return None,
    })
}

fn expected_purefunc(func: PureFunc) -> &'static str {
    match func {
        PureFunc::Na => "1:Numeric[]",
        PureFunc::Nz => "2:Forward0[]",
        PureFunc::MathAbs => "1:Forward0[]",
        PureFunc::MathSign => "1:Forward0[]",
        PureFunc::MathSqrt => "1:Other[]",
        PureFunc::MathPow => "2:Other[]",
        PureFunc::MathExp => "1:Other[]",
        PureFunc::MathLog => "1:Other[]",
        PureFunc::MathLog10 => "1:Other[]",
        PureFunc::MathSin => "1:Other[]",
        PureFunc::MathCos => "1:Other[]",
        PureFunc::MathTan => "1:Other[]",
        PureFunc::MathAsin => "1:Other[]",
        PureFunc::MathAcos => "1:Other[]",
        PureFunc::MathAtan => "1:Other[]",
        PureFunc::MathFloor => "1:Numeric[]",
        PureFunc::MathCeil => "1:Numeric[]",
        PureFunc::MathRound => "1:Numeric[] 2:Other[1]",
        PureFunc::MathMin => {
            "2:Forward0[] 3:Forward0[] 4:Forward0[] 5:Forward0[] 6:Forward0[] 7:Forward0[]"
        }
        PureFunc::MathMax => {
            "2:Forward0[] 3:Forward0[] 4:Forward0[] 5:Forward0[] 6:Forward0[] 7:Forward0[]"
        }
        PureFunc::MathAvg => "2:Other[] 3:Other[] 4:Other[] 5:Other[] 6:Other[] 7:Other[]",
        PureFunc::MathToDegrees => "1:Other[]",
        PureFunc::MathToRadians => "1:Other[]",
        PureFunc::MathRandom => "2:Other[] 3:Other[2]",
        PureFunc::MathRoundToMintick => "1:Other[]",
        PureFunc::StrTostring => "1:Render[] 2:Render[]",
        PureFunc::StrTonumber => "1:Other[]",
        PureFunc::StrFormat => {
            "1:Render[] 2:Render[] 3:Render[] 4:Render[] 5:Render[] 6:Render[] 7:Render[]"
        }
        PureFunc::StrLength => "1:Other[]",
        PureFunc::StrSubstring => "2:Other[1] 3:Other[1,2]",
        PureFunc::StrReplace => "4:Other[3]",
        PureFunc::StrReplaceAll => "3:Other[]",
        PureFunc::StrContains => "2:Other[]",
        PureFunc::StrStartswith => "2:Other[]",
        PureFunc::StrEndswith => "2:Other[]",
        PureFunc::StrPos => "2:Other[]",
        PureFunc::StrUpper => "1:Other[]",
        PureFunc::StrLower => "1:Other[]",
        PureFunc::StrTrim => "1:Other[]",
        PureFunc::StrRepeat => "2:Other[1] 3:Other[1]",
        PureFunc::StrMatch => "2:Other[]",
        PureFunc::StrFormatTime => "1:Other[0] 2:Other[0] 3:Other[0]",
        PureFunc::ColorNew => "2:Other[]",
        PureFunc::ColorRgb => "4:Other[]",
        PureFunc::ColorFromGradient => "5:Other[]",
        PureFunc::ColorR => "1:Other[]",
        PureFunc::ColorG => "1:Other[]",
        PureFunc::ColorB => "1:Other[]",
        PureFunc::ColorT => "1:Other[]",
        PureFunc::ColorRFloat
        | PureFunc::ColorGFloat
        | PureFunc::ColorBFloat
        | PureFunc::ColorTFloat => "1:Other[]",
        PureFunc::StrategyClosedTrade(_) => "1:Other[0]",
        PureFunc::StrategyOpenTrade(_) => "1:Other[0]",
        PureFunc::StrategyConvertToAccount => "1:Other[]",
        PureFunc::StrategyConvertToSymbol => "1:Other[]",
        PureFunc::StrategyDefaultEntryQty => "1:Other[]",
        PureFunc::TimeframeInSeconds => "1:Other[]",
        PureFunc::TimeframeFromSeconds => "1:Other[0]",
        PureFunc::TimeOpen => "5:Other[3,4]",
        PureFunc::TimeClose => "5:Other[3,4]",
        PureFunc::Timestamp => "6:Other[0,1,2,3,4,5]",
        PureFunc::TimestampTz => "7:Other[1,2,3,4,5,6]",
        PureFunc::CastInt => "1:Numeric[]",
        PureFunc::CastFloat => "1:Numeric[]",
        PureFunc::CastBool => "1:Numeric[]",
        PureFunc::CastColor => "1:Other[]",
        PureFunc::WeekOfYear => "1:Other[0] 2:Other[0]",
        PureFunc::Calendar(_) => "1:Other[0] 2:Other[0]",
        PureFunc::Ticker(_) => "2:Other[] 3:Other[] 4:Other[] 5:Other[] 6:Other[]",
        PureFunc::SyminfoPrefix => "1:Other[]",
        PureFunc::SyminfoTicker => "1:Other[]",
    }
}

fn expected_collectionfunc(func: CollectionFunc) -> &'static str {
    match func {
        CollectionFunc::InternalDistanceScanEligible => "12:Other[10,11]",
        CollectionFunc::InternalDistanceScan => "10:Other[]",
        CollectionFunc::ArrayNewFloat => "2:Other[0,1]",
        CollectionFunc::ArrayNewInt => "2:Other[0,1]",
        CollectionFunc::ArrayNewBool => "2:Other[0,1]",
        CollectionFunc::ArrayNewString => "2:Other[0,1]",
        CollectionFunc::ArrayNewColor => "2:Other[0,1]",
        CollectionFunc::ArrayNewObject => "3:Other[0,1,2]",
        CollectionFunc::ArrayFrom => "1:Stored[0] 2:Stored[0,1] 3:Stored[0,1,2] 4:Stored[0,1,2,3] 5:Stored[0,1,2,3,4] 6:Stored[0,1,2,3,4,5] 7:Stored[0,1,2,3,4,5,6]",
        CollectionFunc::StrSplit => "2:Other[]",
        CollectionFunc::ArrayPush => "2:Other[1]",
        CollectionFunc::ArrayPop => "1:Other[]",
        CollectionFunc::ArrayGet => "2:Other[1]",
        CollectionFunc::ArraySet => "3:Other[1,2]",
        CollectionFunc::ArraySize => "1:Other[]",
        CollectionFunc::ArrayClear => "1:Other[]",
        CollectionFunc::ArrayShift => "1:Other[]",
        CollectionFunc::ArrayUnshift => "2:Other[1]",
        CollectionFunc::ArrayInsert => "3:Other[1,2]",
        CollectionFunc::ArrayRemove => "2:Other[1]",
        CollectionFunc::ArrayIncludes => "2:Other[1]",
        CollectionFunc::ArrayIndexOf => "2:Other[1]",
        CollectionFunc::ArrayLastIndexOf => "2:Other[1]",
        CollectionFunc::ArrayReverse => "1:Other[]",
        CollectionFunc::ArrayFirst => "1:Other[]",
        CollectionFunc::ArrayLast => "1:Other[]",
        CollectionFunc::ArrayConcat => "2:Other[]",
        CollectionFunc::ArraySlice => "3:Other[1,2]",
        CollectionFunc::ArrayCopy => "1:Other[]",
        CollectionFunc::ArrayFill => "2:Other[1] 3:Other[1,2] 4:Other[1,2,3]",
        CollectionFunc::ArrayJoin => "1:Other[] 2:Other[]",
        CollectionFunc::ArraySum => "1:Other[]",
        CollectionFunc::ArrayAvg => "1:Other[]",
        CollectionFunc::ArrayMin => "2:Other[1]",
        CollectionFunc::ArrayMax => "2:Other[1]",
        CollectionFunc::ArrayMedian => "1:Other[]",
        CollectionFunc::ArrayRange => "1:Other[]",
        CollectionFunc::ArrayMode => "1:Other[]",
        CollectionFunc::ArrayVariance => "2:Other[]",
        CollectionFunc::ArrayStdev => "2:Other[]",
        CollectionFunc::ArrayCovariance => "3:Other[]",
        CollectionFunc::ArrayEvery => "1:Other[]",
        CollectionFunc::ArraySome => "1:Other[]",
        CollectionFunc::ArrayPercentRank => "2:Other[1]",
        CollectionFunc::ArrayPercentileNearestRank => "2:Other[]",
        CollectionFunc::ArrayPercentileLinearInterpolation => "2:Other[]",
        CollectionFunc::ArrayBinarySearch => "2:Other[1]",
        CollectionFunc::ArrayBinarySearchLeftmost => "2:Other[1]",
        CollectionFunc::ArrayBinarySearchRightmost => "2:Other[1]",
        CollectionFunc::ArrayAbs => "1:Other[]",
        CollectionFunc::ArrayStandardize => "1:Other[]",
        CollectionFunc::ArraySort => "2:Other[1]",
        CollectionFunc::ArraySortIndices => "2:Other[1]",
        CollectionFunc::MapNew => "2:Other[0,1]",
        CollectionFunc::MapPut => "3:Other[1,2]",
        CollectionFunc::MapGet => "2:Other[1]",
        CollectionFunc::MapContains => "2:Other[1]",
        CollectionFunc::MapRemove => "2:Other[1]",
        CollectionFunc::MapSize => "1:Other[]",
        CollectionFunc::MapClear => "1:Other[]",
        CollectionFunc::MapKeys => "1:Other[]",
        CollectionFunc::MapValues => "1:Other[]",
        CollectionFunc::MapCopy => "1:Other[]",
        CollectionFunc::MapPutAll => "2:Other[]",
        CollectionFunc::MatrixNew => "4:Other[0,1,2,3]",
        CollectionFunc::MatrixGet => "3:Other[1,2]",
        CollectionFunc::MatrixSet => "4:Other[1,2,3]",
        CollectionFunc::MatrixRows => "1:Other[]",
        CollectionFunc::MatrixColumns => "1:Other[]",
        CollectionFunc::MatrixElementsCount => "1:Other[]",
        CollectionFunc::MatrixFill => "6:Other[1,2,3,4,5]",
        CollectionFunc::MatrixCopy => "1:Other[]",
        CollectionFunc::MatrixAddRow => "3:Other[1]",
        CollectionFunc::MatrixAddCol => "3:Other[1]",
        CollectionFunc::MatrixRemoveRow => "2:Other[1]",
        CollectionFunc::MatrixRemoveCol => "2:Other[1]",
        CollectionFunc::MatrixSwapRows => "3:Other[1,2]",
        CollectionFunc::MatrixSwapColumns => "3:Other[1,2]",
        CollectionFunc::MatrixReverse => "1:Other[]",
        CollectionFunc::MatrixTranspose => "1:Other[]",
        CollectionFunc::MatrixReshape => "3:Other[1,2]",
        CollectionFunc::MatrixSubmatrix => "5:Other[1,2,3,4]",
        CollectionFunc::MatrixConcat => "2:Other[]",
        CollectionFunc::MatrixSum => "2:Other[]",
        CollectionFunc::MatrixDiff => "2:Other[]",
        CollectionFunc::MatrixMult => "2:Other[]",
        CollectionFunc::MatrixDet => "1:Other[]",
        CollectionFunc::MatrixInv => "1:Other[]",
        CollectionFunc::MatrixTrace => "1:Other[]",
        CollectionFunc::MatrixPow => "2:Other[1]",
        CollectionFunc::MatrixKron => "2:Other[]",
        CollectionFunc::MatrixAvg => "1:Other[]",
        CollectionFunc::MatrixMin => "1:Other[]",
        CollectionFunc::MatrixMax => "1:Other[]",
        CollectionFunc::MatrixMedian => "1:Other[]",
        CollectionFunc::MatrixMode => "1:Other[]",
        CollectionFunc::MatrixIsSquare => "1:Other[]",
        CollectionFunc::MatrixIsIdentity => "1:Other[]",
        CollectionFunc::MatrixIsDiagonal => "1:Other[]",
        CollectionFunc::MatrixIsSymmetric => "1:Other[]",
        CollectionFunc::MatrixIsAntisymmetric => "1:Other[]",
        CollectionFunc::MatrixIsTriangular => "1:Other[]",
        CollectionFunc::MatrixIsStochastic => "1:Other[]",
        CollectionFunc::MatrixIsBinary => "1:Other[]",
        CollectionFunc::MatrixIsZero => "1:Other[]",
        CollectionFunc::MatrixIsAntidiagonal => "1:Other[]",
        CollectionFunc::MatrixRank => "1:Other[]",
        CollectionFunc::MatrixRow => "2:Other[1]",
        CollectionFunc::MatrixCol => "2:Other[1]",
        CollectionFunc::MatrixEigenvalues => "1:Other[]",
        CollectionFunc::MatrixEigenvectors => "1:Other[]",
        CollectionFunc::MatrixPinv => "1:Other[]",
        CollectionFunc::MatrixSort => "4:Other[1,2,3]",
    }
}
