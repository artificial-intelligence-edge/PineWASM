//! Native lowering of the chart-timeframe reads and fixed-timezone calendar
//! calls a standalone projection serves (`projection::ChartSite`).
//!
//! The runtime writes the request's `timeframe` into four scratch cells
//! before the run (`standalone_wasm_fill_timeframe`): the nominal length in
//! seconds, the multiplier, one bit per `timeframe.is*` predicate, and the
//! budget the interpreter charges for passing `timeframe.period` to a call.
//! A read of the chart timeframe loads one of them. A calendar part is a
//! call into the runtime (`runtime/src/calendar.rs`), which computes it as
//! the interpreter does.
//!
//! A served call is charged what the interpreter charges for it: its
//! dispatch unit, then a unit per byte of its string argument after the
//! first (`charge_pure_string_work`), so a budget runs out at the same
//! instruction. For a calendar call that is the timezone literal's length,
//! known now; for `timeframe.in_seconds` of the chart's period it is the
//! trimmed timeframe's, which the runtime leaves in the fourth cell.

use super::{mem_f64, BodyEmitter, Loc, NA_I64, NA_STR};
use crate::analysis::{host_builtin_vty, VTy};
use crate::projection::ChartSite;
use crate::AotError;
use pinewasm_hir::BuiltinSeries;
use wasm_encoder::{BlockType, Instruction as I, MemArg, ValType};

/// The chart timeframe's scratch cells, from `timeframe_cells`.
const SECONDS: u32 = 0;
const MULTIPLIER: u32 = 1;
const FLAGS: u32 = 2;
const PERIOD_EXTRA: u32 = 3;

/// The bit of a `timeframe.is*` predicate in the flags cell
/// (`runtime/src/calendar.rs`, `flag`).
fn timeframe_flag(source: BuiltinSeries) -> Option<i64> {
    use BuiltinSeries as B;
    Some(match source {
        B::TimeframeIsseconds => 1 << 0,
        B::TimeframeIsminutes => 1 << 1,
        B::TimeframeIsdaily => 1 << 2,
        B::TimeframeIsweekly => 1 << 3,
        B::TimeframeIsmonthly => 1 << 4,
        B::TimeframeIsdwm => 1 << 5,
        B::TimeframeIsintraday => 1 << 6,
        _ => return None,
    })
}

impl BodyEmitter<'_, '_> {
    /// What the projection serves at `pc`, if anything.
    pub(super) fn chart_site(&self, pc: usize) -> Option<ChartSite> {
        self.ctx
            .projection
            .and_then(|projection| projection.chart_site.get(&pc).copied())
    }

    /// The lane type of a builtin read the projection serves at `pc`: the
    /// type the interpreter's host (`pinewasm_vm`) would have answered in.
    pub(super) fn chart_builtin_vty(&self, pc: usize, source: BuiltinSeries) -> Option<VTy> {
        match self.chart_site(pc)? {
            ChartSite::Builtin(_) | ChartSite::PeriodArgument => host_builtin_vty(source),
            ChartSite::PeriodSeconds | ChartSite::Calendar { .. } | ChartSite::Timestamp { .. } => {
                None
            }
        }
    }

    /// Push the chart timeframe's length in seconds.
    pub(super) fn timeframe_seconds_cell(&mut self) {
        self.timeframe_cell(SECONDS);
    }

    /// Push chart timeframe cell `cell`.
    fn timeframe_cell(&mut self, cell: u32) {
        let first = self
            .ctx
            .projection
            .map_or(0, |projection| projection.timeframe_cells);
        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.scratch_base))
            .instruction(&I::I64Load(MemArg {
                offset: u64::from(first + cell) * 8,
                align: 3,
                memory_index: 0,
            }));
    }

    /// The served builtin read at `pc` of the bar in `t32a`: one value of
    /// its lane type. `Ok(false)` when the projection serves nothing at `pc`.
    pub(super) fn emit_chart_read_at(&mut self, pc: usize) -> Result<bool, AotError> {
        use BuiltinSeries as B;
        let Some(site) = self.chart_site(pc) else {
            return Ok(false);
        };
        let l = self.ctx.locals;
        let g = self.ctx.globals;
        match site {
            // Only the call after it reads it, and that call drops it.
            ChartSite::PeriodArgument => {
                self.f.instruction(&I::I32Const(NA_STR));
            }
            ChartSite::Builtin(B::TimeframeMultiplier) => self.timeframe_cell(MULTIPLIER),
            // There are no tick timeframes in the interpreter either.
            ChartSite::Builtin(B::TimeframeIsticks) => {
                self.f.instruction(&I::I32Const(0));
            }
            // The bar's open plus the timeframe's nominal length, as the
            // interpreter computes it without a `timeClose` lane. The runtime
            // refused a request where that leaves `i64`.
            ChartSite::Builtin(B::TimeClose) => {
                self.f
                    .instruction(&I::GlobalGet(g.builtin_global(5)))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::F64Load(mem_f64()));
                self.emit_time_close(pc);
            }
            // `time_close` of the last bar, whatever bar reads it.
            ChartSite::Builtin(B::Timenow) => {
                self.f
                    .instruction(&I::GlobalGet(g.builtin_global(5)))
                    .instruction(&I::GlobalGet(g.bars))
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Sub)
                    .instruction(&I::I32Const(3))
                    .instruction(&I::I32Shl)
                    .instruction(&I::I32Add)
                    .instruction(&I::F64Load(mem_f64()));
                self.emit_time_close(pc);
            }
            ChartSite::Builtin(predicate) => {
                let bit = timeframe_flag(predicate).ok_or_else(|| {
                    AotError::InvalidProgram(format!(
                        "no chart timeframe lowering for {} at pc {pc}",
                        predicate.name()
                    ))
                })?;
                // bool3: 2 when the bit is set, else 0.
                self.timeframe_cell(FLAGS);
                self.f
                    .instruction(&I::I64Const(bit))
                    .instruction(&I::I64And)
                    .instruction(&I::I64Eqz)
                    .instruction(&I::I32Eqz)
                    .instruction(&I::I32Const(1))
                    .instruction(&I::I32Shl);
            }
            ChartSite::PeriodSeconds | ChartSite::Calendar { .. } | ChartSite::Timestamp { .. } => {
                return Err(AotError::InvalidProgram(format!(
                    "served call at pc {pc} lowered as a builtin read"
                )));
            }
        }
        Ok(true)
    }

    /// The bar open time on the stack (an f64 time-lane cell) plus the
    /// timeframe's length in milliseconds.
    fn emit_time_close(&mut self, pc: usize) {
        self.emit_time_lane_guard(pc);
        self.f.instruction(&I::I64TruncSatF64S);
        self.timeframe_cell(SECONDS);
        self.f
            .instruction(&I::I64Const(1000))
            .instruction(&I::I64Mul)
            .instruction(&I::I64Add);
    }

    /// Lower the call the projection serves at `pc`: `timeframe.in_seconds`
    /// of the chart's own period, a calendar part of a time in a fixed
    /// timezone, or a `timestamp` in UTC or a fixed timezone.
    pub(super) fn emit_chart_call(&mut self, pc: usize, site: ChartSite) -> Result<(), AotError> {
        let l = self.ctx.locals;
        match site {
            ChartSite::PeriodSeconds => {
                self.timeframe_cell(PERIOD_EXTRA);
                self.f.instruction(&I::LocalSet(l.ti3));
                self.emit_budget_charge_local(pc as u32);
                // The `timeframe.period` placeholder.
                self.discard_top();
                self.timeframe_cell(SECONDS);
                self.pushed(VTy::I64);
            }
            ChartSite::Calendar {
                part,
                offset,
                extra,
            } => {
                let calendar = self.ctx.hosts.calendar.ok_or_else(|| {
                    AotError::InvalidProgram(format!(
                        "calendar call at pc {pc} without the runtime's calendar function"
                    ))
                })?;
                if extra > 0 {
                    self.emit_budget_charge(extra, pc as u32);
                }
                // The timezone literal: its offset is already known.
                self.discard_top();
                // An `na` time is an `na` part.
                self.want(1);
                let valid = self.integer_valid_local(self.vstack.len() - 1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.ti0))
                    .instruction(&I::LocalGet(valid))
                    .instruction(&I::If(BlockType::Result(ValType::I64)))
                    .instruction(&I::I32Const(part))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Const(offset))
                    .instruction(&I::Call(calendar))
                    .instruction(&I::Else)
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::End);
                self.pushed(VTy::I64);
            }
            ChartSite::Timestamp {
                offset,
                extra,
                zoned,
            } => {
                let timestamp = self.ctx.hosts.timestamp.ok_or_else(|| {
                    AotError::InvalidProgram(format!(
                        "timestamp call at pc {pc} without the runtime's timestamp function"
                    ))
                })?;
                if extra > 0 {
                    self.emit_budget_charge(extra, pc as u32);
                }
                // The six fields, year first, and whether all are present:
                // one `na` field makes the time `na`, as in the interpreter.
                self.want(6);
                let first = self.vstack.len() - 6;
                let valid: Vec<u32> = (first..first + 6)
                    .map(|depth| self.integer_valid_local(depth))
                    .collect();
                self.f
                    .instruction(&I::I64Const(offset))
                    .instruction(&I::I32Const(i32::from(zoned)))
                    .instruction(&I::Call(timestamp))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::LocalGet(valid[0]));
                for &local in &valid[1..] {
                    self.f
                        .instruction(&I::LocalGet(local))
                        .instruction(&I::I32And);
                }
                self.f.instruction(&I::Select);
                for _ in 0..6 {
                    self.consumed();
                }
                if zoned {
                    // The timezone literal under the fields: its offset is
                    // already known.
                    self.f.instruction(&I::LocalSet(l.ti0));
                    self.discard_top();
                    self.f.instruction(&I::LocalGet(l.ti0));
                }
                self.pushed(VTy::I64);
            }
            ChartSite::Builtin(_) | ChartSite::PeriodArgument => {
                return Err(AotError::InvalidProgram(format!(
                    "served builtin read at pc {pc} lowered as a call"
                )));
            }
        }
        Ok(())
    }

    /// Drop the top operand, wherever it lives.
    pub(super) fn discard_top(&mut self) {
        if self.vstack.last().map(|cell| cell.1) == Some(Loc::Phys) {
            self.f.instruction(&I::Drop);
            self.consumed();
        } else {
            self.vstack.pop();
        }
    }
}
