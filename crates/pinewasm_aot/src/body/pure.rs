//! Native lowering for stable, side-effect-free builtin calls.
//!
//! Keeping this domain in a child module makes changes to numeric casts,
//! color construction, and `na` propagation reviewable without walking the
//! structured-control-flow, heap, strategy, or host-reroute emitter.

use super::{val_type, BodyEmitter, Loc, NA_COLOR, NA_I64};
use crate::analysis::{pure_plan, PurePlan, VTy};
use crate::{header, AotError};
use pinewasm_csil::CsilInstr;
use pinewasm_hir::PureFunc;
use wasm_encoder::{BlockType, Function, Instruction as I, MemArg, ValType};

impl BodyEmitter<'_, '_> {
    /// The color call at host site `pc` that has a packed-color lowering to
    /// try first ([`Self::emit_packed_color_or_host`]): a channel read,
    /// `color.new` or `color.rgb` of a module whose linked host keeps full
    /// colors. A module that imports its host has no such site: its colors
    /// are packed or the program is refused.
    pub(super) fn packed_color_call(&self, pc: usize) -> Option<(PureFunc, u8, PurePlan)> {
        if !self.ctx.analysis.linked_host {
            return None;
        }
        let CsilInstr::CallPure { func, argc } = self.ctx.program.main().code[pc] else {
            return None;
        };
        let plan = pure_plan(func, argc)?;
        matches!(
            plan,
            PurePlan::ColorChannel | PurePlan::ColorNew | PurePlan::ColorRgb
        )
        .then_some((func, argc, plan))
    }

    /// A color call that is a host site because the program's colors need
    /// not fit a packed cell (docs/aot.md §14): an operand may be a handle
    /// into the linked host's table, and the result may have to become one.
    /// Most calls of most runs meet neither. So the module tests the
    /// operands it has: when every color among them is a packed cell (or
    /// `na`) and the transparency the result takes is a whole percentage,
    /// it runs the lowering a packed-color module runs for this call, which
    /// returns the cell the host would. Otherwise it calls the host, as
    /// before.
    ///
    /// The two agree under the test, case by case
    /// (`pinewasm_vm::Vm::call_pure`, `color_cells::ColorCells::encode`):
    ///
    /// * a channel read takes its byte from a packed cell, and a packed
    ///   cell's transparency is its low byte;
    /// * `color.new` keeps the red, green and blue of its color, drops its
    ///   input origin, and takes the transparency clamped to 0..=100. A
    ///   whole transparency in that range (and an integer one of any size,
    ///   which the clamp makes whole) gives a color the host packs;
    /// * `color.rgb` rounds and clamps its channels as the packed lowering
    ///   does whatever their values, and its transparency as `color.new`
    ///   does.
    ///
    /// A float transparency that is fractional, a negative zero, outside
    /// 0..=100 or `na` goes to the host; so does a handle, and so does a
    /// cell that is neither a color nor `na`, which the host refuses.
    pub(super) fn emit_packed_color_or_host(
        &mut self,
        pc: usize,
        func: PureFunc,
        argc: u8,
        plan: PurePlan,
    ) -> Result<(), AotError> {
        let tys = self.ctx.analysis.operand_tys[pc].clone();
        let result = self
            .ctx
            .analysis
            .result_tys
            .get(pc)
            .and_then(|tys| tys.first().copied())
            .ok_or_else(|| AotError::InvalidProgram(format!("color call at pc {pc} is untyped")))?;
        if tys.len() != usize::from(argc) || self.vstack.len() < tys.len() {
            return Err(AotError::InvalidProgram(format!(
                "color call at pc {pc}: {} typed operands for {argc}",
                tys.len()
            )));
        }
        self.flush_all();
        let base = self.vstack.len() - tys.len();
        // Is the color at `depth` a packed cell or `na`? A packed cell's low
        // byte is its transparency, at most 100, and `na` is all ones. A
        // handle's low byte is 254. Any other cell is no color at all, and
        // the host is the one that says so.
        let packed = |this: &mut Self, depth: usize| {
            let cell = this.stack_local(depth, VTy::Color);
            this.f
                .instruction(&I::LocalGet(cell))
                .instruction(&I::I32Const(0xFF))
                .instruction(&I::I32And)
                .instruction(&I::I32Const(100))
                .instruction(&I::I32LeU)
                .instruction(&I::LocalGet(cell))
                .instruction(&I::I32Const(NA_COLOR))
                .instruction(&I::I32Eq)
                .instruction(&I::I32Or);
        };
        // Is the float at `depth` one of +0, 1, ..., 100?
        let whole_percentage = |this: &mut Self, depth: usize| {
            let t = this.stack_local(depth, VTy::F64);
            this.f
                .instruction(&I::LocalGet(t))
                .instruction(&I::F64Trunc)
                .instruction(&I::LocalGet(t))
                .instruction(&I::F64Eq)
                .instruction(&I::LocalGet(t))
                .instruction(&I::F64Const(100.0.into()))
                .instruction(&I::F64Le)
                .instruction(&I::I32And)
                // The sign bit is clear: not below zero, and not -0.
                .instruction(&I::LocalGet(t))
                .instruction(&I::I64ReinterpretF64)
                .instruction(&I::I64Const(0))
                .instruction(&I::I64GeS)
                .instruction(&I::I32And);
        };
        match (plan, tys.as_slice()) {
            (PurePlan::ColorChannel, [VTy::Color]) => packed(self, base),
            (PurePlan::ColorNew, [VTy::Color, transparency]) => {
                packed(self, base);
                if *transparency == VTy::F64 {
                    whole_percentage(self, base + 1);
                    self.f.instruction(&I::I32And);
                }
            }
            // The channels are exact in the packed lowering whatever they
            // hold, and an integer transparency is always whole.
            (PurePlan::ColorRgb, [_, _, _, VTy::F64]) => whole_percentage(self, base + 3),
            (PurePlan::ColorRgb, [_, _, _, _]) => {
                self.f.instruction(&I::I32Const(1));
            }
            _ => {
                return Err(AotError::InvalidProgram(format!(
                    "no packed lowering to try at pc {pc}"
                )))
            }
        }
        let entry = self.vstack.clone();
        self.f
            .instruction(&I::If(BlockType::Result(val_type(result))));
        self.emit_call_pure(pc, func, argc)?;
        let exit = self.vstack.clone();
        self.f.instruction(&I::Else);
        self.vstack = entry;
        self.emit_hosted_heap_charge(pc);
        self.emit_host_call(pc)?;
        if self.vstack != exit || exit.last() != Some(&(result, Loc::Phys)) {
            return Err(AotError::InvalidProgram(format!(
                "color call at pc {pc}: its two lowerings leave different stacks"
            )));
        }
        self.f.instruction(&I::End);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn emit_call_pure(
        &mut self,
        pc: usize,
        func: PureFunc,
        argc: u8,
    ) -> Result<(), AotError> {
        let l = self.ctx.locals;
        let plan = pure_plan(func, argc)
            .ok_or_else(|| AotError::UnsupportedOpcode(format!("CallPure({})", func.name())))?;
        match plan {
            PurePlan::HostF64 | PurePlan::Round2 => {
                // A direct call into the linked numeric runtime. It carries
                // the interpreter's na policy itself (na in → na out,
                // non-finite result → na), so the lowering is exactly the
                // call, with the same shape as a `host.pure` call but no
                // host round trip.
                let func_id = crate::host_ids::pure_host_id(func)
                    .ok_or_else(|| self.unsup(&CsilInstr::CallPure { func, argc }))?;
                let callee = self.ctx.hosts.pure_fn(func_id)?;
                // Stage args (≤ 2) as f64.
                debug_assert!((1..=2).contains(&argc));
                let stage = [l.tf0, l.tf1];
                if argc == 2 {
                    self.want(1);
                    let ty = self.consumed();
                    if ty == VTy::I64 {
                        self.emit_i64_to_f64_na();
                    }
                    self.f.instruction(&I::LocalSet(stage[1]));
                }
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(stage[0]))
                    .instruction(&I::LocalGet(stage[0]));
                if argc == 2 {
                    self.f.instruction(&I::LocalGet(stage[1]));
                } else {
                    // Unary entries ignore the second parameter; na keeps the
                    // call shape uniform.
                    self.f.instruction(&I::F64Const(f64::NAN.into()));
                }
                self.f.instruction(&I::Call(callee));
                self.pushed(VTy::F64);
            }
            PurePlan::InlineFloat => {
                self.want(1);
                self.consumed();
                match func {
                    PureFunc::MathSqrt => {
                        self.f.instruction(&I::F64Sqrt);
                    }
                    // Multiply then divide, NOT a multiply by a folded
                    // `180/pi`. Both round once, but at different points. This
                    // order rounds where `pinewasm_num::raw::to_degrees` does,
                    // the interpreter's copy. Emitted inline rather than
                    // through the linked runtime because it is still two
                    // instructions.
                    PureFunc::MathToDegrees => {
                        self.f
                            .instruction(&I::F64Const(180.0.into()))
                            .instruction(&I::F64Mul)
                            .instruction(&I::F64Const(std::f64::consts::PI.into()))
                            .instruction(&I::F64Div);
                        self.canonicalize_f64();
                    }
                    PureFunc::MathToRadians => {
                        self.f
                            .instruction(&I::F64Const(std::f64::consts::PI.into()))
                            .instruction(&I::F64Mul)
                            .instruction(&I::F64Const(180.0.into()))
                            .instruction(&I::F64Div);
                        self.canonicalize_f64();
                    }
                    _ => return Err(self.unsup(&CsilInstr::CallPure { func, argc })),
                }
                self.pushed(VTy::F64);
            }
            PurePlan::FloorCeilRound => {
                self.want(1);
                let ty = self.consumed();
                if ty == VTy::I64 {
                    // Identity on ints.
                    self.pushed(VTy::I64);
                    return Ok(());
                }
                match func {
                    PureFunc::MathFloor => {
                        self.f.instruction(&I::F64Floor);
                    }
                    PureFunc::MathCeil => {
                        self.f.instruction(&I::F64Ceil);
                    }
                    _ => self.emit_round_half_away(),
                }
                let result = self.ctx.analysis.result_tys[pc][0];
                if result == VTy::F64 {
                    self.emit_widened_integer_result();
                } else {
                    self.emit_f64_to_int();
                    self.emit_float_to_int_min_guard(pc);
                }
                self.pushed(result);
            }
            PurePlan::AbsSign => {
                self.want(1);
                let ty = self.consumed();
                match (func, ty) {
                    (PureFunc::MathAbs, VTy::F64) => {
                        self.f.instruction(&I::F64Abs);
                        self.pushed(VTy::F64);
                    }
                    (PureFunc::MathAbs, _) => {
                        // (x ^ (x >> 63)) - (x >> 63); MIN maps to MIN (na).
                        self.f
                            .instruction(&I::LocalTee(l.ti0))
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(63))
                            .instruction(&I::I64ShrS)
                            .instruction(&I::I64Xor)
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(63))
                            .instruction(&I::I64ShrS)
                            .instruction(&I::I64Sub);
                        self.pushed(VTy::I64);
                    }
                    (_, VTy::F64) => {
                        // sign: na → NaN; else 1.0 / -1.0 / 0.0
                        self.f
                            .instruction(&I::LocalSet(l.tf0))
                            .instruction(&I::F64Const(f64::NAN.into()))
                            // inner: select(1.0, select(-1.0, 0.0, x<0), x>0)
                            .instruction(&I::F64Const(1.0.into()))
                            .instruction(&I::F64Const((-1.0).into()))
                            .instruction(&I::F64Const(0.0.into()))
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Const(0.0.into()))
                            .instruction(&I::F64Lt)
                            .instruction(&I::Select)
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Const(0.0.into()))
                            .instruction(&I::F64Gt)
                            .instruction(&I::Select)
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Ne)
                            .instruction(&I::Select);
                        self.pushed(VTy::F64);
                    }
                    (_, _) => {
                        // int signum with sentinel preserved.
                        self.f
                            .instruction(&I::LocalSet(l.ti0))
                            .instruction(&I::I64Const(NA_I64))
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64GtS)
                            .instruction(&I::I64ExtendI32U)
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64LtS)
                            .instruction(&I::I64ExtendI32U)
                            .instruction(&I::I64Sub)
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(NA_I64))
                            .instruction(&I::I64Eq)
                            .instruction(&I::Select);
                        self.pushed(VTy::I64);
                    }
                }
            }
            PurePlan::MinMaxAvg => {
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                let all_int = tys.iter().all(|t| *t == VTy::I64);
                if matches!(func, PureFunc::MathAvg) || !all_int {
                    // Float fold. Promote any int operand at its position.
                    // Pop everything to stack locals first (mixed types).
                    self.flush_all();
                    let len = self.vstack.len();
                    let base = len - tys.len();
                    for (i, ty) in tys.iter().enumerate() {
                        self.f
                            .instruction(&I::LocalGet(self.stack_local(base + i, *ty)));
                        if *ty == VTy::I64 {
                            self.emit_i64_to_f64_na();
                        }
                        if i > 0 {
                            match func {
                                PureFunc::MathAvg => {
                                    self.f.instruction(&I::F64Add);
                                }
                                // wasm f64.min/max propagate NaN — exactly
                                // the any-na→na pre-check.
                                PureFunc::MathMin => {
                                    self.f.instruction(&I::F64Min);
                                }
                                _ => {
                                    self.f.instruction(&I::F64Max);
                                }
                            }
                        }
                    }
                    if matches!(func, PureFunc::MathAvg) {
                        self.f
                            .instruction(&I::F64Const(f64::from(tys.len() as u32).into()))
                            .instruction(&I::F64Div);
                        self.canonicalize_f64();
                    }
                    self.vstack.truncate(base);
                    self.pushed(VTy::F64);
                } else {
                    // All-int min/max: fold with selects; any sentinel → na.
                    self.flush_all();
                    let len = self.vstack.len();
                    let base = len - tys.len();
                    // acc = arg0
                    self.f
                        .instruction(&I::LocalGet(self.stack_local(base, VTy::I64)))
                        .instruction(&I::LocalSet(l.ti0));
                    // na accumulator in t32a
                    self.f
                        .instruction(&I::LocalGet(self.stack_local(base, VTy::I64)))
                        .instruction(&I::I64Const(NA_I64))
                        .instruction(&I::I64Eq)
                        .instruction(&I::LocalSet(l.t32a));
                    for i in 1..tys.len() {
                        let sl = self.stack_local(base + i, VTy::I64);
                        self.f
                            .instruction(&I::LocalGet(sl))
                            .instruction(&I::LocalSet(l.ti1))
                            // acc = select(acc, v, acc OP v)
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::LocalGet(l.ti1));
                        if matches!(func, PureFunc::MathMin) {
                            self.f.instruction(&I::I64LtS);
                        } else {
                            self.f.instruction(&I::I64GtS);
                        }
                        self.f
                            .instruction(&I::Select)
                            .instruction(&I::LocalSet(l.ti0))
                            .instruction(&I::LocalGet(l.t32a))
                            .instruction(&I::LocalGet(l.ti1))
                            .instruction(&I::I64Const(NA_I64))
                            .instruction(&I::I64Eq)
                            .instruction(&I::I32Or)
                            .instruction(&I::LocalSet(l.t32a));
                    }
                    self.f
                        .instruction(&I::I64Const(NA_I64))
                        .instruction(&I::LocalGet(l.ti0))
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::Select);
                    self.vstack.truncate(base);
                    self.pushed(VTy::I64);
                }
            }
            PurePlan::CastInt => {
                self.want(1);
                let ty = self.consumed();
                match ty {
                    // Wasm's saturating truncation is guarded by the same
                    // signed-i64 range check as the interpreter's
                    // `f64_to_int`; NaN and either infinity become na.
                    VTy::F64 if self.ctx.analysis.result_tys[pc][0] == VTy::F64 => {
                        self.f.instruction(&I::F64Trunc);
                        self.emit_widened_integer_result();
                    }
                    VTy::F64 => {
                        self.emit_carried_offset_guard(pc);
                        self.emit_f64_to_int();
                        self.emit_float_to_int_min_guard(pc);
                    }
                    VTy::I64 => {}
                    _ => return Err(self.unsup(&CsilInstr::CallPure { func, argc })),
                }
                self.pushed(self.ctx.analysis.result_tys[pc][0]);
            }
            PurePlan::CastFloat => {
                self.want(1);
                let ty = self.consumed();
                match ty {
                    VTy::I64 => self.emit_i64_to_f64_na(),
                    // `Value::from_f64` is the interpreter's last step even
                    // for an already-float operand; keep its non-finite
                    // canonicalization at this explicit cast boundary.
                    VTy::F64 => self.canonicalize_f64(),
                    _ => return Err(self.unsup(&CsilInstr::CallPure { func, argc })),
                }
                self.pushed(VTy::F64);
            }
            PurePlan::CastBool => {
                self.want(1);
                let ty = self.consumed();
                match ty {
                    VTy::Bool3 => {
                        // Only encoded true (2) survives. Both false (0) and
                        // Kleene na (1) become ordinary false, as bool(na)
                        // requires.
                        self.f.instruction(&I::I32Const(2)).instruction(&I::I32Eq);
                    }
                    VTy::I64 => {
                        self.f
                            .instruction(&I::LocalSet(l.ti0))
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(0))
                            .instruction(&I::I64Ne)
                            .instruction(&I::LocalGet(l.ti0))
                            .instruction(&I::I64Const(NA_I64))
                            .instruction(&I::I64Ne)
                            .instruction(&I::I32And);
                    }
                    VTy::F64 => {
                        // `x != 0` is true for NaN in Wasm, so explicitly
                        // require `x == x` as well.
                        self.f
                            .instruction(&I::LocalSet(l.tf0))
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Const(0.0.into()))
                            .instruction(&I::F64Ne)
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::LocalGet(l.tf0))
                            .instruction(&I::F64Eq)
                            .instruction(&I::I32And);
                    }
                    _ => return Err(self.unsup(&CsilInstr::CallPure { func, argc })),
                }
                self.f.instruction(&I::I32Const(1)).instruction(&I::I32Shl);
                self.pushed(VTy::Bool3);
            }
            PurePlan::StrategyTrade => {
                use crate::analysis::{strategy_trade_field, STRATEGY_TRADE_OPEN};
                use crate::runtime::broker as broker_ids;
                let (field, open) = match func {
                    PureFunc::StrategyOpenTrade(field) => (field, true),
                    PureFunc::StrategyClosedTrade(field) => (field, false),
                    _ => return Err(self.unsup(&CsilInstr::CallPure { func, argc })),
                };
                let (id, ty) = strategy_trade_field(field)
                    .ok_or_else(|| self.unsup(&CsilInstr::CallPure { func, argc }))?;
                let getter = self.ctx.hosts.broker_fn(match ty {
                    VTy::I64 => broker_ids::TRADE_I64,
                    _ => broker_ids::TRADE_F64,
                })?;
                let selector = id | if open { STRATEGY_TRADE_OPEN } else { 0 };
                // (field: i32, trade_num: i64); an na `trade_num` is the
                // i64 sentinel, which the blob reads as na.
                self.want(1);
                self.consumed();
                self.f
                    .instruction(&I::LocalSet(l.ti0))
                    .instruction(&I::I32Const(selector as i32))
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::Call(getter));
                self.pushed(ty);
            }
            PurePlan::Na => {
                self.want(1);
                let ty = self.consumed();
                self.emit_is_na(ty);
                self.f.instruction(&I::I32Const(1)).instruction(&I::I32Shl);
                self.pushed(VTy::Bool3);
            }
            PurePlan::Nz => {
                // nz(x, repl): select(repl, x, is_na(x))
                let ty = self.vstack[self.vstack.len() - 1].0;
                self.flush_all();
                let len = self.vstack.len();
                let repl_l = self.stack_local(len - 1, ty);
                let x_l = self.stack_local(len - 2, ty);
                self.vstack.truncate(len - 2);
                self.f
                    .instruction(&I::LocalGet(repl_l))
                    .instruction(&I::LocalGet(x_l))
                    .instruction(&I::LocalGet(x_l));
                self.emit_is_na(ty);
                self.f.instruction(&I::Select);
                self.pushed(ty);
            }
            PurePlan::ColorChannel => {
                self.want(1);
                self.consumed();
                let shift = match func {
                    PureFunc::ColorR | PureFunc::ColorRFloat => 24,
                    PureFunc::ColorG | PureFunc::ColorGFloat => 16,
                    PureFunc::ColorB | PureFunc::ColorBFloat => 8,
                    _ => 0,
                };
                self.f
                    .instruction(&I::LocalSet(l.t32a))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(shift))
                    .instruction(&I::I32ShrU)
                    .instruction(&I::I32Const(0xFF))
                    .instruction(&I::I32And)
                    .instruction(&I::I64ExtendI32U)
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::I32Eq)
                    .instruction(&I::Select);
                if func.color_channel_is_float() {
                    // Convert the integer NA sentinel before widening;
                    // an unavailable colour remains NaN, never a large float.
                    self.emit_i64_to_f64_na();
                    self.pushed(VTy::F64);
                } else {
                    self.pushed(VTy::I64);
                }
            }
            PurePlan::ColorNew => {
                // (color, numeric transp): na if either na; clamp t 0..=100.
                let transp_ty = self.ctx.analysis.operand_tys[pc][1];
                self.want(2);
                self.consumed();
                self.consumed();
                if transp_ty == VTy::F64 {
                    self.emit_f64_channel_to_i64();
                }
                self.f
                    .instruction(&I::LocalSet(l.ti0)) // transp
                    .instruction(&I::LocalSet(l.t32a)); // color
                self.emit_clamp_i64(l.ti0, 0, 100);
                self.f
                    .instruction(&I::I32Const(NA_COLOR))
                    // (c & !0xFF) | clamped
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(!0xFFi32))
                    .instruction(&I::I32And)
                    .instruction(&I::LocalGet(l.t32b))
                    .instruction(&I::I32Or)
                    // na cond
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::I32Eq)
                    .instruction(&I::LocalGet(l.ti0))
                    .instruction(&I::I64Const(NA_I64))
                    .instruction(&I::I64Eq)
                    .instruction(&I::I32Or)
                    .instruction(&I::Select);
                self.pushed(VTy::Color);
            }
            PurePlan::ColorRgb => {
                // (r, g, b, t numerics) → packed; na if any na. Float
                // channels convert with `pure_color_int` parity into the
                // (free) i64 stack local at the same depth first.
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                self.flush_all();
                let len = self.vstack.len();
                let base = len - 4;
                self.vstack.truncate(base);
                for (i, ty) in tys.iter().enumerate() {
                    if *ty == VTy::F64 {
                        self.f
                            .instruction(&I::LocalGet(self.stack_local(base + i, VTy::F64)));
                        self.emit_f64_channel_to_i64();
                        self.f
                            .instruction(&I::LocalSet(self.stack_local(base + i, VTy::I64)));
                    }
                }
                // na cond
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::LocalSet(l.t32a));
                for i in 0..4 {
                    self.f
                        .instruction(&I::LocalGet(self.stack_local(base + i, VTy::I64)))
                        .instruction(&I::I64Const(NA_I64))
                        .instruction(&I::I64Eq)
                        .instruction(&I::LocalGet(l.t32a))
                        .instruction(&I::I32Or)
                        .instruction(&I::LocalSet(l.t32a));
                }
                // packed value
                self.f.instruction(&I::I32Const(NA_COLOR));
                let (mut shift, mut first) = (24i32, true);
                for i in 0..4 {
                    let hi = if i == 3 { 100 } else { 255 };
                    self.f
                        .instruction(&I::LocalGet(self.stack_local(base + i, VTy::I64)));
                    self.f.instruction(&I::LocalSet(l.ti0));
                    self.emit_clamp_i64(l.ti0, 0, hi); // result in t32b
                    self.f.instruction(&I::LocalGet(l.t32b));
                    if shift > 0 {
                        self.f
                            .instruction(&I::I32Const(shift))
                            .instruction(&I::I32Shl);
                    }
                    if !first {
                        self.f.instruction(&I::I32Or);
                    }
                    first = false;
                    shift -= 8;
                }
                self.f
                    .instruction(&I::LocalGet(l.t32a))
                    .instruction(&I::Select);
                self.pushed(VTy::Color);
            }
            PurePlan::ColorFromGradient => {
                // (value, bottom_value, top_value, bottom, top) — exact
                // `color_from_gradient` math: frac = denom == 0 ? 1 :
                // clamp((v-bv)/denom, 0, 1); each channel lerps then rounds
                // half away from zero. Any na argument → na.
                let tys = self.ctx.analysis.operand_tys[pc].clone();
                self.flush_all();
                let len = self.vstack.len();
                let base = len - 5;
                self.vstack.truncate(base);
                for (i, ty) in tys.iter().take(3).enumerate() {
                    if *ty == VTy::I64 {
                        self.f
                            .instruction(&I::LocalGet(self.stack_local(base + i, VTy::I64)));
                        self.emit_i64_to_f64_na();
                        self.f
                            .instruction(&I::LocalSet(self.stack_local(base + i, VTy::F64)));
                    }
                }
                let (v_l, bv_l, tv_l) = (
                    self.stack_local(base, VTy::F64),
                    self.stack_local(base + 1, VTy::F64),
                    self.stack_local(base + 2, VTy::F64),
                );
                let bc_l = self.stack_local(base + 3, VTy::Color);
                let tc_l = self.stack_local(base + 4, VTy::Color);
                // na flag → wm_i (free outside WINDOW_MEAN).
                self.f
                    .instruction(&I::LocalGet(v_l))
                    .instruction(&I::LocalGet(v_l))
                    .instruction(&I::F64Ne)
                    .instruction(&I::LocalGet(bv_l))
                    .instruction(&I::LocalGet(bv_l))
                    .instruction(&I::F64Ne)
                    .instruction(&I::I32Or)
                    .instruction(&I::LocalGet(tv_l))
                    .instruction(&I::LocalGet(tv_l))
                    .instruction(&I::F64Ne)
                    .instruction(&I::I32Or)
                    .instruction(&I::LocalGet(bc_l))
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::I32Eq)
                    .instruction(&I::I32Or)
                    .instruction(&I::LocalGet(tc_l))
                    .instruction(&I::I32Const(NA_COLOR))
                    .instruction(&I::I32Eq)
                    .instruction(&I::I32Or)
                    .instruction(&I::LocalSet(l.wm_i));
                // frac → tf2. denom in tf1 (safe: round helper is not used
                // until after frac is consumed into blends... it IS — the
                // per-channel round clobbers tf0/tf1, so keep frac in tf2).
                self.f
                    .instruction(&I::LocalGet(tv_l))
                    .instruction(&I::LocalGet(bv_l))
                    .instruction(&I::F64Sub)
                    .instruction(&I::LocalTee(l.tf1))
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Eq)
                    .instruction(&I::If(BlockType::Result(ValType::F64)))
                    .instruction(&I::F64Const(1.0.into()))
                    .instruction(&I::Else)
                    .instruction(&I::LocalGet(v_l))
                    .instruction(&I::LocalGet(bv_l))
                    .instruction(&I::F64Sub)
                    .instruction(&I::LocalGet(l.tf1))
                    .instruction(&I::F64Div)
                    .instruction(&I::F64Const(1.0.into()))
                    .instruction(&I::F64Min)
                    .instruction(&I::F64Const(0.0.into()))
                    .instruction(&I::F64Max)
                    .instruction(&I::End)
                    .instruction(&I::LocalSet(l.tf2));
                // Packed accumulate, channel by channel (r,g,b,t).
                self.f.instruction(&I::I32Const(NA_COLOR));
                let mut first = true;
                for shift in [24i32, 16, 8, 0] {
                    let channel = |f: &mut Function, src: u32| {
                        f.instruction(&I::LocalGet(src));
                        if shift > 0 {
                            f.instruction(&I::I32Const(shift)).instruction(&I::I32ShrU);
                        }
                        f.instruction(&I::I32Const(0xFF))
                            .instruction(&I::I32And)
                            .instruction(&I::F64ConvertI32U);
                    };
                    // blended = lo + (hi - lo) * frac
                    channel(self.f, bc_l);
                    channel(self.f, tc_l);
                    channel(self.f, bc_l);
                    self.f
                        .instruction(&I::F64Sub)
                        .instruction(&I::LocalGet(l.tf2))
                        .instruction(&I::F64Mul)
                        .instruction(&I::F64Add);
                    self.emit_round_half_away();
                    self.f.instruction(&I::I32TruncSatF64S);
                    if shift > 0 {
                        self.f
                            .instruction(&I::I32Const(shift))
                            .instruction(&I::I32Shl);
                    }
                    if !first {
                        self.f.instruction(&I::I32Or);
                    }
                    first = false;
                }
                self.f
                    .instruction(&I::LocalGet(l.wm_i))
                    .instruction(&I::Select);
                self.pushed(VTy::Color);
            }
            PurePlan::RoundToMintick => {
                self.want(1);
                let ty = self.consumed();
                if ty == VTy::I64 {
                    self.emit_i64_to_f64_na();
                }
                // (v / mintick).round_half_away() * mintick, canonicalized.
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::F64Load(MemArg {
                        offset: u64::from(header::OFF_MINTICK),
                        align: 3,
                        memory_index: 0,
                    }))
                    .instruction(&I::F64Div);
                self.emit_round_half_away();
                self.f
                    .instruction(&I::I32Const(0))
                    .instruction(&I::F64Load(MemArg {
                        offset: u64::from(header::OFF_MINTICK),
                        align: 3,
                        memory_index: 0,
                    }))
                    .instruction(&I::F64Mul);
                self.canonicalize_f64();
                self.pushed(VTy::F64);
            }
        }
        Ok(())
    }
}
