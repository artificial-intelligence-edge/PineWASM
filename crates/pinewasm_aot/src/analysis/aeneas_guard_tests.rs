//! Differential tests: the closure-free kernel [`int_fact_step`] (plus the
//! classifier [`int_op`]) is checked against a baseline implementation of the
//! integer-minimum range pass, kept in this file.

use super::{
    arith_fact, can_produce_valid_i64_min, guard_bounds, int_fact_step, integer_min_ambiguity,
    integer_min_ambiguity_with_fields, HeapPlan, IntArith, IntFact, IntInputBoundary, IntOp,
    NativeCollOp, VTy,
};
use pinewasm_csil::{CsilInstr, CsilProgram, InputConstraints};
use pinewasm_hir::{BuiltinSeries, InputKind, InputValue, PureFunc, TradeAccessor};
use std::collections::{BTreeMap, BTreeSet};

// ---- baseline implementation: integer-minimum range pass ----

#[derive(Clone, Copy)]
enum OldIntFact {
    NonInt,
    NaOnly,
    Range(i64, i64),
    Unknown,
}

impl OldIntFact {
    fn from_type(ty: Option<&VTy>) -> Self {
        if ty == Some(&VTy::I64) {
            Self::Unknown
        } else {
            Self::NonInt
        }
    }

    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::NaOnly, fact) | (fact, Self::NaOnly) => fact,
            (Self::Range(a, b), Self::Range(c, d)) => Self::Range(a.min(c), b.max(d)),
            (Self::NonInt, Self::NonInt) => Self::NonInt,
            _ => Self::Unknown,
        }
    }
}

/// Prove that reachable integer producers cannot create the sentinel value.
///
/// Non-arithmetic producers are admitted only where their domain is fixed and
/// bounded. Arithmetic may keep native lowering when a straight-line stack
/// interpretation obtains finite operand ranges; control-flow joins and
/// unknown operands conservatively select the VM tier.
fn old_integer_min_ambiguity(program: &CsilProgram, results: &[Vec<VTy>]) -> Option<usize> {
    let code = &program.main().code;
    for (pc, (instr, tys)) in code.iter().zip(results).enumerate() {
        if can_produce_valid_i64_min(
            program,
            pc,
            instr,
            tys,
            super::IntInputBoundary::HostConstraints,
            None,
        ) {
            return Some(pc);
        }
    }

    let has_control_flow = code.iter().any(|instr| {
        matches!(
            instr,
            CsilInstr::Jump { .. }
                | CsilInstr::JumpIfFalse { .. }
                | CsilInstr::JumpIfExact { .. }
                | CsilInstr::JumpIfSlotWritten { .. }
        )
    });
    if has_control_flow {
        return code.iter().zip(results).position(|(instr, tys)| {
            tys.contains(&VTy::I64)
                && matches!(
                    instr,
                    CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul
                )
        });
    }

    let mut stack = Vec::<OldIntFact>::new();
    for (pc, instr) in code.iter().enumerate() {
        // Resolve through CSIL's shared stack contract. TA calls take their
        // arity from the declaration table; an unresolvable effect (such as a
        // malformed TA state index) fails closed to the VM tier.
        let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
            return Some(pc);
        };
        let (pops, pushes) = (effect.pops as usize, effect.pushes as usize);
        if stack.len() < pops {
            return Some(pc);
        }

        let mut popped = Vec::with_capacity(pops);
        for _ in 0..pops {
            popped.push(stack.pop().expect("stack height checked above"));
        }
        let tys = results.get(pc).map(Vec::as_slice).unwrap_or_default();
        let push_fact = |index: usize| OldIntFact::from_type(tys.get(index));

        match instr {
            CsilInstr::PushNa => {
                stack.push(if tys.first() == Some(&VTy::I64) {
                    OldIntFact::NaOnly
                } else {
                    OldIntFact::NonInt
                });
            }
            CsilInstr::PushI64(value) => stack.push(OldIntFact::Range(*value, *value)),
            CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
                let fact = match source {
                    BuiltinSeries::BarIndex => OldIntFact::Range(0, i32::MAX as i64),
                    BuiltinSeries::LastBarIndex => OldIntFact::Range(-1, i32::MAX as i64),
                    BuiltinSeries::DayOfWeek => OldIntFact::Range(1, 7),
                    BuiltinSeries::DayOfMonth => OldIntFact::Range(1, 31),
                    BuiltinSeries::Month => OldIntFact::Range(1, 12),
                    // Every accepted timestamp is an i64 epoch-millisecond
                    // value; its civil year is bounded well inside this
                    // deliberately wide interval.
                    BuiltinSeries::Year => OldIntFact::Range(-1_000_000, 1_000_000),
                    BuiltinSeries::Hour => OldIntFact::Range(0, 23),
                    BuiltinSeries::Minute | BuiltinSeries::Second => OldIntFact::Range(0, 59),
                    BuiltinSeries::WeekOfYear => OldIntFact::Range(1, 53),
                    _ => OldIntFact::from_type(tys.first()),
                };
                stack.push(fact);
            }
            CsilInstr::Dup => {
                let top = popped.first().copied().unwrap_or(OldIntFact::Unknown);
                stack.push(top);
                stack.push(top);
            }
            CsilInstr::Select => {
                let rhs = popped.get(1).copied().unwrap_or(OldIntFact::Unknown);
                let lhs = popped.get(2).copied().unwrap_or(OldIntFact::Unknown);
                stack.push(lhs.combine(rhs));
            }
            CsilInstr::CallPure {
                func: PureFunc::StrLength | PureFunc::StrPos,
                ..
            } => stack.push(OldIntFact::Range(-1, i64::MAX)),
            CsilInstr::CallCollection {
                func:
                    pinewasm_hir::CollectionFunc::ArraySize
                    | pinewasm_hir::CollectionFunc::MapSize
                    | pinewasm_hir::CollectionFunc::MatrixRows
                    | pinewasm_hir::CollectionFunc::MatrixColumns,
                ..
            } => stack.push(OldIntFact::Range(0, i64::MAX)),
            CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul => {
                let lhs = popped.get(1).copied().unwrap_or(OldIntFact::Unknown);
                let rhs = popped.first().copied().unwrap_or(OldIntFact::Unknown);
                let fact = match (lhs, rhs) {
                    (OldIntFact::NaOnly, _) | (_, OldIntFact::NaOnly) => OldIntFact::NaOnly,
                    (OldIntFact::Range(a, b), OldIntFact::Range(c, d)) => {
                        let bounds = match instr {
                            CsilInstr::IntAdd => {
                                [i128::from(a) + i128::from(c), i128::from(b) + i128::from(d)]
                            }
                            CsilInstr::IntSub => {
                                [i128::from(a) - i128::from(d), i128::from(b) - i128::from(c)]
                            }
                            CsilInstr::IntMul => {
                                let products = [
                                    i128::from(a) * i128::from(c),
                                    i128::from(a) * i128::from(d),
                                    i128::from(b) * i128::from(c),
                                    i128::from(b) * i128::from(d),
                                ];
                                [
                                    *products.iter().min().unwrap(),
                                    *products.iter().max().unwrap(),
                                ]
                            }
                            _ => unreachable!("the match arm contains integer arithmetic"),
                        };
                        let min = i128::from(i64::MIN);
                        let max = i128::from(i64::MAX);
                        if bounds[0] <= min && bounds[1] >= min {
                            return Some(pc);
                        }
                        if bounds[1] < min || bounds[0] > max {
                            OldIntFact::NaOnly
                        } else if bounds[0] < min {
                            // The interval straddles the minimum, so a
                            // valid minimum is possible even if other
                            // combinations overflow.
                            return Some(pc);
                        } else {
                            OldIntFact::Range(
                                bounds[0].max(min + 1) as i64,
                                bounds[1].min(max) as i64,
                            )
                        }
                    }
                    _ => return Some(pc),
                };
                stack.push(fact);
            }
            CsilInstr::IntNeg => {
                let operand = popped.first().copied().unwrap_or(OldIntFact::Unknown);
                stack.push(match operand {
                    OldIntFact::NaOnly => OldIntFact::NaOnly,
                    OldIntFact::Range(min, max) => {
                        let output_min = -i128::from(max);
                        let output_max = -i128::from(min);
                        if output_min <= i128::from(i64::MIN) {
                            return Some(pc);
                        }
                        OldIntFact::Range(output_min as i64, output_max as i64)
                    }
                    _ => OldIntFact::Unknown,
                });
            }
            CsilInstr::IntDiv | CsilInstr::IntRem => {
                let lhs = popped.get(1).copied().unwrap_or(OldIntFact::Unknown);
                let rhs = popped.first().copied().unwrap_or(OldIntFact::Unknown);
                stack.push(
                    if matches!(lhs, OldIntFact::NaOnly) || matches!(rhs, OldIntFact::NaOnly) {
                        OldIntFact::NaOnly
                    } else {
                        // Checked division cannot create MIN unless the input is
                        // already MIN (which the producer gate rejects); the
                        // exact quotient/remainder range is not needed unless a
                        // later add/subtract/multiply tries to use it.
                        OldIntFact::Unknown
                    },
                );
            }
            _ => {
                for index in 0..pushes {
                    stack.push(push_fact(index));
                }
            }
        }
    }
    None
}

/// Baseline producer-gate helper written with plain slice `contains`/`find`/`all`
/// operations.
fn old_can_produce_valid_i64_min(
    program: &CsilProgram,
    pc: usize,
    instr: &CsilInstr,
    results: &[VTy],
    inputs: IntInputBoundary,
    heap: Option<&HeapPlan>,
) -> bool {
    if !results.contains(&VTy::I64) {
        return false;
    }
    match instr {
        CsilInstr::PushNa
        | CsilInstr::ReadUserSlot { .. }
        | CsilInstr::ReadUserSlotDynamic { .. }
        | CsilInstr::Dup
        | CsilInstr::Select
        | CsilInstr::IntNeg
        | CsilInstr::IntDiv
        | CsilInstr::IntRem => false,
        CsilInstr::CallPure {
            func: PureFunc::MathRound,
            argc: 1,
        } if pc > 0 && matches!(program.main().code.get(pc - 1), Some(CsilInstr::PineIntDiv)) => {
            false
        }
        CsilInstr::CallPure {
            func:
                // Selection cannot create a new integer; operand producers
                // remain subject to this same MIN-sentinel safety gate.
                PureFunc::Nz
                | PureFunc::MathAbs
                | PureFunc::MathSign
                | PureFunc::MathMin
                | PureFunc::MathMax
                | PureFunc::StrLength
                | PureFunc::StrPos
                | PureFunc::ColorR
                | PureFunc::ColorG
                | PureFunc::ColorB
                | PureFunc::ColorT,
            ..
        } => false,
        // Trade bar indices are u32 bar positions widened to i64; the blob
        // returns MIN only as its na. Trade times stay conservative, like the
        // `time` builtin.
        CsilInstr::CallPure {
            func:
                PureFunc::StrategyOpenTrade(field) | PureFunc::StrategyClosedTrade(field),
            ..
        } if matches!(
            field,
            TradeAccessor::EntryBarIndex | TradeAccessor::ExitBarIndex
        ) =>
        {
            false
        }
        CsilInstr::CallTa { state } => program.ta_states.get(*state as usize).is_none_or(|decl| {
            !matches!(
                decl.func,
                pinewasm_ta::TaFunc::HighestBars
                    | pinewasm_ta::TaFunc::LowestBars
                    | pinewasm_ta::TaFunc::BarsSince
                    | pinewasm_ta::TaFunc::ValueWhenInt
            )
        }),
        CsilInstr::ReadInput { input } => {
            let Some(decl) = program.inputs.iter().find(|decl| decl.id == *input) else {
                return true;
            };
            match decl.kind {
                InputKind::Enum => false,
                // The standalone runtime rejects a MIN int/time parameter
                // before `run()`, so only the compiled-in default can reach
                // the read.
                InputKind::Int | InputKind::Time
                    if inputs == IntInputBoundary::RejectsSentinel =>
                {
                    matches!(decl.defval, InputValue::Int(value) if value == i64::MIN)
                }
                InputKind::Int | InputKind::Time => {
                    let min_excludes_sentinel = matches!(
                        decl.constraints.min,
                        Some(InputValue::Int(min)) if min > i64::MIN
                    );
                    let options_exclude_sentinel = !decl.constraints.options.is_empty()
                        && decl.constraints.options.iter().all(|option| {
                            !matches!(option, InputValue::Int(value) if *value == i64::MIN)
                        });
                    !(min_excludes_sentinel || options_exclude_sentinel)
                }
                _ => false,
            }
        }
        CsilInstr::CallCollection {
            func:
                pinewasm_hir::CollectionFunc::ArraySize
                | pinewasm_hir::CollectionFunc::MapSize
                | pinewasm_hir::CollectionFunc::MatrixRows
                | pinewasm_hir::CollectionFunc::MatrixColumns,
            ..
        } => false,
        // Array sites that lower to the native heap. Native arrays
        // are written only from CSIL operands: the fill of `NewArray`, the
        // arguments of `FromArgs`, and the values of `Push` and `Set`. The
        // host never writes native heap memory, so every stored integer came
        // from a producer this gate already checked, and an element read
        // (`Get`, `Shift`, `PopLast`, `First`, `Last`) returns one of those
        // values or an `na` fill. The void mutators (`Push`, `Set`, `Clear`)
        // push only the lane's na constant as their placeholder, which the
        // element lane types I64 for an `array<int>`. `host.vm` collection
        // sites stay refused: VM builtins (lower-timeframe requests, matrix
        // results) fill elements with no CSIL producer to gate.
        CsilInstr::CallCollection { .. }
            if heap
                .and_then(|plan| plan.coll.get(&pc))
                .is_some_and(|(op, _)| {
                    matches!(
                        op,
                        NativeCollOp::Get
                            | NativeCollOp::Shift
                            | NativeCollOp::PopLast
                            | NativeCollOp::First
                            | NativeCollOp::Last
                            | NativeCollOp::Push
                            | NativeCollOp::Set
                            | NativeCollOp::Clear
                    )
                }) =>
        {
            false
        }
        // A field read, like a slot read, only returns a value stored earlier.
        // Every integer that reaches an object field is a CSIL operand of
        // `NewObject` (explicit arguments, and UDT/drawing defaults, which
        // semantics materializes as ordinary const pushes) or `SetField`, so
        // its producer already passed this gate; `CopyObject` copies such
        // cells verbatim. Omitted and `na` fields are `PushNa`, the sentinel
        // itself, so they read back as `na`. The VM's own objects
        // (footprints and their rows) carry only float and bool fields, and
        // the `host.vm` boundary refuses an `Int(i64::MIN)` it would encode.
        CsilInstr::GetField { .. } => false,
        CsilInstr::PushI64(value) => *value == i64::MIN,
        CsilInstr::ReadBuiltin { source, .. } | CsilInstr::ReadBuiltinDynamic { source } => {
            !matches!(
                source,
                BuiltinSeries::BarIndex
                    | BuiltinSeries::LastBarIndex
                    | BuiltinSeries::DayOfWeek
                    | BuiltinSeries::DayOfMonth
                    | BuiltinSeries::Month
                    | BuiltinSeries::Year
                    | BuiltinSeries::Hour
                    | BuiltinSeries::Minute
                    | BuiltinSeries::Second
                    | BuiltinSeries::WeekOfYear
                    // Broker trade counts and the first retained index are
                    // non-negative; MIN is only the snapshot read's na.
                    | BuiltinSeries::StrategyWinTrades
                    | BuiltinSeries::StrategyLossTrades
                    | BuiltinSeries::StrategyEvenTrades
                    | BuiltinSeries::StrategyClosedTrades
                    | BuiltinSeries::StrategyOpenTrades
                    | BuiltinSeries::StrategyClosedTradesFirstIndex
            )
        }
        // The range pass below admits these only when it can prove their
        // reachable operands cannot produce MIN.
        CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul => false,
        // If the inputs cannot be MIN, the exact integer quotient cannot be
        // MIN either (MIN / 1 is the only possible such quotient).
        CsilInstr::PineIntDiv => false,
        // All other integer-producing instructions are not proven to exclude
        // MIN: user-controlled inputs, float-to-int conversions, host-backed
        // values, collection elements, and TA outputs land here.
        _ => true,
    }
}

/// Baseline copy of the `integer_min_ambiguity_with_fields` orchestration, kept
/// local to tests so the production helper boundaries cannot drift together
/// with the expected result.
fn old_integer_min_ambiguity_with_fields(
    program: &CsilProgram,
    results: &[Vec<VTy>],
    inputs: IntInputBoundary,
    heap: Option<&super::HeapPlan>,
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
) -> Option<usize> {
    let code = &program.main().code;
    for (pc, (instr, tys)) in code.iter().zip(results).enumerate() {
        if !identities.contains(&pc)
            && !fields.contains_key(&pc)
            && old_can_produce_valid_i64_min(program, pc, instr, tys, inputs, heap)
        {
            return Some(pc);
        }
    }

    let mut leader = vec![false; code.len()];
    for (pc, instr) in code.iter().enumerate() {
        let target = match instr {
            CsilInstr::Jump { target }
            | CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => *target as usize,
            _ => continue,
        };
        if let Some(slot) = leader.get_mut(target) {
            *slot = true;
        }
        if let Some(slot) = leader.get_mut(pc + 1) {
            *slot = true;
        }
    }

    let mut stack = Vec::<IntFact>::new();
    let mut after_leader = false;
    for (pc, instr) in code.iter().enumerate() {
        if leader[pc] {
            stack.clear();
            after_leader = true;
        }
        let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
            return Some(pc);
        };
        let tys = results.get(pc).map(Vec::as_slice).unwrap_or_default();
        let pops = effect.pops as usize;
        if after_leader && stack.len() < pops {
            let missing = pops - stack.len();
            stack.splice(0..0, std::iter::repeat_n(IntFact::Unknown, missing));
        }
        if identities.contains(&pc) {
            if effect.pops != 1 || effect.pushes != 1 {
                return Some(pc);
            }
            let Some(fact) = stack.pop() else {
                return Some(pc);
            };
            stack.push(fact);
            continue;
        }
        if int_fact_step(
            &mut stack,
            match fields.get(&pc) {
                Some(IntFact::Range(lo, hi)) => IntOp::PushRange(*lo, *hi),
                Some(IntFact::NaOnly) => IntOp::PushNa,
                _ => super::int_op(instr),
            },
            effect.pops as usize,
            effect.pushes as usize,
            tys,
        ) {
            return Some(pc);
        }
    }
    None
}

// ---- generators ----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const INTERESTING: [i64; 14] = [
    i64::MIN,
    i64::MIN + 1,
    i64::MIN + 2,
    -4_611_686_018_427_387_904,
    -3_037_000_500,
    -2,
    -1,
    0,
    1,
    2,
    3_037_000_500,
    4_611_686_018_427_387_904,
    i64::MAX - 1,
    i64::MAX,
];

fn any_i64(rng: &mut Rng) -> i64 {
    if rng.below(3) == 0 {
        rng.next() as i64
    } else {
        INTERESTING[rng.below(INTERESTING.len() as u64) as usize]
    }
}

const SOURCES: [BuiltinSeries; 13] = [
    BuiltinSeries::BarIndex,
    BuiltinSeries::LastBarIndex,
    BuiltinSeries::DayOfWeek,
    BuiltinSeries::DayOfMonth,
    BuiltinSeries::Month,
    BuiltinSeries::Year,
    BuiltinSeries::Hour,
    BuiltinSeries::Minute,
    BuiltinSeries::Second,
    BuiltinSeries::WeekOfYear,
    BuiltinSeries::Close,
    BuiltinSeries::Time,
    BuiltinSeries::Volume,
];

fn any_instr(rng: &mut Rng, allow_jump: bool) -> CsilInstr {
    let source = SOURCES[rng.below(SOURCES.len() as u64) as usize];
    match rng.below(if allow_jump { 25 } else { 24 }) {
        0 => CsilInstr::PushNa,
        1..=3 => CsilInstr::PushI64(any_i64(rng)),
        4 => CsilInstr::ReadBuiltin { source, offset: 0 },
        5 => CsilInstr::ReadBuiltinDynamic { source },
        6 => CsilInstr::Dup,
        7 => CsilInstr::Pop,
        8 => CsilInstr::Select,
        9 => CsilInstr::CallPure {
            func: PureFunc::StrLength,
            argc: 1,
        },
        10 => CsilInstr::CallPure {
            func: PureFunc::StrPos,
            argc: 2,
        },
        11 => CsilInstr::CallCollection {
            func: pinewasm_hir::CollectionFunc::ArraySize,
            argc: 1,
        },
        12 | 13 => CsilInstr::IntAdd,
        14 | 15 => CsilInstr::IntSub,
        16 | 17 => CsilInstr::IntMul,
        18 => CsilInstr::IntNeg,
        19 => CsilInstr::IntDiv,
        20 => CsilInstr::IntRem,
        21 => CsilInstr::PushBool(true),
        22 => CsilInstr::CallTa {
            state: rng.below(3) as u32,
        },
        23 => CsilInstr::CmpGt,
        _ => CsilInstr::Jump { target: 0 },
    }
}

fn any_tys(rng: &mut Rng, instr: &CsilInstr) -> Vec<VTy> {
    // Mostly the natural lane, sometimes arbitrary, to exercise every branch.
    let natural = match instr {
        CsilInstr::PushBool(_) | CsilInstr::CmpGt => VTy::Bool3,
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Close | BuiltinSeries::Volume,
            ..
        } => VTy::F64,
        _ => VTy::I64,
    };
    let len = match rng.below(8) {
        0 => 0,
        1 => 2,
        _ => 1,
    };
    (0..len)
        .map(|_| match rng.below(6) {
            0 => VTy::F64,
            1 => VTy::Bool3,
            _ => natural,
        })
        .collect()
}

fn base_program() -> CsilProgram {
    let result = pinewasm_compiler::compile(
        "indicator('guard')\nplot(close)\n",
        pinewasm_compiler::CompileOptions::default(),
    );
    result.program.expect("fixture compiles")
}

// ---- tests ----

/// Random straight-line and branching programs. Straight-line programs get
/// exactly the baseline answer (including the failing pc); branching
/// programs still run through the pass so both outcomes stay exercised.
#[test]
fn integer_min_ambiguity_matches_baseline() {
    let mut program = base_program();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut disagreements = 0;
    let mut none_seen = 0;
    let mut some_seen = 0;
    for case in 0..60_000 {
        let len = 1 + rng.below(12) as usize;
        let allow_jump = case % 16 == 0;
        let code: Vec<CsilInstr> = (0..len).map(|_| any_instr(&mut rng, allow_jump)).collect();
        let mut results: Vec<Vec<VTy>> = code.iter().map(|i| any_tys(&mut rng, i)).collect();
        if rng.below(10) == 0 {
            results.truncate(rng.below(len as u64) as usize);
        }
        program.functions[0].code = code;
        let old = old_integer_min_ambiguity(&program, &results);
        let new = integer_min_ambiguity(
            &program,
            &results,
            super::IntInputBoundary::HostConstraints,
            None,
        );
        let branching = program.functions[0].code.iter().any(|instr| {
            matches!(
                instr,
                CsilInstr::Jump { .. }
                    | CsilInstr::JumpIfFalse { .. }
                    | CsilInstr::JumpIfExact { .. }
                    | CsilInstr::JumpIfSlotWritten { .. }
            )
        });
        // The baseline refuses every branching program with integer
        // arithmetic and skips its range pass entirely, so it cannot judge the
        // block-local pass there (which also fails closed on the malformed
        // stacks that the baseline's early return never inspects). Branching
        // shapes are pinned by
        // `block_local_arithmetic_admits_in_block_operands_only`.
        // The production pass bounds LastBarIndex at 0: empty runs have no
        // executing bars, so -1 is not observable, and programs that read it
        // are excluded from this comparison.
        let corrected_domain = program.functions[0].code.iter().any(|instr| {
            matches!(
                instr,
                CsilInstr::ReadBuiltin {
                    source: BuiltinSeries::LastBarIndex,
                    ..
                } | CsilInstr::ReadBuiltinDynamic {
                    source: BuiltinSeries::LastBarIndex
                }
            )
        });
        if !branching && !corrected_domain && old != new {
            disagreements += 1;
        }
        match new {
            None => none_seen += 1,
            Some(_) => some_seen += 1,
        }
    }
    assert_eq!(disagreements, 0);
    // Both outcomes are exercised.
    assert!(none_seen > 1_000, "none {none_seen}");
    assert!(some_seen > 1_000, "some {some_seen}");
}

/// Kernel boundary pins for the integer-guard cases.
#[test]
fn arith_fact_boundaries() {
    let lit = |v: i64| IntFact::Range(v, v);
    // (MIN + 1) + (-1) = MIN: rejected.
    assert_eq!(arith_fact(IntArith::Add, lit(i64::MIN + 1), lit(-1)), None);
    // (MIN + 1) - 1 = MIN: rejected.
    assert_eq!(arith_fact(IntArith::Sub, lit(i64::MIN + 1), lit(1)), None);
    // -2^62 * 2 = MIN: rejected.
    assert_eq!(arith_fact(IntArith::Mul, lit(-(1 << 62)), lit(2)), None);
    // (MIN + 1) * 2 overflows everywhere: na only.
    assert_eq!(
        arith_fact(IntArith::Mul, lit(i64::MIN + 1), lit(2)),
        Some(IntFact::NaOnly)
    );
    // MAX + 1 overflows everywhere: na only.
    assert_eq!(
        arith_fact(IntArith::Add, lit(i64::MAX), lit(1)),
        Some(IntFact::NaOnly)
    );
    // [MAX - 1, MAX] + 1 is finite [MAX, MAX].
    assert_eq!(
        arith_fact(
            IntArith::Add,
            IntFact::Range(i64::MAX - 1, i64::MAX),
            lit(1)
        ),
        Some(IntFact::Range(i64::MAX, i64::MAX))
    );
    // (MIN + 2) + (-1) = MIN + 1: admitted.
    assert_eq!(
        arith_fact(IntArith::Add, lit(i64::MIN + 2), lit(-1)),
        Some(lit(i64::MIN + 1))
    );
    // na absorbs; unknown operands reject.
    assert_eq!(
        arith_fact(IntArith::Add, IntFact::NaOnly, IntFact::Unknown),
        Some(IntFact::NaOnly)
    );
    assert_eq!(arith_fact(IntArith::Add, IntFact::Unknown, lit(1)), None);
    // Straddling interval below MIN is caught by the first test.
    assert_eq!(
        guard_bounds(i128::from(i64::MIN) - 5, i128::from(i64::MIN) + 5),
        None
    );
}

/// Kernel stack discipline: underflow fails closed; `Other` pushes one
/// type-derived fact per result.
#[test]
fn int_fact_step_stack_discipline() {
    let mut stack = vec![];
    assert!(int_fact_step(
        &mut stack,
        IntOp::Arith(IntArith::Add),
        2,
        1,
        &[VTy::I64]
    ));
    let mut stack = vec![IntFact::Range(1, 2), IntFact::Range(3, 4)];
    assert!(!int_fact_step(
        &mut stack,
        IntOp::Arith(IntArith::Sub),
        2,
        1,
        &[VTy::I64]
    ));
    assert_eq!(stack, vec![IntFact::Range(-3, -1)]);
    assert!(!int_fact_step(
        &mut stack,
        IntOp::Other,
        0,
        2,
        &[VTy::I64, VTy::F64]
    ));
    assert_eq!(
        stack,
        vec![IntFact::Range(-3, -1), IntFact::Unknown, IntFact::NonInt]
    );
    // `can_produce_valid_i64_min` is shared with the baseline; referenced so
    // the dependency is explicit.
    let _ = can_produce_valid_i64_min;
}

/// Block-local arithmetic: `bar_index + 1` inside a branch is admitted, but an
/// operand inherited across a jump target is still `Unknown` and refused.
#[test]
fn block_local_arithmetic_admits_in_block_operands_only() {
    use CsilInstr as I;
    let mut program = base_program();
    let i64_result = vec![VTy::I64];
    let none = Vec::new();
    // 0: PushBool; 1: JumpIfFalse -> 5; 2: bar_index; 3: PushI64(1); 4: IntAdd;
    // 5: Pop-free tail.
    program.functions[0].code = vec![
        I::PushBool(true),
        I::JumpIfFalse { target: 6 },
        I::ReadBuiltin {
            source: BuiltinSeries::BarIndex,
            offset: 0,
        },
        I::PushI64(1),
        I::IntAdd,
        I::Pop,
        I::Return,
    ];
    let results = vec![
        vec![VTy::Bool3],
        none.clone(),
        i64_result.clone(),
        i64_result.clone(),
        i64_result.clone(),
        none.clone(),
        none.clone(),
    ];
    assert_eq!(
        integer_min_ambiguity(
            &program,
            &results,
            super::IntInputBoundary::HostConstraints,
            None,
        ),
        None
    );

    // The left operand is produced before the jump target at pc 3, so the
    // add at pc 4 sees an inherited, unknown operand.
    program.functions[0].code = vec![
        I::ReadBuiltin {
            source: BuiltinSeries::BarIndex,
            offset: 0,
        },
        I::PushBool(true),
        I::JumpIfFalse { target: 3 },
        I::PushI64(1),
        I::IntAdd,
        I::Pop,
        I::Return,
    ];
    let results = vec![
        i64_result.clone(),
        vec![VTy::Bool3],
        none.clone(),
        i64_result.clone(),
        i64_result,
        none.clone(),
        none,
    ];
    assert_eq!(
        integer_min_ambiguity(
            &program,
            &results,
            super::IntInputBoundary::HostConstraints,
            None,
        ),
        Some(4)
    );
}

fn assert_with_fields_matches_reference(
    program: &CsilProgram,
    results: &[Vec<VTy>],
    fields: &BTreeMap<usize, IntFact>,
    identities: &BTreeSet<usize>,
    expected: Option<usize>,
) {
    let inputs = IntInputBoundary::HostConstraints;
    let old =
        old_integer_min_ambiguity_with_fields(program, results, inputs, None, fields, identities);
    let new = integer_min_ambiguity_with_fields(program, results, inputs, None, fields, identities);
    assert_eq!(
        new, old,
        "the split pass differs from the baseline orchestration"
    );
    assert_eq!(new, expected);
}

#[test]
fn split_pass_preserves_producer_precedence_over_earlier_bad_effect() {
    let mut program = base_program();
    program.functions[0].code = vec![
        CsilInstr::CallTa { state: u32::MAX },
        CsilInstr::PushI64(i64::MIN),
    ];
    assert!(program.functions[0].code[0]
        .resolve_stack_effect(&program.ta_states)
        .is_err());
    assert_with_fields_matches_reference(
        &program,
        &[vec![VTy::Bool3], vec![VTy::I64]],
        &BTreeMap::new(),
        &BTreeSet::new(),
        Some(1),
    );
}

#[test]
fn split_pass_preserves_first_range_failure() {
    let mut program = base_program();
    program.functions[0].code = vec![
        CsilInstr::CallTa { state: u32::MAX },
        CsilInstr::CallTa {
            state: u32::MAX - 1,
        },
    ];
    assert_with_fields_matches_reference(
        &program,
        &[vec![VTy::Bool3], vec![VTy::Bool3]],
        &BTreeMap::new(),
        &BTreeSet::new(),
        Some(0),
    );
}

#[test]
fn split_pass_preserves_shorter_result_type_vectors() {
    let mut program = base_program();
    program.functions[0].code = vec![CsilInstr::PushI64(i64::MIN), CsilInstr::Return];
    assert_with_fields_matches_reference(&program, &[], &BTreeMap::new(), &BTreeSet::new(), None);
}

#[test]
fn split_pass_preserves_same_block_and_cross_block_facts() {
    use CsilInstr as I;
    let mut program = base_program();
    let none = Vec::new();
    let int = vec![VTy::I64];
    let bool_ty = vec![VTy::Bool3];
    program.functions[0].code = vec![
        I::PushBool(true),
        I::JumpIfFalse { target: 6 },
        I::ReadBuiltin {
            source: BuiltinSeries::BarIndex,
            offset: 0,
        },
        I::PushI64(1),
        I::IntAdd,
        I::Pop,
        I::Return,
    ];
    assert_with_fields_matches_reference(
        &program,
        &[
            bool_ty.clone(),
            none.clone(),
            int.clone(),
            int.clone(),
            int.clone(),
            none.clone(),
            none.clone(),
        ],
        &BTreeMap::new(),
        &BTreeSet::new(),
        None,
    );

    program.functions[0].code = vec![
        I::ReadBuiltin {
            source: BuiltinSeries::BarIndex,
            offset: 0,
        },
        I::PushBool(true),
        I::JumpIfFalse { target: 3 },
        I::PushI64(1),
        I::IntAdd,
        I::Pop,
        I::Return,
    ];
    assert_with_fields_matches_reference(
        &program,
        &[
            int.clone(),
            bool_ty,
            none.clone(),
            int.clone(),
            int,
            none.clone(),
            none,
        ],
        &BTreeMap::new(),
        &BTreeSet::new(),
        Some(4),
    );
}

#[test]
fn split_pass_preserves_field_and_identity_precedence() {
    let mut program = base_program();
    program.functions[0].code = vec![
        CsilInstr::PushI64(i64::MIN),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let results = [vec![VTy::I64], Vec::new(), Vec::new()];
    let mut fields = BTreeMap::new();
    fields.insert(0, IntFact::Range(5, 9));
    assert_with_fields_matches_reference(&program, &results, &fields, &BTreeSet::new(), None);
    fields.insert(0, IntFact::NaOnly);
    assert_with_fields_matches_reference(&program, &results, &fields, &BTreeSet::new(), None);

    program.functions[0].code = vec![
        CsilInstr::PushI64(7),
        CsilInstr::CallPure {
            func: PureFunc::MathRound,
            argc: 1,
        },
        CsilInstr::PushI64(1),
        CsilInstr::IntAdd,
        CsilInstr::Return,
    ];
    let results = [
        vec![VTy::I64],
        vec![VTy::I64],
        vec![VTy::I64],
        vec![VTy::I64],
        Vec::new(),
    ];
    assert_with_fields_matches_reference(
        &program,
        &results,
        &BTreeMap::new(),
        &BTreeSet::from([1]),
        None,
    );

    program.functions[0].code = vec![
        CsilInstr::PushI64(7),
        CsilInstr::CallPure {
            func: PureFunc::MathMin,
            argc: 2,
        },
    ];
    assert_with_fields_matches_reference(
        &program,
        &[vec![VTy::I64], vec![VTy::I64]],
        &BTreeMap::new(),
        &BTreeSet::from([1]),
        Some(1),
    );
}

#[test]
fn split_pass_preserves_invalid_target_and_empty_code() {
    let mut program = base_program();
    program.functions[0].code = vec![CsilInstr::Jump { target: u32::MAX }, CsilInstr::Return];
    assert_with_fields_matches_reference(&program, &[], &BTreeMap::new(), &BTreeSet::new(), None);
    program.functions[0].code.clear();
    assert_with_fields_matches_reference(&program, &[], &BTreeMap::new(), &BTreeSet::new(), None);
}

#[test]
fn split_with_fields_matches_reference_randomized() {
    let mut program = base_program();
    let mut rng = Rng(0xa076_1d64_78bd_642f);
    for case in 0..2_000 {
        let len = 1 + rng.below(12) as usize;
        let code: Vec<CsilInstr> = (0..len)
            .map(|_| any_instr(&mut rng, case % 5 == 0))
            .collect();
        let mut results: Vec<Vec<VTy>> =
            code.iter().map(|instr| any_tys(&mut rng, instr)).collect();
        if rng.below(10) == 0 {
            results.truncate(rng.below(len as u64) as usize);
        }
        let mut fields = BTreeMap::new();
        let mut identities = BTreeSet::new();
        for pc in 0..len {
            if rng.below(6) == 0 {
                fields.insert(
                    pc,
                    match rng.below(4) {
                        0 => IntFact::NonInt,
                        1 => IntFact::NaOnly,
                        2 => IntFact::Range(-3, 4),
                        _ => IntFact::Unknown,
                    },
                );
            }
            if rng.below(9) == 0 {
                identities.insert(pc);
            }
        }
        program.functions[0].code = code;
        let old = old_integer_min_ambiguity_with_fields(
            &program,
            &results,
            IntInputBoundary::HostConstraints,
            None,
            &fields,
            &identities,
        );
        let new = integer_min_ambiguity_with_fields(
            &program,
            &results,
            IntInputBoundary::HostConstraints,
            None,
            &fields,
            &identities,
        );
        assert_eq!(new, old, "case {case}");
    }
}

fn assert_producer_gate_matches_reference(
    program: &CsilProgram,
    instr: &CsilInstr,
    boundary: IntInputBoundary,
    expected: bool,
) {
    assert_producer_gate_at_pc_matches_reference(program, 0, instr, boundary, expected);
}

fn assert_producer_gate_at_pc_matches_reference(
    program: &CsilProgram,
    pc: usize,
    instr: &CsilInstr,
    boundary: IntInputBoundary,
    expected: bool,
) {
    let result_types = [VTy::I64];
    let old = old_can_produce_valid_i64_min(program, pc, instr, &result_types, boundary, None);
    let new = can_produce_valid_i64_min(program, pc, instr, &result_types, boundary, None);
    assert_eq!(
        new, old,
        "the producer helper differs from its baseline implementation"
    );
    assert_eq!(new, expected);
}

#[test]
fn producer_gate_preserves_input_search_and_constraint_semantics() {
    let compiled = pinewasm_compiler::compile(
        "indicator(\"input sentinel gate\")\nx = input.int(0)\nplot(x)\n",
        pinewasm_compiler::CompileOptions::default(),
    );
    let mut program = compiled.program.expect("input fixture compiles");
    let mut first = program.inputs[0].clone();
    first.id = 17;
    first.kind = InputKind::Int;
    first.defval = InputValue::Int(0);
    first.constraints = InputConstraints::default();
    let mut second = first.clone();
    second.constraints.options = vec![InputValue::Int(1)];
    // `iter().find()` chooses the first duplicate id; the second declaration
    // would exclude MIN if the search accidentally chose the last match.
    program.inputs = vec![first, second];
    let instr = CsilInstr::ReadInput { input: 17 };
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        true,
    );

    program.inputs.truncate(1);
    program.inputs[0].constraints.options.clear();
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        true,
    );
    program.inputs[0].constraints.options = vec![InputValue::Int(i64::MIN), InputValue::Int(1)];
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        true,
    );
    program.inputs[0].constraints.options = vec![InputValue::Int(0), InputValue::Int(1)];
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        false,
    );

    program.inputs[0].constraints.options.clear();
    program.inputs[0].constraints.min = Some(InputValue::Int(i64::MIN + 1));
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        false,
    );

    program.inputs[0].constraints.min = None;
    program.inputs[0].defval = InputValue::Int(0);
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::RejectsSentinel,
        false,
    );
    program.inputs[0].defval = InputValue::Int(i64::MIN);
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::RejectsSentinel,
        true,
    );

    program.inputs.clear();
    assert_producer_gate_matches_reference(
        &program,
        &instr,
        IntInputBoundary::HostConstraints,
        true,
    );
}

#[test]
fn producer_gate_explicit_match_branches_match_frozen_oracle() {
    let mut program = base_program();
    let round = CsilInstr::CallPure {
        func: PureFunc::MathRound,
        argc: 1,
    };
    program.functions[0].code = vec![CsilInstr::PineIntDiv, round.clone()];
    assert_producer_gate_at_pc_matches_reference(
        &program,
        1,
        &round,
        IntInputBoundary::HostConstraints,
        false,
    );
    program.functions[0].code = vec![round.clone()];
    assert_producer_gate_matches_reference(
        &program,
        &round,
        IntInputBoundary::HostConstraints,
        true,
    );

    for (accessor, expected) in [
        (TradeAccessor::EntryBarIndex, false),
        (TradeAccessor::ExitBarIndex, false),
        (TradeAccessor::EntryTime, true),
        (TradeAccessor::EntryPrice, true),
    ] {
        assert_producer_gate_matches_reference(
            &program,
            &CsilInstr::CallPure {
                func: PureFunc::StrategyOpenTrade(accessor),
                argc: 0,
            },
            IntInputBoundary::HostConstraints,
            expected,
        );
    }

    assert_producer_gate_matches_reference(
        &program,
        &CsilInstr::ReadBuiltin {
            source: BuiltinSeries::BarIndex,
            offset: 0,
        },
        IntInputBoundary::HostConstraints,
        false,
    );
    assert_producer_gate_matches_reference(
        &program,
        &CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Close,
            offset: 0,
        },
        IntInputBoundary::HostConstraints,
        true,
    );
    assert_producer_gate_matches_reference(
        &program,
        &CsilInstr::CallTa { state: u32::MAX },
        IntInputBoundary::HostConstraints,
        true,
    );

    let highest = pinewasm_compiler::compile(
        "indicator(\"highest bars gate\")\nx = ta.highestbars(close, 3)\nplot(x)\n",
        pinewasm_compiler::CompileOptions::default(),
    )
    .program
    .expect("highestbars fixture compiles");
    assert_eq!(highest.ta_states.len(), 1);
    assert_producer_gate_matches_reference(
        &highest,
        &CsilInstr::CallTa { state: 0 },
        IntInputBoundary::HostConstraints,
        false,
    );

    let ema = pinewasm_compiler::compile(
        "indicator(\"EMA gate\")\nx = ta.ema(close, 3)\nplot(x)\n",
        pinewasm_compiler::CompileOptions::default(),
    )
    .program
    .expect("EMA fixture compiles");
    assert_eq!(ema.ta_states.len(), 1);
    assert_producer_gate_matches_reference(
        &ema,
        &CsilInstr::CallTa { state: 0 },
        IntInputBoundary::HostConstraints,
        true,
    );
}

/// Field reads are admitted like slot reads (every stored integer passed the
/// producer gate at its `NewObject`/`SetField` operand), while collection
/// element reads stay refused unless the native heap plan owns the site.
#[test]
fn field_reads_are_admitted_but_host_collection_reads_are_not() {
    let program = base_program();
    let int_result = [VTy::I64];
    let boundary = super::IntInputBoundary::HostConstraints;
    assert!(!can_produce_valid_i64_min(
        &program,
        0,
        &CsilInstr::GetField { field_idx: 0 },
        &int_result,
        boundary,
        None,
    ));
    for func in [
        pinewasm_hir::CollectionFunc::ArrayGet,
        pinewasm_hir::CollectionFunc::ArrayPush,
        pinewasm_hir::CollectionFunc::MapGet,
        pinewasm_hir::CollectionFunc::MatrixGet,
    ] {
        assert!(can_produce_valid_i64_min(
            &program,
            0,
            &CsilInstr::CallCollection { func, argc: 2 },
            &int_result,
            boundary,
            None,
        ));
    }
}

#[test]
fn last_bar_index_present_domain_excludes_empty_run_value() {
    for instr in [
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::LastBarIndex,
            offset: 0,
        },
        CsilInstr::ReadBuiltinDynamic {
            source: BuiltinSeries::LastBarIndex,
        },
    ] {
        assert_eq!(super::int_op(&instr), IntOp::PushRange(0, u32::MAX as i64));
    }
    assert_eq!(
        arith_fact(
            IntArith::Sub,
            IntFact::Range(0, u32::MAX as i64),
            IntFact::Range(i64::MAX, i64::MAX)
        ),
        Some(IntFact::Range(-i64::MAX, u32::MAX as i64 - i64::MAX))
    );
}

/// The guard-site collector checked against the refusing pass
/// (`integer_min_ambiguity`) as the verdict: on random programs, a program the pass clears has no guard; a
/// site the pass reports is either guarded or keeps the refusal; and an
/// arithmetic guard sits only on a checked add, subtract or multiply.
#[test]
fn guard_sites_cover_every_reported_ambiguity() {
    use super::{integer_min_guard_sites, IntMinGuard};
    use std::collections::{BTreeMap, BTreeSet};

    let mut program = base_program();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut cleared, mut guarded, mut refused) = (0, 0, 0);
    for case in 0..60_000 {
        let len = 1 + rng.below(12) as usize;
        let allow_jump = case % 4 == 0;
        let code: Vec<CsilInstr> = (0..len).map(|_| any_instr(&mut rng, allow_jump)).collect();
        let results: Vec<Vec<VTy>> = code.iter().map(|i| any_tys(&mut rng, i)).collect();
        // Every `I64`-producing call converts a float, so the producer gate's
        // rounding and cast sites are guardable.
        let operands: Vec<Vec<VTy>> = code.iter().map(|_| vec![VTy::F64]).collect();
        program.functions[0].code = code;
        let boundary = super::IntInputBoundary::HostConstraints;
        let reported = integer_min_ambiguity(&program, &results, boundary, None);
        let sites = integer_min_guard_sites(
            &program,
            &operands,
            &results,
            &[],
            boundary,
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        match (reported, sites) {
            (None, sites) => {
                assert_eq!(sites, Ok(BTreeMap::new()), "case {case}");
                cleared += 1;
            }
            (Some(pc), Ok(guards)) => {
                assert!(guards.contains_key(&pc), "case {case}: {pc} unguarded");
                for (&site, guard) in &guards {
                    let instr = &program.functions[0].code[site];
                    assert_eq!(
                        *guard == IntMinGuard::Arith,
                        matches!(
                            instr,
                            CsilInstr::IntAdd | CsilInstr::IntSub | CsilInstr::IntMul
                        ),
                        "case {case}: {guard:?} on {instr:?}"
                    );
                }
                guarded += 1;
            }
            (Some(_), Err(_)) => refused += 1,
        }
    }
    assert!(
        cleared > 1_000 && guarded > 100 && refused > 1_000,
        "{cleared} cleared, {guarded} guarded, {refused} refused"
    );
}
