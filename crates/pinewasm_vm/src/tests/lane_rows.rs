//! Every transfer row of the declared-lane analysis
//! (`pinewasm_csil::lanes`), checked by executing PineWASM's own VM.
//!
//! A row says which runtime cell kind an instruction can produce. Each test
//! here runs the VM's own handler and compares the cell it returns with the
//! row:
//!
//! * every pure function, at every operand count it accepts, over operand
//!   tuples drawn from a pool of every cell kind
//!   ([`every_pure_row_holds_in_the_vm`]);
//! * every builtin series on every bar of an indicator and of a strategy run
//!   ([`every_builtin_row_holds_in_the_vm`]);
//! * every collection function over operand tuples drawn from scalars and
//!   live array, map and matrix handles
//!   ([`every_collection_row_holds_in_the_vm`]);
//! * every TA result decoder ([`every_ta_result_row_holds_in_the_decoder`]);
//! * whole programs, with the operand stack at the entry of every executed
//!   instruction compared against the tags the checker established
//!   ([`executed_stacks_carry_the_checked_tags`]).
//!
//! The enumerations of `PureFunc` and `CollectionFunc` are exhaustive
//! matches, so a new variant does not compile until it is listed here; the
//! row functions in `lanes.rs` are exhaustive too.

use super::{compile, data_from_close};
use crate::{RunOptions, Vm};
use pinewasm_csil::lanes::{
    builtin_tag, collection_tag, pure_row, pure_tag, ta_result_tag, PureRow, Tag,
};
use pinewasm_csil::{
    CalendarField, CollectionFunc, CsilInstr, Lane, PureFunc, TaFunc, TickerFunc, TradeAccessor,
};
use pinewasm_hir::BuiltinSeries;
use pinewasm_mir::{HeapObj, Value};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Runtime cells against tags.

/// The tag of one runtime value, as an operand of the transfer function.
fn tag_of(vm: &Vm, value: &Value) -> Tag {
    match value {
        Value::Na | Value::DisplacedTable(_) => Tag::NaOnly,
        Value::Bool(_) => Tag::Is(Lane::Bool),
        Value::Int(_) => Tag::Is(Lane::I64),
        Value::Float(_) => Tag::Is(Lane::F64),
        Value::String(_) => Tag::Is(Lane::Str),
        Value::Color { .. } => Tag::Is(Lane::Color),
        Value::Ref(handle) => match vm.heap.get(*handle) {
            Some(HeapObj::Object { type_id, .. }) => Tag::Is(Lane::Obj(*type_id)),
            _ => Tag::Is(Lane::Ref),
        },
    }
}

/// Whether a runtime value is one the static tag allows.
fn fits(vm: &Vm, value: &Value, tag: Tag) -> bool {
    match (value, tag) {
        (_, Tag::Top(_)) | (Value::Na | Value::DisplacedTable(_), Tag::NaOnly | Tag::Is(_)) => true,
        (_, Tag::Bottom | Tag::NaOnly) => false,
        (Value::Bool(_), Tag::Is(lane)) => lane == Lane::Bool,
        (Value::Int(_), Tag::Is(lane)) => lane == Lane::I64,
        (Value::Float(_), Tag::Is(lane)) => lane == Lane::F64,
        (Value::String(_), Tag::Is(lane)) => lane == Lane::Str,
        (Value::Color { .. }, Tag::Is(lane)) => lane == Lane::Color,
        (Value::Ref(handle), Tag::Is(lane)) => match lane {
            Lane::Ref => true,
            Lane::Obj(declared) => matches!(
                vm.heap.get(*handle),
                Some(HeapObj::Object { type_id, .. }) if *type_id == declared
            ),
            _ => false,
        },
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Na | Value::DisplacedTable(_) => "na",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::String(_) => "string",
        Value::Color { .. } => "color",
        Value::Ref(_) => "ref",
    }
}

/// A small deterministic generator (splitmix64).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Operands of every scalar cell kind, with the values the handlers branch
/// on: zero, negatives, fractions, the i64 extremes, a millisecond time,
/// numeric and non-numeric strings, timeframe, timezone and format strings.
fn scalar_pool() -> Vec<Value> {
    let mut pool = vec![Value::Na, Value::Bool(true), Value::Bool(false)];
    for v in [
        0,
        1,
        2,
        3,
        -3,
        7,
        60,
        2024,
        1_700_000_000_000,
        i64::MIN,
        i64::MAX,
    ] {
        pool.push(Value::Int(v));
    }
    for v in [0.0, 0.5, -2.5, 3.0, 99.99, 1.0e300, 1_700_000_000_000.0] {
        pool.push(Value::Float(v));
    }
    for v in [
        "",
        "abc",
        "12.5",
        "-7",
        "1D",
        "60",
        "NASDAQ:AAPL",
        "UTC",
        "{0} and {1}",
        "yyyy-MM-dd",
        "a",
        "#.##",
    ] {
        pool.push(Value::String(v.to_string()));
    }
    pool.push(Value::Color {
        r: 10,
        g: 20,
        b: 30,
        t: 40.0,
        input_origin: None,
    });
    pool
}

fn indicator_vm() -> Vm {
    let program = compile("//@version=6\nindicator(\"rows\")\nplot(close)\n");
    let mut data = data_from_close(&[10.0, 11.5, 9.25, 12.0]);
    data.time = (0..4)
        .map(|bar| 1_700_000_000_000 + bar * 3_600_000)
        .collect();
    let mut vm = Vm::new(&program, &data, &RunOptions::default()).expect("vm");
    let errors = vm.run_history().1;
    assert!(errors.is_empty(), "{errors:?}");
    vm
}

// ---------------------------------------------------------------------------
// Pure functions.

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

const CALENDAR_FIELDS: [CalendarField; 7] = [
    CalendarField::Year,
    CalendarField::Month,
    CalendarField::DayOfMonth,
    CalendarField::DayOfWeek,
    CalendarField::Hour,
    CalendarField::Minute,
    CalendarField::Second,
];

const TICKER_FUNCS: [TickerFunc; 9] = [
    TickerFunc::New,
    TickerFunc::Modify,
    TickerFunc::Heikinashi,
    TickerFunc::Kagi,
    TickerFunc::Linebreak,
    TickerFunc::Pointfigure,
    TickerFunc::Renko,
    TickerFunc::Standard,
    TickerFunc::Inherit,
];

const TRADE_ACCESSORS: [TradeAccessor; 18] = [
    TradeAccessor::EntryPrice,
    TradeAccessor::ExitPrice,
    TradeAccessor::Size,
    TradeAccessor::Profit,
    TradeAccessor::ProfitPercent,
    TradeAccessor::Commission,
    TradeAccessor::EntryBarIndex,
    TradeAccessor::ExitBarIndex,
    TradeAccessor::EntryTime,
    TradeAccessor::ExitTime,
    TradeAccessor::MaxRunup,
    TradeAccessor::MaxRunupPercent,
    TradeAccessor::MaxDrawdown,
    TradeAccessor::MaxDrawdownPercent,
    TradeAccessor::EntryId,
    TradeAccessor::ExitId,
    TradeAccessor::EntryComment,
    TradeAccessor::ExitComment,
];

/// The payload lists above are complete: each of these matches is
/// exhaustive, and every variant must be found in its list.
fn payload_lists_are_complete() {
    for field in CALENDAR_FIELDS {
        match field {
            CalendarField::Year
            | CalendarField::Month
            | CalendarField::DayOfMonth
            | CalendarField::DayOfWeek
            | CalendarField::Hour
            | CalendarField::Minute
            | CalendarField::Second => {}
        }
    }
    for func in TICKER_FUNCS {
        match func {
            TickerFunc::New
            | TickerFunc::Modify
            | TickerFunc::Heikinashi
            | TickerFunc::Kagi
            | TickerFunc::Linebreak
            | TickerFunc::Pointfigure
            | TickerFunc::Renko
            | TickerFunc::Standard
            | TickerFunc::Inherit => {}
        }
    }
    for field in TRADE_ACCESSORS {
        match field {
            TradeAccessor::EntryPrice
            | TradeAccessor::ExitPrice
            | TradeAccessor::Size
            | TradeAccessor::Profit
            | TradeAccessor::ProfitPercent
            | TradeAccessor::Commission
            | TradeAccessor::EntryBarIndex
            | TradeAccessor::ExitBarIndex
            | TradeAccessor::EntryTime
            | TradeAccessor::ExitTime
            | TradeAccessor::MaxRunup
            | TradeAccessor::MaxRunupPercent
            | TradeAccessor::MaxDrawdown
            | TradeAccessor::MaxDrawdownPercent
            | TradeAccessor::EntryId
            | TradeAccessor::ExitId
            | TradeAccessor::EntryComment
            | TradeAccessor::ExitComment => {}
        }
    }
    let distinct = |names: Vec<String>| names.into_iter().collect::<BTreeSet<_>>().len();
    assert_eq!(
        distinct(CALENDAR_FIELDS.iter().map(|f| format!("{f:?}")).collect()),
        7
    );
    assert_eq!(
        distinct(TICKER_FUNCS.iter().map(|f| format!("{f:?}")).collect()),
        9
    );
    assert_eq!(
        distinct(TRADE_ACCESSORS.iter().map(|f| format!("{f:?}")).collect()),
        18
    );
}

/// Every pure function, with every payload of the payload-carrying ones.
fn pure_funcs() -> Vec<PureFunc> {
    let mut funcs = Vec::new();
    let mut func = next_purefunc(None);
    while let Some(current) = func {
        match current {
            PureFunc::Calendar(_) => funcs.extend(CALENDAR_FIELDS.map(PureFunc::Calendar)),
            PureFunc::Ticker(_) => funcs.extend(TICKER_FUNCS.map(PureFunc::Ticker)),
            PureFunc::StrategyClosedTrade(_) => {
                funcs.extend(TRADE_ACCESSORS.map(PureFunc::StrategyClosedTrade));
            }
            PureFunc::StrategyOpenTrade(_) => {
                funcs.extend(TRADE_ACCESSORS.map(PureFunc::StrategyOpenTrade));
            }
            other => funcs.push(other),
        }
        func = next_purefunc(func);
    }
    funcs
}

/// Operand tuples for one call: every tuple from the pool while that is
/// small, otherwise a fixed number of drawn ones.
fn operand_tuples(pool: &[Value], argc: usize, rng: &mut Rng) -> Vec<Vec<Value>> {
    const DRAWN: usize = 2000;
    let exhaustive = pool.len().checked_pow(argc as u32).filter(|n| *n <= DRAWN);
    match exhaustive {
        Some(count) => (0..count)
            .map(|mut index| {
                (0..argc)
                    .map(|_| {
                        let value = pool[index % pool.len()].clone();
                        index /= pool.len();
                        value
                    })
                    .collect()
            })
            .collect(),
        None => (0..DRAWN)
            .map(|_| {
                (0..argc)
                    .map(|_| pool[rng.below(pool.len())].clone())
                    .collect()
            })
            .collect(),
    }
}

/// Well-typed operand tuples for the calls with too many operands for a
/// drawn tuple to be well typed by chance.
fn witnesses(func: PureFunc, argc: u8) -> Vec<Vec<Value>> {
    let text = |s: &str| Value::String(s.to_string());
    let color = |r: u8| Value::Color {
        r,
        g: 20,
        b: 30,
        t: 0.0,
        input_origin: None,
    };
    match (func, argc) {
        (PureFunc::ColorFromGradient, 5) => vec![vec![
            Value::Float(0.5),
            Value::Float(0.0),
            Value::Float(1.0),
            color(0),
            color(200),
        ]],
        (PureFunc::TimeOpen | PureFunc::TimeClose, 5) => {
            let mut tuples = Vec::new();
            for timeframe in ["", "1", "60", "240", "D", "1D", "W"] {
                for session in ["", "0000-0000", "0930-1600"] {
                    for timezone in ["", "UTC"] {
                        for bars_back in [0, 1] {
                            tuples.push(vec![
                                text(timeframe),
                                text(session),
                                text(timezone),
                                Value::Int(bars_back),
                                Value::Int(0),
                            ]);
                        }
                    }
                }
            }
            tuples
        }
        (PureFunc::TimestampTz, 7) => vec![vec![
            text("UTC"),
            Value::Int(2024),
            Value::Int(3),
            Value::Int(9),
            Value::Int(12),
            Value::Int(30),
            Value::Int(0),
        ]],
        _ => Vec::new(),
    }
}

#[test]
fn every_pure_row_holds_in_the_vm() {
    payload_lists_are_complete();
    let pool = scalar_pool();
    let mut rng = Rng(0x1A9E5);
    let funcs = pure_funcs();
    // Four float-returning color accessors supplement the rows listed above.
    assert_eq!(funcs.len(), 73 - 4 + 7 + 9 + 18 + 18);
    // Function -> the cell kinds it returned.
    let mut seen: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut calls = 0u64;
    for func in funcs {
        let (min, max) = func.argc_bounds();
        for argc in min..=max.min(7) {
            let mut vm = indicator_vm();
            if std::env::var_os("LANE_ROWS_PRINT").is_some() {
                eprintln!("start\t{func:?}/{argc}");
            }
            let mut tuples = witnesses(func, argc);
            tuples.extend(operand_tuples(&pool, usize::from(argc), &mut rng));
            for args in tuples {
                // `str.repeat` of an empty string loops once per repetition
                // without a charge, so a huge count does not return. The
                // interpreter reaches it through the same handler; the
                // counts are kept small here.
                if func == PureFunc::StrRepeat
                    && !matches!(args[1], Value::Na | Value::Int(0..=1000))
                    && !matches!(args[1], Value::Float(n) if n < 1000.0)
                {
                    continue;
                }
                let mut budget = u64::MAX;
                let started = std::time::Instant::now();
                let outcome = vm.call_pure(func, &args, 0, &mut budget);
                if started.elapsed().as_millis() > 200
                    && std::env::var_os("LANE_ROWS_PRINT").is_some()
                {
                    println!("slow\t{func:?}\t{args:?}\t{:?}", started.elapsed());
                }
                let Ok(result) = outcome else {
                    continue;
                };
                calls += 1;
                // `popped[0]` is the top of the stack: the last argument.
                let popped: Vec<Tag> = args.iter().rev().map(|arg| tag_of(&vm, arg)).collect();
                let tag = pure_tag(func, argc, &popped);
                assert!(
                    fits(&vm, &result, tag),
                    "{func:?}({args:?}) returned {result:?}, the row says {tag:?}"
                );
                seen.entry(format!("{func:?}/{argc}"))
                    .or_default()
                    .insert(kind(&result));
            }
        }
    }
    if std::env::var_os("LANE_ROWS_PRINT").is_some() {
        for (func, kinds) in &seen {
            println!("pure\t{func}\t{kinds:?}");
        }
        println!("pure calls checked: {calls}");
    }
    // A fixed row is only evidence when the VM actually produced a non-na
    // cell for it.
    let mut vacuous = Vec::new();
    for func in pure_funcs() {
        let (min, max) = func.argc_bounds();
        for argc in min..=max.min(7) {
            if matches!(pure_row(func, argc), PureRow::Unfixed) {
                continue;
            }
            let produced = seen
                .get(&format!("{func:?}/{argc}"))
                .is_some_and(|kinds| kinds.iter().any(|kind| *kind != "na"));
            if !produced {
                vacuous.push(format!("{func:?}/{argc}"));
            }
        }
    }
    assert_eq!(
        vacuous,
        Vec::<String>::new(),
        "rows the VM never produced a value for"
    );
}

// ---------------------------------------------------------------------------
// Builtin series.

/// A feed with every optional host column present, so no builtin series is
/// na for lack of data.
fn full_feed() -> crate::MarketData {
    let close = [10.0, 11.0, 13.0, 12.0, 9.0, 11.0];
    let mut data = data_from_close(&close);
    data.time = (0..6)
        .map(|bar| 1_700_000_000_000 + bar * 3_600_000)
        .collect();
    data.bid = close.iter().map(|price| price - 0.25).collect();
    data.ask = close.iter().map(|price| price + 0.25).collect();
    data.chart_left_visible_bar_time = Some(1_700_000_000_000);
    data.chart_right_visible_bar_time = Some(1_700_018_000_000);
    let s = &mut data.syminfo;
    s.expiration_date = Some(1_800_000_000_000);
    s.current_contract = "CME_MINI:ESZ2026".to_string();
    s.employees = Some(1200);
    s.shareholders = Some(34);
    s.shares_outstanding_float = Some(1.5e9);
    s.shares_outstanding_total = Some(2_000_000_000);
    s.recommendations_buy = Some(7);
    s.recommendations_buy_strong = Some(3);
    s.recommendations_date = Some(1_700_000_000_000);
    s.recommendations_hold = Some(5);
    s.recommendations_sell = Some(1);
    s.recommendations_sell_strong = Some(0);
    s.recommendations_total = Some(16);
    s.target_price_average = Some(12.5);
    s.target_price_date = Some(1_700_000_000_000);
    s.target_price_estimates = Some(9.0);
    s.target_price_high = Some(20.0);
    s.target_price_low = Some(8.0);
    s.target_price_median = Some(12.0);
    let e = &mut data.upcoming_events;
    e.dividends_future_amount = Some(0.22);
    e.dividends_future_ex_date = Some(1_700_100_000_000);
    e.dividends_future_pay_date = Some(1_700_200_000_000);
    e.earnings_future_eps = Some(1.1);
    e.earnings_future_period_end_time = Some(1_700_300_000_000);
    e.earnings_future_revenue = Some(3.0e9);
    e.earnings_future_time = Some(1_700_400_000_000);
    data
}

/// The builtin series the runs below never produce a value for. Their rows
/// are `Top` in `builtin_tag`; a series leaves this list by being produced.
const UNPRODUCED_BUILTINS: &[&str] = &["StrategyMarginLiquidationPrice"];

#[test]
fn every_builtin_row_holds_in_the_vm() {
    // An indicator, and a strategy with a winning and a losing closed trade
    // and a position still open on the last bar.
    let sources = [
        "//@version=6\nindicator(\"rows\")\nplot(close)\n",
        "//@version=6\nstrategy(\"rows\", margin_long = 50)\n\
         if bar_index == 0 or bar_index == 2 or bar_index == 4\n    strategy.entry(\"L\", strategy.long)\n\
         if bar_index == 1 or bar_index == 3\n    strategy.close(\"L\")\nplot(close)\n",
    ];
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for source in sources {
        let program = compile(source);
        let data = full_feed();
        let mut vm = Vm::new(&program, &data, &RunOptions::default()).expect("vm");
        let errors = vm.run_history().1;
        assert!(errors.is_empty(), "{errors:?}");
        for source in BuiltinSeries::ALL {
            for offset in 0..6 {
                let value = vm.read_builtin(source, offset);
                let tag = builtin_tag(source);
                assert!(
                    fits(&vm, &value, tag),
                    "{source:?}[{offset}] is {value:?}, the row says {tag:?}"
                );
                if !matches!(value, Value::Na) {
                    produced.insert(format!("{source:?}"));
                }
            }
        }
    }
    let unproduced: Vec<String> = BuiltinSeries::ALL
        .iter()
        .map(|series| format!("{series:?}"))
        .filter(|series| !produced.contains(series))
        .collect();
    assert_eq!(unproduced, UNPRODUCED_BUILTINS);
    for series in BuiltinSeries::ALL {
        if UNPRODUCED_BUILTINS.contains(&format!("{series:?}").as_str()) {
            assert!(matches!(builtin_tag(series), Tag::Top(_)), "{series:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// TA result decoders.

#[test]
fn every_ta_result_row_holds_in_the_decoder() {
    let vm = indicator_vm();
    let samples = [
        0.0,
        1.0,
        -1.0,
        2.5,
        -0.5,
        7.0,
        1.0e18,
        1.0e300,
        4_278_190_080.0,
        f64::NAN,
        f64::INFINITY,
    ];
    for func in TaFunc::ALL {
        for value in samples {
            let decoded = crate::ta::decode(func, value);
            let tag = ta_result_tag(func);
            assert!(
                fits(&vm, &decoded, tag),
                "{func:?} decodes {value} as {decoded:?}, the row says {tag:?}"
            );
        }
    }
    // The functions that keep a fraction really do, which is why their row
    // is `Top`. `MedianInt` truncates and stays an integer cell.
    assert!(matches!(
        crate::ta::decode(TaFunc::MedianInt, 2.5),
        Value::Int(2)
    ));
    assert!(matches!(
        ta_result_tag(TaFunc::MedianInt),
        Tag::Is(Lane::I64)
    ));
    for func in [
        TaFunc::ValueWhenInt,
        TaFunc::FixnanInt,
        TaFunc::ModeInt,
        TaFunc::RangeInt,
    ] {
        assert!(matches!(crate::ta::decode(func, 2.5), Value::Float(_)));
        assert!(matches!(crate::ta::decode(func, 2.0), Value::Int(2)));
        assert!(matches!(ta_result_tag(func), Tag::Top(_)));
    }
}

// ---------------------------------------------------------------------------
// Collection functions.

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

/// Small scalars: sizes and indices that stay inside the heap limits.
fn collection_scalars() -> Vec<Value> {
    let mut pool = vec![Value::Na, Value::Bool(true), Value::Bool(false)];
    pool.extend([0, 1, 2, 3, -1].map(Value::Int));
    pool.extend([0.5, 2.0, -1.5].map(Value::Float));
    pool.extend(["a", "b,c", ","].map(|s| Value::String(s.to_string())));
    pool.push(Value::Color {
        r: 1,
        g: 2,
        b: 3,
        t: 0.0,
        input_origin: None,
    });
    pool
}

const COLLECTION_SETUP: &str = "//@version=6
indicator(\"rows\")
var a1 = array.from(1.5, 2.5, 3.5)
var extra_values = array.from(4.0, 1.0, 2.0, 2.0)
var a3 = array.from(1, 2, 3)
var a4 = array.from(7, 7, 2)
var a5 = array.from(true, false)
var a6 = array.from(\"a\", \"b\")
var a7 = array.from(color.red, color.blue)
var m1 = matrix.new<float>(2, 2, 1.5)
var m2 = matrix.new<float>(2, 2, 2.0)
var m3 = matrix.new<float>(3, 3, 0.0)
var m4 = matrix.new<int>(2, 2, 1)
var m5 = matrix.new<int>(2, 3, 4)
var m6 = matrix.new<float>(2, 3, 0.5)
var p1 = map.new<string, float>()
var p2 = map.new<int, int>()
var p3 = map.new<string, string>()
if barstate.isfirst
    m1.set(0, 1, 0.25)
    m3.set(0, 0, 1.0)
    m3.set(1, 1, 1.0)
    m3.set(2, 2, 1.0)
    p1.put(\"a\", 1.5)
    p1.put(\"b,c\", 2.5)
    p2.put(1, 2)
    p2.put(2, 3)
    p3.put(\"a\", \"b\")
plot(close)
";

/// The collection functions the run below never gets a non-na value from
/// although their row fixes a cell kind. Their class row is not evidence for
/// them; they are listed so the gap is visible and cannot grow unnoticed.
// The internal scan needs a metered dispatcher and valid coupled object/array
// operands. Its successful Float row is covered by the distance_scan and compiler
// equivalence tests, rather than the unmetered random-tuple harness here.
const UNPRODUCED_COLLECTIONS: &[&str] = &["InternalDistanceScan"];

#[test]
fn every_collection_row_holds_in_the_vm() {
    let mut funcs = Vec::new();
    let mut func = next_collectionfunc(None);
    while let Some(current) = func {
        funcs.push(current);
        func = next_collectionfunc(func);
    }
    let mut rng = Rng(0xC011);
    // A run that leaves arrays, maps and matrices of every element kind
    // alive in `var` slots; their handles join the operand pool.
    let program = compile(COLLECTION_SETUP);
    let data = full_feed();
    let mut vm = Vm::new(&program, &data, &RunOptions::default()).expect("vm");
    let errors = vm.run_history().1;
    assert!(errors.is_empty(), "{errors:?}");
    let mut pool = collection_scalars();
    for index in 0..vm.heap.arena_len() {
        let handle = pinewasm_mir::HandleId(index as u32);
        if vm.heap.get(handle).is_some() {
            pool.push(Value::Ref(handle));
        }
    }
    let scalars = pool.len();
    assert!(
        scalars > collection_scalars().len() + 12,
        "the setup built no collections"
    );
    let mut produced: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut calls = 0u64;
    // Calls a drawn tuple will not hit: a fresh invertible 2x2 float matrix
    // and the operations that need one.
    {
        let float_tag = Value::Int(pinewasm_mir::ElemType::Float.to_tag());
        let int = Value::Int;
        let mut witness = |vm: &mut Vm, func: CollectionFunc, args: Vec<Value>| {
            let result = vm
                .call_collection(func, &args, 0)
                .unwrap_or_else(|error| panic!("{func:?}({args:?}): {error:?}"));
            let tag = collection_tag(func);
            assert!(
                fits(vm, &result, tag),
                "{func:?}({args:?}) returned {result:?}, the row says {tag:?}"
            );
            produced
                .entry(format!("{func:?}"))
                .or_default()
                .insert(kind(&result));
            result
        };
        let matrix = witness(
            &mut vm,
            CollectionFunc::MatrixNew,
            vec![int(2), int(2), Value::Float(1.0), float_tag],
        );
        for (row, col, value) in [(0, 0, 2.0), (1, 1, 4.0)] {
            witness(
                &mut vm,
                CollectionFunc::MatrixSet,
                vec![matrix.clone(), int(row), int(col), Value::Float(value)],
            );
        }
        witness(&mut vm, CollectionFunc::MatrixInv, vec![matrix.clone()]);
        witness(
            &mut vm,
            CollectionFunc::MatrixPow,
            vec![matrix.clone(), int(2)],
        );
        witness(
            &mut vm,
            CollectionFunc::MatrixSubmatrix,
            vec![matrix, int(0), int(1), int(0), int(1)],
        );
    }
    // Three passes: the first builds handles out of scalars, the later ones
    // draw operands from scalars and live handles alike.
    for pass in 0..3 {
        for &func in &funcs {
            // Free what the previous function allocated, keeping the pool.
            let roots: Vec<pinewasm_mir::HandleId> = pool
                .iter()
                .filter_map(|value| match value {
                    Value::Ref(handle) => Some(*handle),
                    _ => None,
                })
                .collect();
            vm.heap.collect(&roots);
            let (min, max) = func.argc_bounds();
            for argc in min..=max.min(6.max(min)) {
                for _ in 0..1500 {
                    let args: Vec<Value> = (0..argc)
                        .map(|_| pool[rng.below(pool.len())].clone())
                        .collect();
                    let Ok(result) = vm.call_collection(func, &args, 0) else {
                        continue;
                    };
                    calls += 1;
                    let tag = collection_tag(func);
                    assert!(
                        fits(&vm, &result, tag),
                        "{func:?}({args:?}) returned {result:?}, the row says {tag:?}"
                    );
                    produced
                        .entry(format!("{func:?}"))
                        .or_default()
                        .insert(kind(&result));
                    // Keep a bounded number of live handles as operands.
                    if matches!(result, Value::Ref(_)) && pool.len() < scalars + 60 + pass * 30 {
                        pool.push(result);
                    }
                }
            }
        }
    }
    if std::env::var_os("LANE_ROWS_PRINT").is_some() {
        for (func, kinds) in &produced {
            println!("collection\t{func}\t{kinds:?}");
        }
        println!("collection calls checked: {calls}");
    }
    let unproduced: Vec<String> = funcs
        .iter()
        .filter(|func| !matches!(collection_tag(**func), Tag::Top(_)))
        .map(|func| format!("{func:?}"))
        .filter(|func| {
            !produced
                .get(func)
                .is_some_and(|kinds| kinds.iter().any(|kind| *kind != "na"))
        })
        .collect();
    assert_eq!(unproduced, UNPRODUCED_COLLECTIONS);
}

// ---------------------------------------------------------------------------
// Whole programs: every executed stack against the checked tags.

/// What the checker compares against while a run is in progress.
struct Monitor {
    facts: pinewasm_csil::lanes::LaneFacts,
    /// Instruction entries observed.
    entries: u64,
    /// Per-PC entries, including the intermediate rows of fused instructions.
    entries_by_pc: Vec<u64>,
    /// Stack cells compared.
    cells: u64,
    /// Cells with a fixed tag that held a non-na value.
    fixed_values: u64,
    /// The first disagreement, if any.
    violation: Option<String>,
}

thread_local! {
    static MONITOR: std::cell::RefCell<Option<Monitor>> = const { std::cell::RefCell::new(None) };
}

/// Called by the dispatch loop before each instruction of `main` executes.
/// Does nothing unless a test installed a checker on this thread.
pub(crate) fn observe(vm: &Vm, pc: usize) {
    MONITOR.with(|monitor| {
        let mut monitor = monitor.borrow_mut();
        let Some(monitor) = monitor.as_mut() else {
            return;
        };
        if monitor.violation.is_some() {
            return;
        }
        monitor.entries += 1;
        if let Some(entries) = monitor.entries_by_pc.get_mut(pc) {
            *entries += 1;
        }
        let Some(Some(expected)) = monitor.facts.stacks.first().and_then(|main| main.get(pc))
        else {
            monitor.violation = Some(format!(
                "pc {pc} executes but the analysis never reached it"
            ));
            return;
        };
        if expected.len() != vm.stack.len() {
            monitor.violation = Some(format!(
                "pc {pc}: {} cells on the stack, the analysis has {}",
                vm.stack.len(),
                expected.len()
            ));
            return;
        }
        for (depth, (value, tag)) in vm.stack.iter().zip(expected).enumerate() {
            monitor.cells += 1;
            if !fits(vm, value, *tag) {
                monitor.violation = Some(format!(
                    "pc {pc}, cell {depth}: the VM holds {value:?}, the analysis says {tag:?}"
                ));
                return;
            }
            if matches!(tag, Tag::Is(_)) && !matches!(value, Value::Na) {
                monitor.fixed_values += 1;
            }
        }
    });
}

/// A longer feed with every optional host column present.
fn long_feed(bars: usize) -> crate::MarketData {
    let close: Vec<f64> = (0..bars)
        .map(|bar| 100.0 + (bar as f64 * 0.7).sin() * 4.0 + bar as f64 * 0.05)
        .collect();
    let mut data = full_feed();
    data.open = close.iter().map(|price| price - 0.3).collect();
    data.high = close.iter().map(|price| price + 1.0).collect();
    data.low = close.iter().map(|price| price - 1.0).collect();
    data.volume = (0..bars)
        .map(|bar| 1000.0 + (bar % 7) as f64 * 10.0)
        .collect();
    data.time = (0..bars as i64)
        .map(|bar| 1_700_000_000_000 + bar * 3_600_000)
        .collect();
    data.bid = close.iter().map(|price| price - 0.25).collect();
    data.ask = close.iter().map(|price| price + 0.25).collect();
    data.close = close;
    data
}

/// Compile without panicking: `None` for a source the front end rejects.
fn try_compile(source: &str) -> Option<pinewasm_csil::CsilProgram> {
    let parsed = pinewasm_parse::parse(source);
    if pinewasm_span::has_errors(&parsed.diagnostics) {
        return None;
    }
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    if pinewasm_span::has_errors(&analysis.diagnostics) {
        return None;
    }
    let program = pinewasm_csil::lower_mir(&pinewasm_mir::lower(&analysis.hir)).ok()?;
    pinewasm_csil::validate(&program).ok()?;
    Some(program)
}

fn run_observed(
    program: &pinewasm_csil::CsilProgram,
    facts: pinewasm_csil::lanes::LaneFacts,
    data: &crate::MarketData,
    options: &RunOptions,
) -> (Monitor, crate::RunOutcome) {
    MONITOR.with(|monitor| {
        *monitor.borrow_mut() = Some(Monitor {
            facts,
            entries: 0,
            entries_by_pc: vec![0; program.main().code.len()],
            cells: 0,
            fixed_values: 0,
            violation: None,
        });
    });
    let outcome = crate::run(program, data, options);
    let monitor = MONITOR
        .with(|monitor| monitor.borrow_mut().take())
        .expect("installed above");
    (monitor, outcome)
}

/// Declare every place the joint fixpoint supports, validate the declared
/// program, and run it with the checker installed. Returns the checker's
/// counters, or the disagreement.
fn run_with_monitor(
    mut program: pinewasm_csil::CsilProgram,
    tamper: impl FnOnce(&mut pinewasm_csil::lanes::LaneFacts),
) -> Result<(u64, u64, u64, usize), String> {
    let solution = pinewasm_csil::lanes::solve_lanes(&program);
    pinewasm_csil::lanes::apply_solution(&mut program, &solution);
    let declared = solution
        .slots
        .iter()
        .chain(solution.fields.iter().flatten())
        .filter(|place| matches!(place, pinewasm_csil::lanes::PlaceOutcome::Declared(_)))
        .count();
    pinewasm_csil::validate_with_lane_facts(&program)
        .map_err(|errors| format!("the checker rejects the solved declarations: {errors:?}"))?;
    // The facts of an undeclared program are just as checkable.
    let mut facts = pinewasm_csil::lanes::check_lanes(&program)
        .map_err(|errors| format!("check_lanes: {errors:?}"))?;
    tamper(&mut facts);
    let data = long_feed(40);
    // A run that stops with a runtime error is still a run: every stack up
    // to the stop was observed.
    let (monitor, _) = run_observed(&program, facts, &data, &RunOptions::default());
    match monitor.violation {
        Some(violation) => Err(violation),
        None => Ok((
            monitor.entries,
            monitor.cells,
            monitor.fixed_values,
            declared,
        )),
    }
}

// The first bar supplies a numeric `na`; later bars produce finite results.
// Alternating gates exercise both successors of the composed Boolean tail.
const ARRAY_MATH_STORE_READ: &str = "var values = array.from(2.0)\nfloat acc = 0.0\nbool gate = bar_index % 2 == 0\nfloat current = bar_index == 0 ? na : close\nacc := acc + math.log(1.0 + math.abs(current - array.get(values, 0)))\nif gate\n    acc := nz(acc) + 1.0\nplot(acc)\n";

/// Programs written for the rows no fixture reaches: object fields and
/// copies, slots that feed their own store, an integer quotient, the
/// two TA functions that keep a fraction, and collections of each kind.
const MONITOR_SNIPPETS: &[&str] = &[
    "type T\n    int n\n    int m\nvar T st = T.new(3, 0)\nst.m := st.n\nplot(st.m)\n",
    "type T\n    int n\nvar T st = T.new(na)\nst.n := bar_index % 5\nplot(nz(st.n) + math.abs(st.n))\n",
    "type T\n    float x\n    bool b\n    string s\n    color c\nvar T st = T.new(close, true, \"a\", color.red)\nu = T.copy(st)\nu.x := st.x + 1\nplot(u.b ? u.x : str.length(u.s))\n",
    "type A\n    int n\ntype B\n    A a\n    int m\nvar B b = B.new(A.new(2), 0)\nb.m := b.a.n + bar_index\nplot(b.m)\n",
    "type T\n    int n\nvar arr = array.new<T>()\nif bar_index == 0\n    arr.push(T.new(1))\no = arr.get(0)\no.n := o.n + 1\nplot(o.n)\n",
    "var int n = 0\nn := n + 1\nvar float acc = 0.0\nacc := acc + close\nint k = bar_index / 2\nint j = k * 3\nplot(n + j)\nplot(acc / k)\n",
    "int q = bar_index / 2\nv = ta.valuewhen(close > open, q, 0)\nf = fixnan(q)\nplot(v + f)\nplot(ta.barssince(close > open))\n",
    "a = array.from(3, 1, 2)\nb = array.from(1.5, 2.5)\nm = map.new<string, int>()\nm.put(\"k\", bar_index)\nx = matrix.new<float>(2, 2, 1.0)\nplot(a.max() + b.avg() + nz(m.get(\"k\")) + x.get(0, 0) + a.size())\n",
    "s = str.tostring(close) + \"x\"\nc = close > open ? color.red : color.blue\nb = na(close[1]) or close > close[1]\nint i = math.round(close)\nfloat f = nz(close[1], open)\nplot(str.length(s) + color.r(c) + (b ? i : f))\n",
    "var line l = na\nif bar_index == 2\n    l := line.new(bar_index, close, bar_index + 1, close)\nif bar_index > 2\n    line.set_y2(l, close)\nplot(nz(line.get_y2(l)))\n",
    ARRAY_MATH_STORE_READ,
];

#[test]
fn composed_array_math_store_read_observes_every_row_and_fuel_boundary() {
    let prepare = |body: &str| {
        let mut program = compile(&format!(
            "//@version=6\nindicator(\"composed rows\")\n{body}"
        ));
        // Budget-error source remapping may replay a different program. This
        // checker attributes only this optimized instruction list.
        program.functions[0].budget_reference = None;
        let solution = pinewasm_csil::lanes::solve_lanes(&program);
        pinewasm_csil::lanes::apply_solution(&mut program, &solution);
        pinewasm_csil::validate_with_lane_facts(&program).expect("solved declarations");
        let facts = pinewasm_csil::lanes::check_lanes(&program).expect("checked stacks");
        let pc = program
            .main()
            .code
            .windows(9)
            .position(|code| {
                matches!(
                    code,
                    [
                        CsilInstr::CallCollection {
                            func: CollectionFunc::ArrayGet,
                            argc: 2
                        },
                        CsilInstr::F64Sub,
                        CsilInstr::CallPure {
                            func: PureFunc::MathAbs,
                            argc: 1
                        },
                        CsilInstr::F64Add,
                        CsilInstr::CallPure {
                            func: PureFunc::MathLog,
                            argc: 1
                        },
                        CsilInstr::F64Add,
                        CsilInstr::StoreUserSlotCurrent { .. },
                        CsilInstr::ReadUserSlot { offset: 0, .. },
                        CsilInstr::JumpIfFalse { .. },
                    ]
                )
            })
            .expect("fixture must contain the actual composed sequence");
        assert_eq!(facts.operand(0, pc + 8, 0), Some(Tag::Is(Lane::Bool)));
        (program, facts, pc)
    };
    let check = |monitor: Monitor, outcome: &crate::RunOutcome| {
        assert_eq!(monitor.violation, None);
        assert!(monitor.cells > 0 && monitor.fixed_values > 0);
        let profile = outcome.profile.as_ref().expect("profile enabled");
        assert_eq!(
            monitor
                .entries_by_pc
                .iter()
                .map(|&count| count as f64)
                .collect::<Vec<_>>(),
            profile.count,
            "every logical instruction needs an observed entry, including fused tails"
        );
    };
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let (program, facts, pc) = prepare(ARRAY_MATH_STORE_READ);
    let data = long_feed(4);
    let (monitor, complete) = run_observed(&program, facts.clone(), &data, &options);
    check(monitor, &complete);
    assert!(complete.errors.is_empty(), "{:?}", complete.errors);
    assert_eq!(complete.bars_executed, 4);
    let profile = complete.profile.as_ref().unwrap();
    for tail in pc + 1..=pc + 8 {
        assert_eq!(profile.combined_dispatch_by_pc[tail], 4.0, "pc {tail}");
    }

    let data = long_feed(1);
    let (monitor, first) = run_observed(&program, facts.clone(), &data, &options);
    check(monitor, &first);
    assert!(first.errors.is_empty());
    let prefix_units = first.profile.as_ref().unwrap().units[..pc]
        .iter()
        .sum::<f64>() as u64;
    // Stop before each logical entry, including after math, the store and
    // its restored Boolean read. At eight units only the branch is deferred.
    for cut in 0..=9 {
        for per_run in [false, true] {
            let mut limited = options.clone();
            if per_run {
                limited.max_instructions_per_run = prefix_units + cut;
            } else {
                limited.max_instructions_per_bar = prefix_units + cut;
            }
            let (monitor, outcome) = run_observed(&program, facts.clone(), &data, &limited);
            check(monitor, &outcome);
            assert_eq!(outcome.errors.len(), 1);
            assert_eq!(
                outcome.errors[0].kind,
                if per_run {
                    crate::RuntimeErrorKind::RunInstructionLimitExceeded
                } else {
                    crate::RuntimeErrorKind::InstructionLimitExceeded
                }
            );
            assert_eq!(
                outcome.errors[0].instruction_offset,
                Some(pc as u32 + cut as u32)
            );
            let skipped = &outcome.profile.as_ref().unwrap().combined_dispatch_by_pc;
            assert_eq!(
                skipped[pc + 6],
                f64::from(u8::from(cut >= 8)),
                "store, cut {cut}"
            );
            assert_eq!(
                skipped[pc + 7],
                f64::from(u8::from(cut >= 8)),
                "read, cut {cut}"
            );
            assert_eq!(
                skipped[pc + 8],
                f64::from(u8::from(cut >= 9)),
                "branch, cut {cut}"
            );
        }
    }

    let failing = ARRAY_MATH_STORE_READ.replace("array.get(values, 0)", "array.get(values, 1)");
    let (program, facts, pc) = prepare(&failing);
    let (monitor, outcome) = run_observed(&program, facts, &data, &options);
    check(monitor, &outcome);
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(
        outcome.errors[0].kind,
        crate::RuntimeErrorKind::CollectionIndexOutOfBounds
    );
    assert_eq!(outcome.errors[0].instruction_offset, Some(pc as u32));
    assert!(
        outcome.profile.as_ref().unwrap().combined_dispatch_by_pc[pc + 1..=pc + 8]
            .iter()
            .all(|&count| count == 0.0)
    );
}

fn fixture_sources() -> Vec<(String, String)> {
    let mut sources = Vec::new();
    for dir in ["../../tests/compile-pass", "../../tests/regression"] {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .expect("fixture directory")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "pine"))
            .collect();
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path).expect("readable fixture");
            sources.push((path.display().to_string(), text));
        }
    }
    for (index, body) in MONITOR_SNIPPETS.iter().enumerate() {
        sources.push((
            format!("snippet {index}"),
            format!("//@version=6\nindicator(\"checker\")\n{body}"),
        ));
    }
    sources
}

#[test]
fn executed_stacks_carry_the_checked_tags() {
    let mut programs = 0;
    let mut totals = (0u64, 0u64, 0u64, 0usize);
    let mut failures = Vec::new();
    for (name, source) in fixture_sources() {
        let Some(program) = try_compile(&source) else {
            assert!(!name.starts_with("snippet"), "{name} does not compile");
            continue;
        };
        programs += 1;
        match run_with_monitor(program, |_| {}) {
            Ok((entries, cells, fixed, declared)) => {
                totals.0 += entries;
                totals.1 += cells;
                totals.2 += fixed;
                totals.3 += declared;
            }
            Err(violation) => failures.push(format!("{name}: {violation}")),
        }
    }
    if std::env::var_os("LANE_ROWS_PRINT").is_some() {
        println!(
            "checker: {programs} programs, {} declarations, {} instruction entries, {} cells, \
             {} non-na values under a fixed tag",
            totals.3, totals.0, totals.1, totals.2
        );
    }
    assert_eq!(failures, Vec::<String>::new());
    assert!(programs >= 50, "{programs} programs ran");
    assert!(
        totals.2 > 10_000,
        "the checker compared too few values: {totals:?}"
    );
}

/// The checker can fail: with one established tag replaced by a wrong one it
/// reports the cell.
#[test]
fn the_checker_reports_a_wrong_tag() {
    let program =
        compile("//@version=6\nindicator(\"tag check\")\nint n = bar_index + 1\nplot(n)\n");
    assert!(run_with_monitor(program.clone(), |_| {}).is_ok());
    let violation = run_with_monitor(program, |facts| {
        for stack in facts.stacks[0].iter_mut().flatten() {
            for tag in stack {
                if matches!(tag, Tag::Is(Lane::I64)) {
                    *tag = Tag::Is(Lane::F64);
                }
            }
        }
    })
    .expect_err("a wrong tag must be reported");
    assert!(
        violation.contains("the analysis says Is(F64)"),
        "{violation}"
    );
}
