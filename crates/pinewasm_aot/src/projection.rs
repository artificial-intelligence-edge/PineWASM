//! Conservative, standalone WebAssembly-only projection of host-owned PineWASM surfaces.
//!
//! Standalone WebAssembly deliberately has no PineWASM host.  This pass therefore does a
//! few narrowly-scoped things before standalone WebAssembly emission:
//!
//! * finite `input.string(options = [...])` declarations travel over standalone WebAssembly's
//!   existing integer parameter lane as option indexes, then become canonical
//!   constant-pool handles inside the module;
//! * computations proven to flow only into drawing/table sinks may be erased,
//!   but only when they cannot fail in the VM. Today that is the
//!   drawing lifecycle: `DeleteDrawing`, and the `NewObject` +
//!   `RegisterDrawing` pair a `*.new` call lowers to, under a program-level
//!   heap bound the artifact enforces per request (see [`certify`]). Table
//!   and string-formatting instructions can fail and are never erased; and
//! * same-symbol `request.security` reads, reads of the chart timeframe the
//!   request names, and calendar calls in a timezone a string literal names
//!   are answered by the module itself (`serve_security_sites`,
//!   `serve_chart_sites`).
//!
//! Ordinary [`crate::compile_program`] analysis and emission never consult
//! this data.  Dynamic strings, and any host value that can reach a signal,
//! remain host sites and make standalone WebAssembly compilation fail closed.

use crate::analysis::{Analysis, IntMinGuard, Term, VTy};
use crate::AotError;
use kernel::{FlowError, FlowInput, FlowOp, InstrClass, ResultCanon};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::{BinOp, BuiltinSeries, HtfExpr, InputKind, InputValue, PureFunc};
use pinewasm_ta::TaFunc;
use std::collections::{BTreeMap, BTreeSet};

mod kernel;
mod wf;

#[cfg(test)]
mod aeneas_refactor_tests;
/// Aeneas extraction roots for this pass (`--features aeneas` only).
#[cfg(feature = "aeneas")]
mod aeneas_roots_aot_projection;

/// One finite string input represented on the standalone WebAssembly wire as an integer index.
#[derive(Debug, Clone)]
pub(crate) struct StringOptions {
    /// Canonical `CsilProgram::constants` handles, in declared option order.
    pub handles: Vec<u32>,
    pub default_index: u32,
}

/// Facts consumed only by the standalone WebAssembly analyzer/schema/emitter.
#[derive(Debug, Clone, Default)]
pub(crate) struct StandaloneWasmProjection {
    pub string_inputs: BTreeMap<u32, StringOptions>,
    pub color_inputs: BTreeSet<u32>,
    pub canonical_string_cmp: BTreeSet<usize>,
    /// Host sites whose values/effects are proven chart-only.
    pub chart_only: BTreeSet<usize>,
    /// Signal-slice placeholders that carry no instruction-budget charge
    /// (see `slice::Sliced::budget_free`).
    pub budget_free: BTreeSet<usize>,
    /// Fraction-carry instructions the emitter checks for exactness (see
    /// `fraction::Carried::exact_guard`).
    pub exact_guard: BTreeSet<usize>,
    /// Same-symbol `request.security` reads the module resamples itself, in
    /// lane order. Plain series are shared between identical sites; each
    /// compound expression has its own entry and scratch lanes.
    pub security_specs: Vec<SecuritySpec>,
    /// `ReadSecurity`/`ReadSecurityExpr` site → the lane it reads, and a
    /// `time(timeframe)` call's site ([`Self::time_open_site`]).
    pub security_site: BTreeMap<usize, SecuritySite>,
    /// The served `CallPure(TimeOpen)` sites, with the length in seconds
    /// their timeframe literal names. Each drops its five operands and
    /// compares that length with the chart's (`body.rs`): a shorter
    /// timeframe reads `na`, the chart's own reads the bar's open time, and
    /// only a longer one reads its lane in [`Self::security_site`]. A blank
    /// literal (`""`, `" "`) is the chart's own period: 0 here, with no lane,
    /// and it always reads the bar's open time.
    pub time_open_site: BTreeMap<usize, i64>,
    /// Scratch cell index of the first request's status cell; request `k`
    /// uses cell `security_status_base + k`.
    pub security_status_base: u32,
    /// Chart-timeframe reads and fixed-timezone calendar calls the module
    /// answers itself ([`Self::serve_chart_sites`]).
    pub chart_site: BTreeMap<usize, ChartSite>,
    /// Scratch cell index of the first of the four cells the runtime writes
    /// the request's chart timeframe to: its length in seconds, its
    /// multiplier, its `timeframe.is*` bits, and what passing
    /// `timeframe.period` to `timeframe.in_seconds` adds to the call's budget
    /// charge. Meaningful when [`Self::reads_timeframe`].
    pub timeframe_cells: u32,
    /// The most bars one request may execute, when erased drawing
    /// constructors made the no-failure certificate depend on a heap bound
    /// ([`certify`]). The standalone entry point refuses a longer request.
    pub max_bars: Option<u32>,
}

/// A host read the module answers from the request's `timeframe` field, or
/// a calendar call in a fixed timezone it computes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartSite {
    /// `timeframe.multiplier`, a `timeframe.is*` predicate, `time_close` or
    /// `timenow`.
    Builtin(BuiltinSeries),
    /// A `timeframe.period` read whose only use is the `timeframe.in_seconds`
    /// call right after it: the module pushes a placeholder the call drops.
    PeriodArgument,
    /// `timeframe.in_seconds` of the chart's own period.
    PeriodSeconds,
    /// A calendar function (`year`, `dayofmonth`, `weekofyear`, ...) of a
    /// time in the timezone a string literal names.
    Calendar {
        /// The runtime's part code (`runtime/src/calendar.rs`, `part`).
        part: i32,
        /// Seconds east of UTC.
        offset: i64,
        /// What the call costs the VM beyond its dispatch unit: a
        /// unit per byte of the timezone literal after the first
        /// (`charge_pure_string_work`).
        extra: u64,
    },
    /// `timestamp(year, month, day, hour, minute, second)` in UTC, or
    /// `timestamp(timezone, ...)` with a fixed-offset timezone literal.
    Timestamp {
        /// Seconds east of UTC; 0 for the UTC form.
        offset: i64,
        /// What the call costs the VM beyond its dispatch unit, as
        /// for [`ChartSite::Calendar`]; 0 for the UTC form.
        extra: u64,
        /// Whether the call is the timezone form, whose literal sits under
        /// the six fields.
        zoned: bool,
    },
}

/// Where a `request.security` site finds its value and its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SecuritySite {
    /// The output lane's ordinal among the security lanes.
    pub lane: u32,
    /// Index into [`StandaloneWasmProjection::security_specs`].
    pub spec: u32,
}

/// One same-symbol `request.security` read standalone WebAssembly can serve
/// without a host: a
/// plain price series over fixed-width higher-timeframe buckets. The
/// runtime fills a lane per spec from the chart bars
/// (`runtime/src/standalone_wasm.rs`, `fill_security_lane`); a site reads its lane at
/// the current bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecuritySpec {
    /// Source code shared with the runtime (`security_source`).
    pub source: u8,
    pub timeframe: SecurityTimeframe,
    pub gaps_on: bool,
    pub lookahead_on: bool,
    /// A compound expression's postfix program (`runtime/src/standalone_wasm.rs`,
    /// `expr_op`); empty for a plain series.
    pub program: Vec<u8>,
    /// Scratch lanes after the output lane: one per ta slot, then one per
    /// history slot, each holding that slot's value per bucket.
    pub ta_slots: u32,
    pub history_slots: u32,
    /// For a `time(timeframe)` lane, the timeframe literal as written.
    /// The VM keeps a separate lane, and a separate one-time charge, for each
    /// distinct timeframe string, so `"D"` and `"1D"` (one length) stay apart.
    /// `None` for a `request.security` read.
    pub time_open: Option<String>,
}

/// Source code of a compound expression entry.
const SECURITY_SOURCE_EXPR: u8 = 9;
/// Source code of a `time(timeframe)` lane: each bar's bucket opening time,
/// the open time of the bucket's first bar (`runtime/src/standalone_wasm.rs`,
/// `security_source::TIME_OPEN`).
const SECURITY_SOURCE_TIME_OPEN: u8 = 10;
/// Limits of the runtime evaluator's fixed state.
const MAX_EXPR_SLOTS: u32 = 16;
const MAX_EXPR_STACK: u32 = 32;
/// A kind-4 payload is counted in 8-byte words in a one-byte length.
const MAX_SECURITY_PAYLOAD: usize = 255 * 8;

/// Where a request's timeframe comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SecurityTimeframe {
    /// Known at compile time, in seconds; 0 is the chart's own period.
    Fixed(i64),
    /// The selected option of a finite string input, resolved by the
    /// runtime from the input table on every evaluation. `seconds` has one
    /// entry per option: 0 for the chart's own period, negative for an
    /// option the module cannot serve (selecting it fails the evaluation).
    Input {
        input: u32,
        default_index: u32,
        seconds: Vec<i64>,
    },
}

/// The module's ceiling on a request timeframe (a hundred
/// leap years), which also fits the cell's four bytes of seconds.
const MAX_SECURITY_SECONDS: i64 = 100 * 366 * 86_400;
/// Marks an option the module cannot resample (a calendar week or month).
const UNSERVABLE_TIMEFRAME: i64 = -1;

/// Seconds of a timeframe string as the module can serve it: 0 for the empty
/// string (the chart's own period), [`UNSERVABLE_TIMEFRAME`] for calendar
/// weeks and months (they need the exchange timezone) and out-of-range
/// values. Otherwise the same reading as `pinewasm_vm::timeframe_in_seconds`:
/// a bare number is minutes, `S` seconds, `D` days, and any other unit letter one day.
pub fn servable_timeframe_seconds(timeframe: &str) -> i64 {
    // Only the empty string is the chart's own period; a timeframe that
    // trims to empty is read as one day.
    if timeframe.is_empty() {
        return 0;
    }
    let timeframe = timeframe.trim();
    if timeframe.is_empty() {
        return 86_400;
    }
    let (digits, unit) = match timeframe.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (
            &timeframe[..timeframe.len() - 1],
            Some(c.to_ascii_uppercase()),
        ),
        _ => (timeframe, None),
    };
    let count: i64 = digits.parse().unwrap_or(1).max(1);
    let seconds = match unit {
        None => count.saturating_mul(60),
        Some('S') => count,
        Some('D') => count.saturating_mul(86_400),
        Some('W' | 'M') => return UNSERVABLE_TIMEFRAME,
        Some(_) => 86_400,
    };
    if seconds > MAX_SECURITY_SECONDS {
        UNSERVABLE_TIMEFRAME
    } else {
        seconds
    }
}

/// How the module serves a `time(timeframe)` call ([`time_open_spec`]).
enum TimeOpen {
    /// A blank literal: the chart's own period, the bar's open time.
    ChartPeriod,
    /// A fixed-width literal: the open time of the bar's bucket, from a lane.
    Lane(SecuritySpec),
}

/// How the module serves a `time(timeframe)` call at `pc`, or `None`. Such a
/// call lowers to `CallPure(TimeOpen)` over a timeframe literal, an empty
/// session and timezone, and both `bars_back` arguments zero, pushed by the
/// five instructions before it in the same block. `timeframe.change` lowers onto exactly this call
/// (`pinewasm_semantics`, `analyze_timeframe_change`).
///
/// The literal must name a fixed width ([`servable_timeframe_seconds`]).
/// Its bucket is read as UTC, as for a `request.security` lane.
/// A literal that trims to nothing selects the chart's own period
/// (`TimeOpen::ChartPeriod`), which reads the bar's open time before any bucket.
/// A calendar week or month keeps `host.vm`, and so does a fixed width too
/// long for a lane.
fn time_open_spec(
    pc: usize,
    program: &CsilProgram,
    analysis: &Analysis,
    leaders: &BTreeSet<usize>,
) -> Option<TimeOpen> {
    let code = &program.main().code;
    if !matches!(
        code.get(pc),
        Some(CsilInstr::CallPure {
            func: PureFunc::TimeOpen,
            argc: 5,
        })
    ) || !analysis.host_site.get(pc).copied().unwrap_or(false)
        || analysis.operand_tys.get(pc).map(Vec::as_slice)
            != Some(&[VTy::Str, VTy::Str, VTy::Str, VTy::I64, VTy::I64][..])
        || analysis.result_tys.get(pc).map(Vec::as_slice) != Some(&[VTy::I64][..])
    {
        return None;
    }
    let first = pc.checked_sub(5)?;
    if (first + 1..=pc).any(|at| leaders.contains(&at)) {
        return None;
    }
    let constant = |at: usize| match code.get(at) {
        Some(CsilInstr::PushStr(handle)) => program.constants.get(*handle as usize),
        _ => None,
    };
    let timeframe = constant(first)?;
    if !constant(first + 1)?.is_empty()
        || !constant(first + 2)?.is_empty()
        || !matches!(code.get(first + 3), Some(CsilInstr::PushI64(0)))
        || !matches!(code.get(first + 4), Some(CsilInstr::PushI64(0)))
    {
        return None;
    }
    if timeframe.trim().is_empty() {
        return Some(TimeOpen::ChartPeriod);
    }
    let seconds = servable_timeframe_seconds(timeframe);
    if seconds <= 0 {
        return None;
    }
    Some(TimeOpen::Lane(SecuritySpec {
        source: SECURITY_SOURCE_TIME_OPEN,
        timeframe: SecurityTimeframe::Fixed(seconds),
        gaps_on: false,
        // Every bar of a bucket reads the bucket's own opening time.
        lookahead_on: true,
        program: Vec::new(),
        ta_slots: 0,
        history_slots: 0,
        time_open: Some(timeframe.clone()),
    }))
}

/// Whether the module answers builtin `source` from the request's chart
/// timeframe ([`ChartSite::Builtin`]).
fn chart_timeframe_builtin(source: BuiltinSeries) -> bool {
    use BuiltinSeries as B;
    matches!(
        source,
        B::TimeframeMultiplier
            | B::TimeframeIsseconds
            | B::TimeframeIsminutes
            | B::TimeframeIsdaily
            | B::TimeframeIsweekly
            | B::TimeframeIsmonthly
            | B::TimeframeIsdwm
            | B::TimeframeIsintraday
            | B::TimeframeIsticks
            | B::TimeClose
            | B::Timenow
    )
}

/// The runtime's part code for a calendar function
/// (`runtime/src/calendar.rs`, `part`), or `None` for any other call.
fn calendar_part(func: PureFunc) -> Option<i32> {
    use pinewasm_hir::CalendarField as F;
    Some(match func {
        PureFunc::Calendar(F::DayOfWeek) => 0,
        PureFunc::Calendar(F::DayOfMonth) => 1,
        PureFunc::Calendar(F::Month) => 2,
        PureFunc::Calendar(F::Year) => 3,
        PureFunc::Calendar(F::Hour) => 4,
        PureFunc::Calendar(F::Minute) => 5,
        PureFunc::Calendar(F::Second) => 6,
        PureFunc::WeekOfYear => 7,
        _ => return None,
    })
}

/// The constant handle of the string literal a call at `pc` takes as its
/// first of `argc` operands, or `None` unless one `PushStr` in the call's own
/// block pushed it.
///
/// The walk goes back from the call, tracking which stack cell holds that
/// operand: an instruction that pushed it ends the walk, and one that left it
/// alone moves it by what it popped and pushed. Nothing between the literal
/// and the call may be a jump target, where a cell could come from another
/// path. Any other producer (a `Dup`, a call, a read) gives `None`, so a
/// timezone chosen at run time keeps its host site.
fn pushed_literal(
    code: &[CsilInstr],
    analysis: &Analysis,
    leaders: &BTreeSet<usize>,
    pc: usize,
    argc: usize,
) -> Option<usize> {
    if leaders.contains(&pc) {
        return None;
    }
    // The operand's position from the top of the stack, 1 for the top.
    let mut depth = argc;
    for at in (0..pc).rev() {
        let pushed = analysis.result_tys.get(at)?.len();
        let popped = analysis.operand_tys.get(at)?.len();
        if depth <= pushed {
            return match &code[at] {
                CsilInstr::PushStr(handle) if pushed == 1 => Some(*handle as usize),
                _ => None,
            };
        }
        if leaders.contains(&at) {
            return None;
        }
        depth = depth - pushed + popped;
    }
    None
}

/// The runtime's code for a plain price series, or `None` for a series the
/// secondary context does not read.
fn security_source_code(source: BuiltinSeries) -> Option<u8> {
    Some(match source {
        BuiltinSeries::Open => 0,
        BuiltinSeries::High => 1,
        BuiltinSeries::Low => 2,
        BuiltinSeries::Close => 3,
        BuiltinSeries::Volume => 4,
        BuiltinSeries::Hl2 => 5,
        BuiltinSeries::Hlc3 => 6,
        BuiltinSeries::Ohlc4 => 7,
        BuiltinSeries::Hlcc4 => 8,
        _ => return None,
    })
}

/// Encodes a compound expression as the runtime's postfix program, tracking
/// the evaluation stack depth it needs.
struct ExprEncoder<'a> {
    program: &'a CsilProgram,
    out: Vec<u8>,
    depth: u32,
    max_depth: u32,
}

impl ExprEncoder<'_> {
    fn word(&mut self, op: u8, a: u8, b: u32, c: u32) -> Option<()> {
        let b = u16::try_from(b).ok()?;
        self.out.push(op);
        self.out.push(a);
        self.out.extend_from_slice(&b.to_le_bytes());
        self.out.extend_from_slice(&c.to_le_bytes());
        Some(())
    }

    fn pushed(&mut self) {
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
    }

    /// `None` when the expression uses something the runtime evaluator does
    /// not carry; the request then stays a host site.
    fn encode(&mut self, expr: &HtfExpr) -> Option<()> {
        match expr {
            HtfExpr::Const(value) => {
                // `NaN` is the evaluator's `na`; a literal NaN constant is refused here.
                if value.is_nan() {
                    return None;
                }
                self.word(0, 0, 0, 0)?;
                self.out.extend_from_slice(&value.to_le_bytes());
                self.pushed();
            }
            HtfExpr::Source(source) => {
                self.word(1, security_source_code(*source)?, 0, 0)?;
                self.pushed();
            }
            HtfExpr::InputSource { input } => {
                // Standalone WebAssembly pins every `input.source` to its
                // declared default.
                let decl = self.program.inputs.get(*input as usize)?;
                let InputValue::Source(source) = &decl.defval else {
                    return None;
                };
                self.word(1, security_source_code(*source)?, 0, 0)?;
                self.pushed();
            }
            HtfExpr::History {
                history_slot,
                offset,
                source,
            } => {
                self.encode(source)?;
                self.word(4, 0, *history_slot, *offset)?;
            }
            // Standalone WebAssembly evaluates history only: the realtime arm
            // never runs.
            HtfExpr::HistoryRealtime {
                history_slot,
                history_offset,
                source,
                ..
            } => {
                self.encode(source)?;
                self.word(4, 0, *history_slot, *history_offset)?;
            }
            HtfExpr::Neg(inner) => {
                self.encode(inner)?;
                self.word(2, 0, 0, 0)?;
            }
            HtfExpr::Binary { op, lhs, rhs, .. } => {
                self.encode(lhs)?;
                self.encode(rhs)?;
                let code = match op {
                    BinOp::Add => 0,
                    BinOp::Sub => 1,
                    BinOp::Mul => 2,
                    BinOp::Div => 3,
                    BinOp::Rem => 4,
                    BinOp::Lt => 5,
                    BinOp::Le => 6,
                    BinOp::Gt => 7,
                    BinOp::Ge => 8,
                    BinOp::Eq => 9,
                    BinOp::Ne => 10,
                    BinOp::And | BinOp::Or => 11,
                };
                self.word(3, code, 0, 0)?;
                self.depth -= 1;
            }
            HtfExpr::Ta {
                func,
                ta_slot,
                length,
                source,
            } => {
                if !(1..=100_000).contains(length) {
                    return None;
                }
                self.encode(source)?;
                self.word(5, ta_code(*func)?, *ta_slot, *length)?;
            }
            HtfExpr::TaInput {
                func,
                ta_slot,
                input,
                source,
                ..
            } => {
                let decl = self.program.inputs.get(*input as usize)?;
                if decl.kind != InputKind::Int {
                    return None;
                }
                self.encode(source)?;
                self.word(6, ta_code(*func)?, *ta_slot, *input)?;
            }
            // A boolean result, the secondary context's `barstate.*`, and a
            // history offset computed from inputs are not carried by the
            // runtime evaluator: the request stays a host site.
            HtfExpr::BoolResult(_)
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime
            | HtfExpr::HistoryInput { .. }
            | HtfExpr::EnumSelect { .. } => return None,
        }
        Some(())
    }
}

/// The runtime's code for a secondary-context `ta.*`. EMA, SMA and change
/// are evaluated by the module; the windowed functions (`ta.atr`, `ta.hma`,
/// `ta.vwma`) are not, so a request using one stays a host site.
fn ta_code(func: TaFunc) -> Option<u8> {
    match func {
        TaFunc::Ema => Some(0),
        TaFunc::Sma => Some(1),
        TaFunc::Change => Some(2),
        _ => None,
    }
}

impl SecuritySpec {
    /// Lanes this spec occupies: its output lane plus its scratch lanes.
    pub fn lanes(&self) -> u32 {
        1 + self.ta_slots + self.history_slots
    }

    /// The spec for a `ReadSecurity`/`ReadSecurityExpr` instruction, or
    /// `None` when the read needs something the module does not have:
    /// another symbol's feed, a calendar month (which needs the exchange
    /// timezone), a timeframe computed by an input expression, a timeframe
    /// input none of whose values can be served, or an expression beyond the
    /// runtime evaluator.
    fn of(
        instr: &CsilInstr,
        program: &CsilProgram,
        string_inputs: &BTreeMap<u32, StringOptions>,
    ) -> Option<Self> {
        match instr {
            CsilInstr::ReadSecurity {
                selector: None,
                source,
                htf_seconds,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on,
                lookahead_on,
            } => Some(Self {
                source: security_source_code(*source)?,
                timeframe: SecurityTimeframe::of(*htf_seconds, program, string_inputs)?,
                gaps_on: *gaps_on,
                lookahead_on: *lookahead_on,
                program: Vec::new(),
                ta_slots: 0,
                history_slots: 0,
                time_open: None,
            }),
            CsilInstr::ReadSecurityExpr {
                expr,
                htf_seconds,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on,
                lookahead_on,
            } => {
                let timeframe = SecurityTimeframe::of(*htf_seconds, program, string_inputs)?;
                let ta_slots = expr.ta_slot_count();
                let history_slots = expr.history_slot_count();
                if ta_slots > MAX_EXPR_SLOTS || history_slots > MAX_EXPR_SLOTS {
                    return None;
                }
                let mut encoder = ExprEncoder {
                    program,
                    out: Vec::new(),
                    depth: 0,
                    max_depth: 0,
                };
                encoder.encode(expr)?;
                if encoder.max_depth > MAX_EXPR_STACK {
                    return None;
                }
                let spec = Self {
                    source: SECURITY_SOURCE_EXPR,
                    timeframe,
                    gaps_on: *gaps_on,
                    lookahead_on: *lookahead_on,
                    program: encoder.out,
                    ta_slots,
                    history_slots,
                    time_open: None,
                };
                Some(spec)
            }
            _ => None,
        }
        .filter(|spec| spec.payload().len() <= MAX_SECURITY_PAYLOAD)
    }

    /// The eight key-table cell bytes the runtime decodes. `status_cell` is
    /// the scratch cell the runtime reports this lane's outcome in.
    pub fn cell(&self, status_cell: u16) -> [u8; 8] {
        let mut cell = [0u8; 8];
        cell[0] = self.source;
        cell[1] = u8::from(self.gaps_on) | (u8::from(self.lookahead_on) << 1);
        match &self.timeframe {
            SecurityTimeframe::Fixed(seconds) => {
                cell[2..6].copy_from_slice(&(*seconds as u32).to_le_bytes());
            }
            SecurityTimeframe::Input {
                input,
                default_index,
                ..
            } => {
                cell[1] |= 4;
                cell[2..4].copy_from_slice(&(*input as u16).to_le_bytes());
                cell[4..6].copy_from_slice(&(*default_index as u16).to_le_bytes());
            }
        }
        cell[6..8].copy_from_slice(&status_cell.to_le_bytes());
        cell
    }

    /// The entry's payload, a whole number of 8-byte words: a header word
    /// (option count and ta slot count, both LE u16), one LE i64 of seconds
    /// per option of an input-selected timeframe, then the expression
    /// program.
    pub fn payload(&self) -> Vec<u8> {
        let options: &[i64] = match &self.timeframe {
            SecurityTimeframe::Fixed(_) => &[],
            SecurityTimeframe::Input { seconds, .. } => seconds,
        };
        let mut payload = Vec::with_capacity(8 + options.len() * 8 + self.program.len());
        payload.extend_from_slice(&(options.len() as u16).to_le_bytes());
        payload.extend_from_slice(&(self.ta_slots as u16).to_le_bytes());
        payload.extend_from_slice(&[0u8; 4]);
        for seconds in options {
            payload.extend_from_slice(&seconds.to_le_bytes());
        }
        payload.extend_from_slice(&self.program);
        payload
    }
}

impl SecurityTimeframe {
    /// The timeframe a request's `htf_seconds` operand names, or `None`
    /// when the module cannot serve any value it may take.
    fn of(
        htf_seconds: i64,
        program: &CsilProgram,
        string_inputs: &BTreeMap<u32, StringOptions>,
    ) -> Option<Self> {
        if htf_seconds >= 0 {
            return (htf_seconds <= MAX_SECURITY_SECONDS).then_some(Self::Fixed(htf_seconds));
        }
        // `-(input_id + 1)` names a run-constant string input.
        let input = u32::try_from(-i128::from(htf_seconds) - 1).ok()?;
        let decl = program.inputs.get(input as usize)?;
        if decl.kind != InputKind::String || input > u32::from(u16::MAX) {
            return None;
        }
        if let Some(options) = string_inputs.get(&input) {
            let seconds: Vec<i64> = decl
                .constraints
                .options
                .iter()
                .map(|option| match option {
                    InputValue::String(value) => servable_timeframe_seconds(value),
                    _ => UNSERVABLE_TIMEFRAME,
                })
                .collect();
            if seconds.len() != options.handles.len() || seconds.iter().all(|value| *value < 0) {
                return None;
            }
            Some(Self::Input {
                input,
                default_index: options.default_index,
                seconds,
            })
        } else {
            // An unbounded string input has no wire lane, so the module
            // always runs its declared default.
            let InputValue::String(default) = &decl.defval else {
                return None;
            };
            let seconds = servable_timeframe_seconds(default);
            (seconds >= 0).then_some(Self::Fixed(seconds))
        }
    }
}

/// The VM heap budgets that a standalone artifact is checked against:
/// the run-wide live element cap (`RunOptions::max_collection_elements`,
/// baked into the artifact) and the live object cap
/// (`pinewasm_mir::heap::DEFAULT_MAX_LIVE_OBJECTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeapBudget {
    pub max_elements: u64,
    pub max_objects: u64,
}

impl Default for HeapBudget {
    fn default() -> Self {
        Self {
            max_elements: crate::heap_defaults::MAX_ELEMENTS,
            max_objects: crate::heap_defaults::MAX_OBJECTS,
        }
    }
}

impl StandaloneWasmProjection {
    /// Clone the program and intern every finite string option.  The source
    /// program is untouched, so ordinary CSAW and VM paths keep their
    /// exact input and string-table behavior.
    pub fn prepare(program: &CsilProgram) -> Result<(CsilProgram, Self), AotError> {
        let mut projected = program.clone();
        let mut out = Self::default();

        // Serialized/JS-supplied CSIL is validator-legal with duplicate string
        // contents at distinct constant-pool indexes.  The ordinary VM
        // compares their contents; standalone WebAssembly's canonical-string comparison uses
        // handles. Rewrite every PushStr in the projected clone to the first
        // handle for its content before proving handle equality.  Keeping the
        // pool itself intact preserves every externally-visible index while
        // ensuring no executed constant can carry a duplicate handle.
        let mut first_handle: BTreeMap<String, u32> = BTreeMap::new();
        let mut canonical_handle = Vec::with_capacity(projected.constants.len());
        for (index, value) in projected.constants.iter().enumerate() {
            let handle = *first_handle.entry(value.clone()).or_insert(index as u32);
            canonical_handle.push(handle);
        }
        let main = projected
            .functions
            .first_mut()
            .ok_or(AotError::NoFunctions)?;
        for instr in &mut main.code {
            if let CsilInstr::PushStr(handle) = instr {
                *handle = *canonical_handle.get(*handle as usize).ok_or_else(|| {
                    AotError::InvalidProgram(format!(
                        "string constant handle {handle} is out of range"
                    ))
                })?;
            }
        }

        for decl in &program.inputs {
            match decl.kind {
                InputKind::String if !decl.constraints.options.is_empty() => {
                    let InputValue::String(default) = &decl.defval else {
                        return Err(AotError::InvalidProgram(format!(
                            "string input {} has non-string default",
                            decl.id
                        )));
                    };
                    let mut handles = Vec::with_capacity(decl.constraints.options.len());
                    let mut default_index = None;
                    let mut seen = BTreeSet::new();
                    for (index, option) in decl.constraints.options.iter().enumerate() {
                        let InputValue::String(value) = option else {
                            return Err(AotError::InvalidProgram(format!(
                                "string input {} has a non-string option",
                                decl.id
                            )));
                        };
                        if !seen.insert(value.clone()) {
                            return Err(AotError::InvalidProgram(format!(
                                "string input {} has duplicate option {value:?}",
                                decl.id
                            )));
                        }
                        if value == default {
                            default_index = Some(index as u32);
                        }
                        let handle = match projected.constants.iter().position(|item| item == value)
                        {
                            Some(handle) => handle as u32,
                            None => {
                                projected.constants.push(value.clone());
                                (projected.constants.len() - 1) as u32
                            }
                        };
                        handles.push(handle);
                    }
                    let default_index = default_index.ok_or_else(|| {
                        AotError::InvalidProgram(format!(
                            "string input {} default {default:?} is not in its options",
                            decl.id
                        ))
                    })?;
                    out.string_inputs.insert(
                        decl.id,
                        StringOptions {
                            handles,
                            default_index,
                        },
                    );
                }
                InputKind::Color => {
                    out.color_inputs.insert(decl.id);
                }
                _ => {}
            }
        }

        Ok((projected, out))
    }

    /// Prove canonical comparisons and chart-only value flows, then remove
    /// only those sites from the host plan.  The original typed analysis is
    /// retained; projection changes where an already-typed opcode executes,
    /// not its stack contract.
    ///
    /// This wrapper only classifies instructions and looks up the input
    /// maps; the proofs themselves run in the index-only [`kernel`].
    pub fn finish(
        &mut self,
        program: &CsilProgram,
        analysis: &mut Analysis,
        budget: HeapBudget,
    ) -> Result<(), AotError> {
        // The classification: an instruction keeps its chart class
        // only when it cannot fail in the VM, so erasing it cannot turn a VM
        // error into a completed standalone run.
        let certified = certify(program, analysis, budget);
        self.finish_with(program, analysis, certified.classes)?;
        let code = &program.main().code;
        let erases_constructor = self
            .chart_only
            .iter()
            .any(|&pc| matches!(code[pc], CsilInstr::NewObject { .. }));
        self.max_bars = if erases_constructor {
            certified.max_bars
        } else {
            None
        };
        self.serve_security_sites(program, analysis);
        self.serve_chart_sites(program, analysis);
        Ok(())
    }

    /// Whether a served site reads the request's chart timeframe, which the
    /// request must then carry: the module's entry point then calls the
    /// runtime's `standalone_wasm_fill_timeframe`.
    pub fn reads_timeframe(&self) -> bool {
        // A served `time(timeframe)` compares its timeframe with the chart's.
        self.time_open_site.values().any(|seconds| *seconds > 0)
            || self.chart_site.values().any(|site| match site {
                ChartSite::Builtin(source) => *source != BuiltinSeries::TimeframeIsticks,
                ChartSite::PeriodArgument | ChartSite::PeriodSeconds => true,
                ChartSite::Calendar { .. } | ChartSite::Timestamp { .. } => false,
            })
    }

    /// Whether a served site calls the runtime's calendar function.
    pub fn calls_calendar(&self) -> bool {
        self.chart_site
            .values()
            .any(|site| matches!(site, ChartSite::Calendar { .. }))
    }

    /// Whether a served site calls the runtime's `timestamp` function.
    pub fn calls_timestamp(&self) -> bool {
        self.chart_site
            .values()
            .any(|site| matches!(site, ChartSite::Timestamp { .. }))
    }

    /// Reads of the chart timeframe, and calendar calls in a timezone a
    /// string literal names, stop being host sites. The request carries the
    /// timeframe (`timeframe`), and a calendar part of a time in a fixed
    /// timezone is integer arithmetic the runtime does the way the VM does.
    /// Runs after the chart-only proof, like the security sites.
    ///
    /// Served: `timeframe.multiplier`, the `timeframe.is*` predicates,
    /// `time_close` and `timenow`; `timeframe.in_seconds` of a
    /// `timeframe.period` read right before it, which is what the call with
    /// no argument compiles to; and `year`, `month`, `dayofmonth`,
    /// `dayofweek`, `hour`, `minute`, `second` and `weekofyear` of a time
    /// with a string literal right before the call as the timezone; and
    /// `timestamp` of six integer fields, in UTC or with a string literal as
    /// the timezone, wherever in the same block that literal was pushed. A
    /// `timeframe.period` used any other way stays a host site: the module
    /// has the request's timeframe as numbers, not as the string the
    /// VM would compare or print.
    fn serve_chart_sites(&mut self, program: &CsilProgram, analysis: &mut Analysis) {
        let code = &program.main().code;
        // A pair of instructions is one unit only when nothing jumps between
        // them, so the second consumes exactly what the first pushed.
        let leaders: BTreeSet<usize> = analysis.blocks.iter().map(|block| block.start).collect();
        let hosted = |pc: usize, analysis: &Analysis| {
            analysis.host_site.get(pc).copied().unwrap_or(false) && !self.chart_only.contains(&pc)
        };
        let typed = |pc: usize, operands: &[VTy], result: VTy, analysis: &Analysis| {
            analysis.operand_tys.get(pc).map(Vec::as_slice) == Some(operands)
                && analysis.result_tys.get(pc).map(Vec::as_slice) == Some(&[result][..])
        };
        let mut served = Vec::new();
        for (pc, instr) in code.iter().enumerate() {
            if !hosted(pc, analysis) {
                continue;
            }
            match instr {
                CsilInstr::ReadBuiltin { source, .. }
                | CsilInstr::ReadBuiltinDynamic { source }
                    if chart_timeframe_builtin(*source) =>
                {
                    served.push((pc, ChartSite::Builtin(*source)));
                }
                CsilInstr::ReadBuiltin {
                    source: BuiltinSeries::TimeframePeriod | BuiltinSeries::TimeframeMainPeriod,
                    offset: 0,
                } if matches!(
                    code.get(pc + 1),
                    Some(CsilInstr::CallPure {
                        func: PureFunc::TimeframeInSeconds,
                        argc: 1,
                    })
                ) && !leaders.contains(&(pc + 1))
                    && hosted(pc + 1, analysis)
                    && typed(pc + 1, &[VTy::Str], VTy::I64, analysis) =>
                {
                    served.push((pc, ChartSite::PeriodArgument));
                    served.push((pc + 1, ChartSite::PeriodSeconds));
                }
                CsilInstr::CallPure { func, argc: 2 }
                    if typed(pc, &[VTy::I64, VTy::Str], VTy::I64, analysis) =>
                {
                    let Some(part) = calendar_part(*func) else {
                        continue;
                    };
                    let timezone = match pc.checked_sub(1).and_then(|at| code.get(at)) {
                        Some(CsilInstr::PushStr(handle)) if !leaders.contains(&pc) => {
                            program.constants.get(*handle as usize)
                        }
                        _ => None,
                    };
                    // A fixed offset is read as a fixed UTC offset. An IANA
                    // name (whose clocks may change) or an unknown string
                    // keeps `host.vm`: the module carries no zone rules,
                    // and the VM stops on an unknown name.
                    let fixed = timezone.and_then(|tz| Some((tz, pinewasm_tz::parse_fixed(tz)?)));
                    if let Some((timezone, offset)) = fixed {
                        served.push((
                            pc,
                            ChartSite::Calendar {
                                part,
                                offset: i64::from(offset),
                                extra: timezone.len().saturating_sub(1) as u64,
                            },
                        ));
                    }
                }
                CsilInstr::CallPure {
                    func: PureFunc::Timestamp,
                    argc: 6,
                } if typed(pc, &[VTy::I64; 6], VTy::I64, analysis) => {
                    served.push((
                        pc,
                        ChartSite::Timestamp {
                            offset: 0,
                            extra: 0,
                            zoned: false,
                        },
                    ));
                }
                CsilInstr::CallPure {
                    func: PureFunc::TimestampTz,
                    argc: 7,
                } if typed(
                    pc,
                    &[
                        VTy::Str,
                        VTy::I64,
                        VTy::I64,
                        VTy::I64,
                        VTy::I64,
                        VTy::I64,
                        VTy::I64,
                    ],
                    VTy::I64,
                    analysis,
                ) =>
                {
                    let timezone = pushed_literal(code, analysis, &leaders, pc, 7)
                        .and_then(|handle| program.constants.get(handle));
                    // A fixed offset only, as for a calendar call.
                    let fixed = timezone.and_then(|tz| Some((tz, pinewasm_tz::parse_fixed(tz)?)));
                    if let Some((timezone, offset)) = fixed {
                        served.push((
                            pc,
                            ChartSite::Timestamp {
                                offset: i64::from(offset),
                                extra: timezone.len().saturating_sub(1) as u64,
                                zoned: true,
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
        for (pc, site) in served {
            self.chart_site.insert(pc, site);
            if let Some(host_site) = analysis.host_site.get_mut(pc) {
                *host_site = false;
            }
            // A host cell needed no guard of its own. A served time reads the
            // time lane, which is exact only below 2^53, so it takes the
            // guard `time` takes; every other served integer is a length, a
            // multiplier or a calendar part, never the `na` sentinel.
            match site {
                ChartSite::Builtin(BuiltinSeries::TimeClose | BuiltinSeries::Timenow) => {
                    analysis.int_min_guards.insert(pc, IntMinGuard::TimeLane);
                }
                _ if analysis.int_min_guards.get(&pc) == Some(&IntMinGuard::HostCell) => {
                    analysis.int_min_guards.remove(&pc);
                }
                _ => {}
            }
        }
    }

    /// Same-symbol requests read a lane the runtime fills, so they stop being
    /// host sites. Runs after the chart-only proof, which therefore still
    /// sees them as the host values they are everywhere else.
    fn serve_security_sites(&mut self, program: &CsilProgram, analysis: &mut Analysis) {
        let leaders: BTreeSet<usize> = analysis.blocks.iter().map(|block| block.start).collect();
        for (pc, instr) in program.main().code.iter().enumerate() {
            if self.chart_only.contains(&pc) {
                continue;
            }
            let (spec, is_time_open) = match time_open_spec(pc, program, analysis, &leaders) {
                Some(TimeOpen::ChartPeriod) => {
                    self.time_open_site.insert(pc, 0);
                    if let Some(host_site) = analysis.host_site.get_mut(pc) {
                        *host_site = false;
                    }
                    analysis.int_min_guards.insert(pc, IntMinGuard::TimeLane);
                    continue;
                }
                Some(TimeOpen::Lane(spec)) => (spec, true),
                None => match SecuritySpec::of(instr, program, &self.string_inputs) {
                    Some(spec) => (spec, false),
                    None => continue,
                },
            };
            // Identical plain requests share a lane; an expression keeps its
            // own, because its scratch lanes are its secondary context.
            let shared = if spec.program.is_empty() {
                self.security_specs.iter().position(|known| *known == spec)
            } else {
                None
            };
            let index = shared.unwrap_or_else(|| {
                self.security_specs.push(spec);
                self.security_specs.len() - 1
            });
            let lane: u32 = self.security_specs[..index]
                .iter()
                .map(SecuritySpec::lanes)
                .sum();
            self.security_site.insert(
                pc,
                SecuritySite {
                    lane,
                    spec: index as u32,
                },
            );
            if let Some(host_site) = analysis.host_site.get_mut(pc) {
                *host_site = false;
            }
            if let SecurityTimeframe::Fixed(seconds) = self.security_specs[index].timeframe {
                if is_time_open {
                    self.time_open_site.insert(pc, seconds);
                }
            }
            if is_time_open {
                // The host cell needed no guard of its own; a bucket opening
                // time is a bar's open time, exact only below 2^53, so it
                // takes the guard `time` takes.
                analysis.int_min_guards.insert(pc, IntMinGuard::TimeLane);
            }
        }
    }

    /// [`Self::finish`] with the per-pc classification `classes` (from
    /// [`certify`] in production; tests also run the [`instr_class`]
    /// rule and compare the kernel with it).
    fn finish_with(
        &mut self,
        program: &CsilProgram,
        analysis: &mut Analysis,
        classes: Vec<InstrClass>,
    ) -> Result<(), AotError> {
        let code = &program.main().code;
        // One class table feeds both the precondition check in `build` and
        // the chart-only proof below.
        let flow = FlowFacts::build(program, analysis, self, &classes)?;
        self.canonical_string_cmp = flow.canonical_string_cmp;

        // Chart producers are erased only when the use proof says every
        // downstream use is chart-only, and chart sinks and RegisterDrawing
        // are erased whenever they keep their class. Under `certify` no
        // instruction that can fail keeps a chart class, so every erased
        // site has a no-failure certificate and every other chart host site
        // stays a host site (standalone emission fails closed on it).
        let chart_only = kernel::chart_only_sites(&flow.uses, &classes);
        for (pc, &erase) in chart_only.iter().enumerate() {
            if erase {
                self.chart_only.insert(pc);
            }
        }

        let projected_input: Vec<bool> = code
            .iter()
            .map(|instr| match instr {
                CsilInstr::ReadInput { input } => {
                    self.string_inputs.contains_key(input) || self.color_inputs.contains(input)
                }
                _ => false,
            })
            .collect();
        let canonical_cmp: Vec<bool> = (0..code.len())
            .map(|pc| self.canonical_string_cmp.contains(&pc))
            .collect();
        let chart_only: Vec<bool> = (0..code.len())
            .map(|pc| self.chart_only.contains(&pc))
            .collect();
        kernel::erase_host_sites(
            &mut analysis.host_site,
            &projected_input,
            &canonical_cmp,
            &chart_only,
        );
        Ok(())
    }
}

/// The classification the chart-only proof runs on, and the
/// request bound it depends on.
pub(crate) struct Certified {
    /// Per pc: the instruction's [`instr_class`] when it has a no-failure
    /// certificate, otherwise [`InstrClass::Other`] (a real use, so neither
    /// it nor anything it reads is erased).
    pub classes: Vec<InstrClass>,
    /// The bar bound under which the `NewObject` certificates hold; `None`
    /// when no `NewObject` is certified.
    pub max_bars: Option<u32>,
}

/// No-failure certificates for erasure.
///
/// Erasing an instruction that can fail in the VM lets a standalone run
/// complete where the VM reports an error (for example an out-of-range
/// `table.cell`). With this classification the projection is strict: the
/// standalone run fails exactly where the VM does, with equal values. The
/// classification rests on one hypothesis: a certified pc never fails in the
/// VM. A certificate is about the operands a pc can
/// actually receive in an admitted run (validated, well-typed, and at most
/// `max_bars` bars): on those runs the VM is total at every certified pc,
/// which is that hypothesis for the VM restricted to admitted runs. The
/// certificates, against `pinewasm_vm::host_exec`:
///
/// * `Pop` never fails.
/// * `DeleteDrawing` (every `DrawKind`): pops its receiver (underflow is
///   validator-excluded), returns on `na`, and otherwise fails only on a
///   receiver that is neither `na` nor a reference. The analysis types the
///   operand `Ref`, whose producers yield only references or `na`; registry
///   removal is infallible, and a kind without a registry slot is a no-op.
/// * `RegisterDrawing`: fails only on a non-reference receiver or on its
///   push. Certified only right after a certified drawing `NewObject` in the
///   same basic block (the `*.new` lowering), so its operand is the fresh
///   reference that `NewObject` pushed; pop-then-push cannot overflow.
/// * `NewObject` (that constructor pattern only) fails on fuel, on its push,
///   or on the heap caps. The erased site charges the same fuel extra
///   (`field_count - 1`, see `emit_projected_chart_site`), so fuel runs out
///   at the same pc; with `field_count >= 1` its push follows a pop of at
///   least one field. The heap caps need a program-level argument, see
///   [`drawing_heap_bound`]. A constructor on a CFG cycle, or any other
///   allocating instruction in the program, withholds every `NewObject`
///   certificate.
///
/// String producers (`str.tostring`, `StrConcat`) can exhaust the string
/// byte budget, and table sinks can fail on their coordinates or receivers,
/// so they stay uncertified: a drawing whose field needs one keeps its
/// constructor as a host site and standalone emission fails closed.
pub(crate) fn certify(program: &CsilProgram, analysis: &Analysis, budget: HeapBudget) -> Certified {
    let code = &program.main().code;
    let n = code.len();
    let mut block_of = vec![usize::MAX; n];
    let mut block_start = vec![false; n];
    for (b, block) in analysis.blocks.iter().enumerate() {
        if block.start < n {
            block_start[block.start] = true;
        }
        for slot in block_of.iter_mut().take(block.end.min(n)).skip(block.start) {
            *slot = b;
        }
    }
    let on_cycle = blocks_on_cycles(&analysis.term, analysis.blocks.len());

    // The `*.new` lowering: `NewObject` immediately followed, in the same
    // basic block, by `RegisterDrawing`.
    let constructor = |pc: usize| {
        matches!(code[pc], CsilInstr::NewObject { field_count, .. } if field_count >= 1)
            && pc + 1 < n
            && matches!(code[pc + 1], CsilInstr::RegisterDrawing { .. })
            && !block_start[pc + 1]
    };
    let candidates: Vec<usize> = (0..n)
        .filter(|&pc| constructor(pc) && block_of[pc] != usize::MAX && !on_cycle[block_of[pc]])
        .collect();
    let other_allocation =
        (0..n).any(|pc| may_allocate(&code[pc]) && candidates.binary_search(&pc).is_err());
    let max_bars = if other_allocation {
        None
    } else {
        drawing_heap_bound(code, &candidates, budget)
    };

    let classes = (0..n)
        .map(|pc| {
            let certified = match &code[pc] {
                CsilInstr::Pop => true,
                CsilInstr::DeleteDrawing { .. } => analysis.operand_tys[pc] == [VTy::Ref],
                CsilInstr::NewObject { .. } => {
                    max_bars.is_some() && candidates.binary_search(&pc).is_ok()
                }
                CsilInstr::RegisterDrawing { .. } => {
                    pc > 0 && max_bars.is_some() && candidates.binary_search(&(pc - 1)).is_ok()
                }
                _ => false,
            };
            if certified {
                instr_class(&code[pc])
            } else {
                InstrClass::Other
            }
        })
        .collect();
    Certified { classes, max_bars }
}

/// The most bars a request may execute while the certified drawing
/// constructors at `sites` provably stay inside the VM heap caps, or `None`
/// when no bar is admissible (or there is nothing to bound).
///
/// Each site is off every CFG cycle, and every program that reaches
/// standalone emission runs its bar body exactly once per bar
/// (`calc_on_order_fills` and `calc_on_every_history_tick` are refused), so
/// each site allocates at most one object of `field_count` elements per bar.
/// The caller has checked that nothing else in the program allocates on the
/// VM heap. After `bars` bars at most `bars * sites.len()` objects and
/// `bars * sum(field_count)` elements were ever allocated, which bounds the
/// live counts whatever the VM's bar-boundary sweeps have or have not
/// collected yet. `Heap::alloc` fails only when the live object count is
/// already at its cap or the live elements would exceed theirs, so
/// `bars <= max_objects / sites` and `bars <= max_elements / fields` make
/// every allocation succeed.
fn drawing_heap_bound(code: &[CsilInstr], sites: &[usize], budget: HeapBudget) -> Option<u32> {
    if sites.is_empty() {
        return None;
    }
    let fields: u64 = sites
        .iter()
        .map(|&pc| match code[pc] {
            CsilInstr::NewObject { field_count, .. } => u64::from(field_count),
            _ => 0,
        })
        .sum();
    let by_objects = budget.max_objects / sites.len() as u64;
    let by_elements = budget.max_elements / fields.max(1);
    let bars = by_objects.min(by_elements).min(u64::from(u32::MAX));
    (bars > 0).then_some(bars as u32)
}

/// Whether `instr` may allocate on the VM heap. Deliberately
/// broad: any listed opcode other than a certified drawing constructor
/// withholds every `NewObject` certificate.
fn may_allocate(instr: &CsilInstr) -> bool {
    matches!(
        instr,
        CsilInstr::NewObject { .. }
            | CsilInstr::CopyObject { .. }
            | CsilInstr::CallCollection { .. }
            | CsilInstr::ReadDrawingsAll { .. }
            | CsilInstr::ReadSecurity { .. }
            | CsilInstr::ReadSecurityExpr { .. }
            | CsilInstr::ReadSecurityLowerTf { .. }
            | CsilInstr::ReadRequest { .. }
            | CsilInstr::BuildFootprint { .. }
            | CsilInstr::FootprintRowByPrice
            | CsilInstr::ReadPivotLevels { .. }
            | CsilInstr::CallPivotPointLevels { .. }
    )
}

/// Per block of the `blocks` whose terminators are `term`: whether it lies
/// on a cycle of the control-flow graph (it can reach itself). Computed from
/// the graph rather than from natural loops, so an irreducible cycle counts
/// too.
///
/// A block reaches itself exactly when it has an edge to itself or shares
/// its strongly connected component with another block. The components come
/// from one depth-first walk (Tarjan's algorithm, on an explicit stack), so
/// the cost is linear in the blocks and edges. A target outside the graph is
/// no edge.
fn blocks_on_cycles(term: &[Term], blocks: usize) -> Vec<bool> {
    const NONE: usize = usize::MAX;
    let successors = |b: usize| match term.get(b) {
        Some(Term::Branch { taken, fall }) => [*taken, *fall],
        Some(Term::Fall(next) | Term::Jump(next)) => [*next, NONE],
        Some(Term::Return) | None => [NONE, NONE],
    };
    let mut on_cycle = vec![false; blocks];
    // Discovery order, and the earliest block of the walk's path each block
    // reaches.
    let mut order = vec![NONE; blocks];
    let mut low = vec![0; blocks];
    let mut discovered = 0;
    // The blocks whose component is not complete yet, in discovery order.
    let mut open = Vec::new();
    let mut is_open = vec![false; blocks];
    // The walk's path: each block with the next of its successors to follow.
    let mut path: Vec<(usize, usize)> = Vec::new();
    for root in 0..blocks {
        if order[root] != NONE {
            continue;
        }
        path.push((root, 0));
        while let Some(&(b, edge)) = path.last() {
            if edge == 0 {
                order[b] = discovered;
                low[b] = discovered;
                discovered += 1;
                open.push(b);
                is_open[b] = true;
            }
            if edge < 2 {
                if let Some(frame) = path.last_mut() {
                    frame.1 = edge + 1;
                }
                let target = successors(b)[edge];
                if target >= blocks {
                    continue;
                }
                if target == b {
                    on_cycle[b] = true;
                } else if order[target] == NONE {
                    path.push((target, 0));
                } else if is_open[target] {
                    low[b] = low[b].min(order[target]);
                }
                continue;
            }
            path.pop();
            if let Some(&(parent, _)) = path.last() {
                low[parent] = low[parent].min(low[b]);
            }
            if low[b] == order[b] {
                // `b` is the first block of its component, which is `b` and
                // every block opened after it.
                let first = open.iter().rposition(|member| *member == b).unwrap_or(0);
                let shared = open.len() - first > 1;
                for member in open.drain(first..) {
                    is_open[member] = false;
                    on_cycle[member] |= shared;
                }
            }
        }
    }
    on_cycle
}

/// The chart-only proof's view of one instruction.
fn instr_class(instr: &CsilInstr) -> InstrClass {
    match instr {
        CsilInstr::Pop => InstrClass::Pop,
        CsilInstr::SetCell { .. }
        | CsilInstr::ClearCells
        | CsilInstr::MergeCells
        | CsilInstr::DeleteDrawing { .. } => InstrClass::ChartSink,
        CsilInstr::CallPure {
            func: PureFunc::StrTostring,
            ..
        }
        | CsilInstr::StrConcat
        | CsilInstr::NewObject { .. } => InstrClass::ChartProducer,
        CsilInstr::RegisterDrawing { .. } => InstrClass::RegisterDrawing,
        _ => InstrClass::Other,
    }
}

/// The data flow's view of one instruction.
fn flow_op(instr: &CsilInstr) -> FlowOp {
    match instr {
        CsilInstr::Dup => FlowOp::Dup,
        CsilInstr::ReadUserSlot { slot, .. } => FlowOp::ReadSlot(*slot as usize),
        CsilInstr::ReadUserSlotDynamic { slot } => FlowOp::ReadSlotDynamic(*slot as usize),
        CsilInstr::StoreUserSlotCurrent { slot } => FlowOp::StoreSlot(*slot as usize),
        _ => FlowOp::Generic,
    }
}

/// Canonical-string status of each result of `instr` (typed `result_tys`).
fn result_canons(
    instr: &CsilInstr,
    result_tys: &[VTy],
    projection: &StandaloneWasmProjection,
) -> Vec<ResultCanon> {
    result_tys
        .iter()
        .map(|ty| {
            if *ty != VTy::Str {
                return ResultCanon::No;
            }
            match instr {
                CsilInstr::PushStr(_) | CsilInstr::PushNa => ResultCanon::Yes,
                CsilInstr::ReadInput { input } if projection.string_inputs.contains_key(input) => {
                    ResultCanon::Yes
                }
                CsilInstr::Select => ResultCanon::SelectArms,
                _ => ResultCanon::No,
            }
        })
        .collect()
}

struct FlowFacts {
    /// Consumers of each producer pc, strictly increasing.
    uses: Vec<Vec<usize>>,
    canonical_string_cmp: BTreeSet<usize>,
}

impl FlowFacts {
    /// A small forward data-flow proof over the validated stack machine.
    /// Slot reads/writes and `dup` are transparent, so a producer's recorded
    /// consumers are its real semantic uses rather than storage plumbing.
    /// Tables are built here; the fixpoint runs in [`kernel::flow_fixpoint`].
    ///
    /// The tables must pass [`wf::kernel_input_ok`] (the preconditions of
    /// the kernel's proofs, for these tables and the per-pc `classes`);
    /// otherwise the projection fails closed, like a kernel `FlowError`.
    fn build(
        program: &CsilProgram,
        analysis: &Analysis,
        projection: &StandaloneWasmProjection,
        classes: &[InstrClass],
    ) -> Result<Self, AotError> {
        let code = &program.main().code;
        let (input, slot_canonical, cmp_candidate) = flow_tables(program, analysis, projection);
        if !wf::kernel_input_ok(&input, code.len(), &slot_canonical, &cmp_candidate, classes) {
            return Err(AotError::InvalidProgram(
                "standalone WebAssembly projection tables are not well formed".to_string(),
            ));
        }
        let mut state = kernel::flow_init(
            input.blocks.len(),
            code.len(),
            &slot_canonical,
            &cmp_candidate,
        );
        let cap = analysis.blocks.len() + code.len() + program.series_slots.len() + 4;
        kernel::flow_fixpoint(&input, &mut state, cap).map_err(flow_error)?;
        Ok(Self {
            uses: state.uses,
            canonical_string_cmp: state
                .cmp_ok
                .iter()
                .enumerate()
                .filter_map(|(pc, ok)| ok.then_some(pc))
                .collect(),
        })
    }
}

/// The kernel's input tables, and the per-slot canonical and per-pc
/// comparison flags, for `program` under `analysis`.
fn flow_tables(
    program: &CsilProgram,
    analysis: &Analysis,
    projection: &StandaloneWasmProjection,
) -> (FlowInput, Vec<bool>, Vec<bool>) {
    let code = &program.main().code;
    let ops: Vec<FlowOp> = code.iter().map(flow_op).collect();
    let pops: Vec<usize> = analysis.operand_tys.iter().map(Vec::len).collect();
    let results: Vec<Vec<ResultCanon>> = code
        .iter()
        .zip(&analysis.result_tys)
        .map(|(instr, tys)| result_canons(instr, tys, projection))
        .collect();
    // Every series slot begins as na.  It stays canonical exactly while
    // every value ever stored into a string slot is too.
    let slot_canonical: Vec<bool> = analysis.slot_ty.iter().map(|ty| *ty == VTy::Str).collect();
    let cmp_candidate: Vec<bool> = code
        .iter()
        .enumerate()
        .map(|(pc, instr)| {
            matches!(instr, CsilInstr::CmpEq | CsilInstr::CmpNe)
                && analysis.operand_tys[pc].first() == Some(&VTy::Str)
        })
        .collect();
    let mut rpo_order: Vec<usize> = (0..analysis.blocks.len())
        .filter(|&block| analysis.rpo[block] != usize::MAX)
        .collect();
    rpo_order.sort_by_key(|&block| analysis.rpo[block]);

    // Owned tables: Aeneas cannot join loop/branch contexts that hold a
    // struct of borrowed slices, so the kernel input owns copies.
    let input = FlowInput {
        ops,
        pops,
        results,
        blocks: analysis.blocks.clone(),
        terms: analysis.term.clone(),
        rpo_order,
    };
    (input, slot_canonical, cmp_candidate)
}

fn flow_error(error: FlowError) -> AotError {
    AotError::InvalidProgram(match error {
        FlowError::StackUnderflow(pc) => {
            format!("standalone WebAssembly projection stack underflow at instruction {pc}")
        }
        FlowError::MissingTerminator(block_id) => {
            format!("standalone WebAssembly projection has no terminator for block {block_id}")
        }
        FlowError::InconsistentDepth => {
            "standalone WebAssembly projection saw inconsistent stack depth".to_string()
        }
        FlowError::NoConvergence => {
            "standalone WebAssembly projection data-flow did not converge".to_string()
        }
    })
}

/// Differential tests for [`blocks_on_cycles`]: the one walk against a search
/// from every block, used as the expected result.
#[cfg(test)]
mod cycle_tests {
    use super::{blocks_on_cycles, Term};

    /// A direct per-block search, used as the expected result for these tests.
    fn reference(term: &[Term], nb: usize) -> Vec<bool> {
        let successors = |b: usize| -> Vec<usize> {
            match term.get(b) {
                Some(Term::Branch { taken, fall }) => vec![*taken, *fall],
                Some(Term::Fall(next) | Term::Jump(next)) => vec![*next],
                Some(Term::Return) | None => Vec::new(),
            }
        };
        (0..nb)
            .map(|b| {
                let mut seen = vec![false; nb];
                let mut work = successors(b);
                while let Some(next) = work.pop() {
                    if next == b {
                        return true;
                    }
                    if next < nb && !seen[next] {
                        seen[next] = true;
                        work.extend(successors(next));
                    }
                }
                false
            })
            .collect()
    }

    fn assert_same(term: &[Term], blocks: usize) -> Vec<bool> {
        let on_cycle = blocks_on_cycles(term, blocks);
        assert_eq!(on_cycle, reference(term, blocks), "{term:?} over {blocks}");
        on_cycle
    }

    #[test]
    fn shapes_with_known_answers() {
        // No block.
        assert_eq!(assert_same(&[], 0), Vec::<bool>::new());
        // A straight line.
        let line = [Term::Fall(1), Term::Jump(2), Term::Return];
        assert_eq!(assert_same(&line, 3), [false, false, false]);
        // A block that jumps to itself, then leaves.
        let own = [Term::Branch { taken: 0, fall: 1 }, Term::Return];
        assert_eq!(assert_same(&own, 2), [true, false]);
        // A loop with a preheader and an exit.
        let looped = [
            Term::Fall(1),
            Term::Branch { taken: 3, fall: 2 },
            Term::Jump(1),
            Term::Return,
        ];
        assert_eq!(assert_same(&looped, 4), [false, true, true, false]);
        // A diamond: two paths meet, nothing returns.
        let diamond = [
            Term::Branch { taken: 1, fall: 2 },
            Term::Jump(3),
            Term::Fall(3),
            Term::Return,
        ];
        assert_eq!(assert_same(&diamond, 4), [false; 4]);
        // An irreducible cycle: two entries into the pair `1 <-> 2`.
        let irreducible = [
            Term::Branch { taken: 1, fall: 2 },
            Term::Jump(2),
            Term::Branch { taken: 1, fall: 3 },
            Term::Return,
        ];
        assert_eq!(assert_same(&irreducible, 4), [false, true, true, false]);
        // A cycle no block before it reaches, found from its own root.
        let detached = [Term::Return, Term::Jump(2), Term::Jump(1)];
        assert_eq!(assert_same(&detached, 3), [false, true, true]);
        // Targets outside the graph, and fewer terminators than blocks.
        let ragged = [Term::Jump(7), Term::Branch { taken: 9, fall: 1 }];
        assert_eq!(assert_same(&ragged, 4), [false, true, false, false]);
        // Nested loops sharing a header.
        let nested = [
            Term::Fall(1),
            Term::Branch { taken: 2, fall: 5 },
            Term::Branch { taken: 3, fall: 4 },
            Term::Jump(2),
            Term::Jump(1),
            Term::Return,
        ];
        assert_eq!(
            assert_same(&nested, 6),
            [false, true, true, true, true, false]
        );
    }

    /// SplitMix64: a fixed stream per seed, with no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    #[test]
    fn deterministic_sweep_matches_reference() {
        let mut rng = Rng(0x00c0_ffee);
        let (mut graphs, mut cyclic) = (0, 0);
        for _ in 0..20_000 {
            let blocks = rng.below(24);
            // Sometimes fewer or more terminators than blocks, and targets a
            // little past the end: the function is total over both.
            let terms = (blocks + rng.below(3)).saturating_sub(rng.below(2));
            let bound = blocks + 2;
            // Mostly forward edges, so that many blocks are off every cycle.
            let forward = rng.below(4) != 0;
            let target = |rng: &mut Rng, from: usize| {
                if forward && rng.below(5) != 0 {
                    (from + 1 + rng.below(3)).min(bound - 1)
                } else {
                    rng.below(bound)
                }
            };
            let term: Vec<Term> = (0..terms)
                .map(|from| match rng.below(4) {
                    0 => Term::Return,
                    1 => Term::Fall(target(&mut rng, from)),
                    2 => Term::Jump(target(&mut rng, from)),
                    _ => Term::Branch {
                        taken: target(&mut rng, from),
                        fall: target(&mut rng, from),
                    },
                })
                .collect();
            let on_cycle = assert_same(&term, blocks);
            graphs += 1;
            cyclic += usize::from(on_cycle.contains(&true));
        }
        // Both answers have to be common for the sweep to mean anything.
        assert!(
            cyclic > graphs / 5,
            "{cyclic} of {graphs} graphs had a cycle"
        );
        assert!(
            cyclic < graphs * 4 / 5,
            "{cyclic} of {graphs} graphs had a cycle"
        );
    }

    #[test]
    fn a_long_chain_is_walked_once() {
        // Linear-time walk; this test checks a 200000-block chain.
        let blocks = 200_000;
        let mut term: Vec<Term> = (0..blocks).map(|b| Term::Fall(b + 1)).collect();
        term[blocks - 1] = Term::Jump(blocks / 2);
        let on_cycle = blocks_on_cycles(&term, blocks);
        assert!(on_cycle[..blocks / 2].iter().all(|flag| !flag));
        assert!(on_cycle[blocks / 2..].iter().all(|flag| *flag));
    }
}
