//! Block-local bounds for nonnegative float-produced integers.
//! Bounds describe present finite values; missing values propagate separately
//! in the existing emitter. Unknown sources span the entire float domain.
use super::*;

type Bounds = (f64, f64);
const UNKNOWN: Bounds = (f64::NEG_INFINITY, f64::INFINITY);

pub(super) fn proven_ranges(
    program: &CsilProgram,
    blocks: &[Block],
    typed: &TypedOut,
) -> BTreeMap<usize, IntFact> {
    let mut proven = BTreeMap::new();
    for (block_id, block) in blocks.iter().enumerate() {
        let mut stack = vec![UNKNOWN; typed.entry_stack[block_id].len()];
        let mut slots = BTreeMap::new();
        for pc in block.start..block.end {
            let instr = &program.main().code[pc];
            let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
                return BTreeMap::new();
            };
            let pops = effect.pops as usize;
            if stack.len() < pops {
                return BTreeMap::new();
            }
            let base = stack.len() - pops;
            let args = &stack[base..];
            let bounds = match instr {
                CsilInstr::PushF64(x) if x.get().is_finite() => (x.get(), x.get()),
                CsilInstr::PushI64(x) => (*x as f64, *x as f64),
                CsilInstr::Dup => stack.last().copied().unwrap_or(UNKNOWN),
                CsilInstr::ReadUserSlot { slot, offset: 0 } => {
                    slots.get(slot).copied().unwrap_or(UNKNOWN)
                }
                CsilInstr::StoreUserSlotCurrent { slot } => {
                    slots.insert(*slot, args[0]);
                    UNKNOWN
                }
                CsilInstr::IntToF64 => args[0],
                CsilInstr::F64Mul if args[0].0 >= 0.0 && args[1].0 >= 0.0 => {
                    // Outward bounds: rounding and overflow cannot cross zero.
                    (0.0, f64::INFINITY)
                }
                CsilInstr::F64Div if args[0].0 >= 0.0 && args[1].0 > 0.0 => (0.0, f64::INFINITY),
                CsilInstr::CallPure { func, argc } => match func {
                    PureFunc::MathAbs => (0.0, f64::INFINITY),
                    PureFunc::MathMin | PureFunc::MathMax if *argc >= 2 => {
                        let fold = |a: f64, b: f64| {
                            if *func == PureFunc::MathMin {
                                a.min(b)
                            } else {
                                a.max(b)
                            }
                        };
                        args.iter()
                            .skip(1)
                            .fold(args[0], |a, b| (fold(a.0, b.0), fold(a.1, b.1)))
                    }
                    PureFunc::CastFloat => args[0],
                    PureFunc::MathFloor
                    | PureFunc::MathCeil
                    | PureFunc::MathRound
                    | PureFunc::CastInt
                        if *argc == 1
                            && typed.operand_tys[pc] == [VTy::F64]
                            && args[0].0 >= 0.0 =>
                    {
                        let hi = args[0].1.ceil();
                        let fact = if args[0].0 >= i64::MAX as f64 {
                            IntFact::NaOnly
                        } else {
                            // Any accepted integer is at most MAX. Saturating
                            // Rust conversion handles an infinite upper bound.
                            IntFact::Range(0, hi as i64)
                        };
                        if typed.result_tys[pc] == [VTy::I64] {
                            proven.insert(pc, fact);
                        }
                        (0.0, hi)
                    }
                    _ => UNKNOWN,
                },
                _ => UNKNOWN,
            };
            if matches!(instr, CsilInstr::Dup) {
                stack.push(bounds);
            } else {
                stack.truncate(base);
                stack.extend(std::iter::repeat_n(bounds, effect.pushes as usize));
            }
        }
    }
    proven
}
