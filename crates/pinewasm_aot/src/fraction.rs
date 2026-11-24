//! The fraction carry: run a kept Pine int quotient as float code.
//!
//! PineWASM types `int / int` as int, but an uneven quotient keeps its
//! fraction (`7 / 2` is `3.5`), and the fraction survives the statically-int
//! arithmetic that follows it. The interpreter holds such a value as a
//! float inside an int-typed cell. A typed `i64` lane cannot, so the typed
//! pass lowers only the shapes whose consumer hides the difference (an
//! immediate widening or `math.round`, a quotient slot, a fused comparison;
//! see `docs/aot.md`) and refuses the rest as
//! `UnsupportedOpcode("PineIntDiv")`.
//!
//! This pass is what the standalone compile and the linked chart-module tier
//! (`plan_native` in `lib.rs`) try after that refusal, and
//! only then: a program the typed pass accepts as written is never rewritten,
//! so its module does not change. It runs before typing. It finds every
//! value that can be reached from a kept quotient (through int arithmetic,
//! `?:`, `nz`/`math.abs`/`math.min`/`math.max`/`math.sign`, and series slots)
//! and rewrites that arithmetic into the float opcodes, widening the
//! whole-int values that meet it. Integers the quotient never reaches keep
//! their exact `i64` lane. A quotient that is widened, rounded or cast to an
//! integer by the very next instruction is left to the typed pass, which
//! lowers it exactly at any magnitude.
//!
//! What the rewrite preserves, against PineWASM's own interpreter:
//!
//! * a whole quotient and whole-int arithmetic stay exact while every value
//!   involved is below 2^53 in magnitude; the widening is exact there;
//! * a zero result is `+0.0`, as an int zero widens (each rewritten `*`,
//!   `/`, `%` and negation is followed by `+ 0.0`);
//! * a history offset truncates toward zero;
//! * `na`, a zero divisor and a non-finite result are `na`;
//! * the instruction budget: a rewritten opcode is charged like the one it
//!   replaces and every inserted instruction is free.
//!
//! What f64 cannot hold is every int at or beyond 2^53. A whole value there
//! is fine as long as it is exactly representable and each `+`, `-` and `*`
//! on two whole operands is exact, because the interpreter's int arithmetic
//! and its float arithmetic then agree (an even `/` of two exact operands is
//! always exact). The emitter checks
//! exactly that at every widened int and rewritten operation
//! ([`Carried::exact_guard`]) and stops the run (`INT_FRACTION_RANGE` in a
//! standalone module, the reroute status `ABI_MISMATCH` in a linked chart-module one)
//! only when a whole result would lose bits. One difference remains: where
//! the interpreter's float path yields `-0.0` (a fractional operand times
//! zero, say), this code yields `+0.0`.
//!
//! The pass fails closed. When a carried value reaches an instruction this
//! file does not list, it
//! returns `None` and the caller reports the typed pass's refusal of the
//! program as written.

use pinewasm_csil::flow::{effect, Flow, Producers};
use pinewasm_csil::fraction::{classify, Use};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use std::collections::BTreeSet;

/// A program whose kept quotients run as float code.
pub(crate) struct Carried {
    pub program: CsilProgram,
    /// Instructions the emitter must not charge: the ones this pass inserted
    /// and the caller's own free set, moved to their new positions.
    pub budget_free: BTreeSet<usize>,
    /// Instructions the emitter treats specially: every inserted widening
    /// of a whole int, and every rewritten operation. At a widening and at
    /// `+`, `-`, `*`, when two whole operands give a result f64 cannot hold
    /// exactly, the module stops the run instead of returning a value the
    /// interpreter would not. At every rewritten operation, `%` and negation
    /// included, an `na` result is made the canonical NaN.
    pub exact_guard: BTreeSet<usize>,
    /// For each instruction of `program`, the instruction of the program as
    /// written that it came from, so a refusal of the rewritten program can
    /// name a place the caller knows.
    pub origin: Vec<usize>,
    /// Instructions of `program` whose result is an int this pass turned
    /// into a float: a carried value, or a whole int widened where it meets
    /// one (the last instruction of each such group).
    pub float: BTreeSet<usize>,
    /// The `int(...)` this pass put before each history offset it carries
    /// as a float. The interpreter reads such an offset as a number below
    /// 2^64, and the cell the cast fills holds one below 2^63. An offset
    /// between the two is past every `max_bars_back`, so the interpreter
    /// stops the run there, and a linked chart module hands the run back
    /// (`BodyEmitter::emit_carried_offset_guard`).
    pub offset_casts: BTreeSet<usize>,
}

/// A quotient the carry leaves to the typed pass: its only consumer is the
/// next instruction, which either widens it or turns it into a genuine
/// integer (`math.round`, `math.floor`, `math.ceil`, `int()`). The typed pass
/// lowers both exactly, also beyond 2^53, where carrying the quotient would
/// stop the run. (The typed pass's other exact shapes, the quotient slot and
/// the fused comparison, are carried: in a program that needs the carry at
/// all, their values usually meet carried ones.)
fn fused(code: &[CsilInstr], pc: usize) -> bool {
    matches!(code.get(pc + 1), Some(CsilInstr::IntToF64))
        || crate::analysis::quotient_rounding(code, pc).is_some()
}

fn slot_of(instr: &CsilInstr) -> Option<usize> {
    match instr {
        CsilInstr::ReadUserSlot { slot, .. } | CsilInstr::ReadUserSlotDynamic { slot } => {
            Some(*slot as usize)
        }
        _ => None,
    }
}

/// The program with every kept quotient carried as a float, or `None` when
/// there is nothing to carry or the pass cannot prove its rewrite.
#[allow(clippy::too_many_lines)]
pub(crate) fn carry(program: &CsilProgram, budget_free: &BTreeSet<usize>) -> Option<Carried> {
    if program.functions.len() != 1 {
        return None;
    }
    let code = &program.main().code;
    if code.is_empty() || code.len() != program.main().source_map.len() {
        return None;
    }
    if !code
        .iter()
        .enumerate()
        .any(|(pc, instr)| matches!(instr, CsilInstr::PineIntDiv) && !fused(code, pc))
    {
        return None;
    }
    let flow = Flow::build(program)?;
    let n = code.len();

    // `carried[pc]`: the node's own result may hold a fraction.
    // `widened[pc]`: a whole-int result that meets one, converted on the spot.
    // Both run as f64 after the rewrite; `na` unifies with either and is
    // left alone.
    let mut carried = vec![false; n];
    let mut widened = vec![false; n];
    let mut slot_carried = vec![false; program.series_slots.len()];
    let is_na = |pc: usize| matches!(code[pc], CsilInstr::PushNa);
    loop {
        let mut changed = false;
        let float = |pc: &usize, carried: &[bool], widened: &[bool]| carried[*pc] || widened[*pc];
        for (pc, instr) in code.iter().enumerate() {
            // Unreachable code never runs and has no recorded operands.
            if !*flow.reached.get(pc)? {
                continue;
            }
            let operands = flow.operands.get(pc)?;
            let touched = |range: &[Producers], carried: &[bool], widened: &[bool]| {
                range
                    .iter()
                    .any(|producers| producers.iter().any(|p| float(p, carried, widened)))
            };
            // Which operands must agree, and whether the node itself carries.
            let (values, carries): (&[Producers], bool) = match classify(instr) {
                Use::Arithmetic => {
                    let own = matches!(instr, CsilInstr::PineIntDiv) && !fused(code, pc);
                    let carries = own || carried[pc] || touched(operands, &carried, &widened);
                    (operands.as_slice(), carries)
                }
                Use::Forward { first } => {
                    let values = operands.get(first..)?;
                    let carries = carried[pc] || touched(values, &carried, &widened);
                    (values, carries)
                }
                Use::Numeric | Use::Offset => (operands.as_slice(), false),
                // Formatting takes whatever number it is given; its other
                // operands (the format string) are not numeric.
                Use::Render => continue,
                // A consumer that reads the whole part: lowering gave it an
                // `int(...)`, so a carried value reaching it is unexpected.
                Use::Other | Use::Stored => {
                    if touched(operands, &carried, &widened) {
                        return None;
                    }
                    if let Some(slot) = slot_of(instr) {
                        if *slot_carried.get(slot)? && !carried[pc] {
                            carried[pc] = true;
                            changed = true;
                        }
                    }
                    continue;
                }
            };
            if let Some(slot) = slot_of(instr) {
                if *slot_carried.get(slot)? && !carried[pc] {
                    carried[pc] = true;
                    changed = true;
                }
            }
            if carries && !carried[pc] {
                carried[pc] = true;
                changed = true;
            }
            if carries || touched(values, &carried, &widened) {
                for producers in values {
                    for &producer in producers {
                        if !carried[producer] && !widened[producer] && !is_na(producer) {
                            widened[producer] = true;
                            changed = true;
                        }
                    }
                }
            }
        }
        for (slot, producers) in flow.slots.iter().enumerate() {
            if !producers
                .iter()
                .any(|p| float(p, &carried, &widened) || slot_carried[slot])
            {
                continue;
            }
            if !slot_carried[slot] {
                slot_carried[slot] = true;
                changed = true;
            }
            for &producer in producers {
                if !carried[producer] && !widened[producer] && !is_na(producer) {
                    widened[producer] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // A widened node must itself produce an int: a value this pass does not
    // understand in an int position means the proof does not hold.
    // A node first widened as a whole int and later found to read a carried
    // slot is already a float.
    for (widen, carry) in widened.iter_mut().zip(&carried) {
        *widen &= !carry;
    }
    for (pc, instr) in code.iter().enumerate() {
        if widened[pc] {
            let (_, pushes) = effect(program, instr)?;
            if pushes != 1 || matches!(instr, CsilInstr::PushF64(_)) {
                return None;
            }
        }
    }

    // An int-result engine whose result follows its source returns it
    // through an `i64` lane, which drops a fraction
    // (`analysis::int_result_follows_source`). The typed pass refuses a quotient
    // that reaches one; after the rewrite a carried value is no longer a
    // quotient it can see, so the same rule is applied here. The front end
    // widens the source first, so the value arrives through an `IntToF64`.
    for (pc, instr) in code.iter().enumerate() {
        let CsilInstr::CallTa { state } = instr else {
            continue;
        };
        let decl = program.ta_states.get(*state as usize)?;
        if !crate::analysis::int_result_follows_source(decl.func) || !*flow.reached.get(pc)? {
            continue;
        }
        let float = |value: &usize| carried[*value] || widened[*value];
        let carried_source = flow.operands.get(pc)?.iter().flatten().any(|producer| {
            float(producer)
                || (matches!(code[*producer], CsilInstr::IntToF64)
                    && flow.operands[*producer].iter().flatten().any(float))
        });
        if carried_source {
            return None;
        }
    }

    let mut free = BTreeSet::new();
    let mut exact_guard = BTreeSet::new();
    let mut float = BTreeSet::new();
    let mut offset_casts = BTreeSet::new();
    let mut new_code = Vec::with_capacity(n + 8);
    let mut new_map = Vec::with_capacity(n + 8);
    let mut pc_map = Vec::with_capacity(n + 1);
    let mut origin = Vec::with_capacity(n + 8);
    for (pc, instr) in code.iter().enumerate() {
        let span = program.main().source_map[pc];
        pc_map.push(u32::try_from(new_code.len()).ok()?);
        let mut push = |instr: CsilInstr, charged: bool, code: &mut Vec<CsilInstr>| {
            if !charged {
                free.insert(code.len());
            }
            code.push(instr);
            new_map.push(span);
            origin.push(pc);
        };
        let float_operand = flow.operands[pc]
            .iter()
            .any(|producers| producers.iter().any(|&p| carried[p] || widened[p]));
        if matches!(classify(instr), Use::Offset) && float_operand {
            offset_casts.insert(new_code.len());
            push(
                CsilInstr::CallPure {
                    func: PureFunc::CastInt,
                    argc: 1,
                },
                false,
                &mut new_code,
            );
        }
        let charged = !budget_free.contains(&pc);
        let zero_fix = match instr {
            _ if !carried[pc] => None,
            CsilInstr::IntAdd => Some((CsilInstr::F64Add, false)),
            CsilInstr::IntSub => Some((CsilInstr::F64Sub, false)),
            CsilInstr::IntMul => Some((CsilInstr::F64Mul, true)),
            CsilInstr::IntRem => Some((CsilInstr::F64Rem, true)),
            CsilInstr::IntNeg => Some((CsilInstr::F64Neg, true)),
            CsilInstr::PineIntDiv => Some((CsilInstr::F64Div, true)),
            _ => None,
        };
        match (instr, zero_fix) {
            // An unreachable quotient never runs; give the typed pass the
            // plain int opcode it accepts.
            (CsilInstr::PineIntDiv, None) if !flow.reached[pc] => {
                push(CsilInstr::IntDiv, charged, &mut new_code);
            }
            (_, Some((float_op, fix))) => {
                // Every rewritten operation is listed: `+ - * /` for the
                // exactness check, and all six so that an `na` result is the
                // canonical NaN the int operation produced.
                exact_guard.insert(new_code.len());
                push(float_op, charged, &mut new_code);
                if fix {
                    // `-0.0 + 0.0` is `+0.0`; every other value is unchanged.
                    push(CsilInstr::PushF64(0.0.into()), false, &mut new_code);
                    push(CsilInstr::F64Add, false, &mut new_code);
                }
            }
            // The operand is already a float: keep the conversion's charge
            // and stack shape without converting.
            (CsilInstr::IntToF64, None) if float_operand => {
                push(CsilInstr::PushF64(0.0.into()), charged, &mut new_code);
                push(CsilInstr::F64Add, false, &mut new_code);
            }
            _ => push(instr.clone(), charged, &mut new_code),
        }
        if widened[pc] {
            push(CsilInstr::IntToF64, false, &mut new_code);
            exact_guard.insert(new_code.len() - 1);
        }
        if carried[pc] || widened[pc] {
            float.insert(new_code.len() - 1);
        }
    }
    pc_map.push(u32::try_from(new_code.len()).ok()?);
    for instr in &mut new_code {
        match instr {
            CsilInstr::Jump { target }
            | CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => {
                *target = *pc_map.get(*target as usize)?;
            }
            _ => {}
        }
    }

    let mut rewritten = program.clone();
    let main = rewritten.functions.first_mut()?;
    main.code = new_code;
    main.source_map = new_map;
    // The zero fix pushes one constant above the original operands.
    main.max_stack = main.max_stack.checked_add(1)?;
    // As for the signal slice: the budget reference describes the code before
    // this rewrite, and the standalone tier never reads it.
    main.budget_reference = None;
    pinewasm_csil::validate(&rewritten).ok()?;
    Some(Carried {
        program: rewritten,
        budget_free: free,
        exact_guard,
        origin,
        float,
        offset_casts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_compiler::{compile, CompileOptions};

    fn program(body: &str) -> CsilProgram {
        let source = format!("//@version=6\nindicator(\"carry\")\nn = input.int(7, \"N\")\n{body}");
        let result = compile(&source, CompileOptions::all());
        assert!(
            !result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "{:?}",
            result.diagnostics
        );
        result.program.expect("program")
    }

    #[test]
    fn a_rewritten_program_charges_exactly_the_original_instructions() {
        for body in [
            "k = n / 2 + 1\nplot(k)\n",
            "var int acc = 0\nacc := acc + bar_index / 2\nplot(close[acc % 3])\n",
            "half = n / 2\nplot(bar_index % half == 0 ? 1 : -half)\n",
        ] {
            let original = program(body);
            let carried = carry(&original, &BTreeSet::new()).expect("carried");
            let code = &carried.program.main().code;
            assert!(
                !code.iter().any(|i| matches!(i, CsilInstr::PineIntDiv)),
                "{body}"
            );
            assert_eq!(
                code.len() - carried.budget_free.len(),
                original.main().code.len(),
                "{body}"
            );
        }
    }

    #[test]
    fn the_callers_free_instructions_stay_free_at_their_new_positions() {
        let original = program("k = n / 2 + 1\nplot(k)\n");
        let last = original.main().code.len() - 1;
        let free = BTreeSet::from([0, last]);
        let carried = carry(&original, &free).expect("carried");
        let code = &carried.program.main().code;
        assert!(carried.budget_free.contains(&0));
        assert!(carried.budget_free.contains(&(code.len() - 1)));
        assert_eq!(
            code.len() - carried.budget_free.len(),
            original.main().code.len() - free.len()
        );
    }

    #[test]
    fn every_rewritten_instruction_names_the_one_it_stands_for() {
        let original =
            program("var int acc = 0\nacc := acc + bar_index / 2\nplot(close[acc % 3])\n");
        let carried = carry(&original, &BTreeSet::new()).expect("carried");
        let main = carried.program.main();
        assert_eq!(carried.origin.len(), main.code.len());
        // The charged instructions are the original ones, in order.
        let charged: Vec<usize> = (0..main.code.len())
            .filter(|pc| !carried.budget_free.contains(pc))
            .map(|pc| carried.origin[pc])
            .collect();
        let written: Vec<usize> = (0..original.main().code.len()).collect();
        assert_eq!(charged, written);
        for (pc, &from) in carried.origin.iter().enumerate() {
            assert_eq!(main.source_map[pc], original.main().source_map[from]);
        }
        // Every value the carry turned into a float is marked, and nothing
        // else: the marks are float opcodes (or the zero fix after one),
        // inserted widenings, or reads of a carried slot.
        assert!(!carried.float.is_empty());
        for &pc in &carried.float {
            assert!(
                matches!(
                    main.code[pc],
                    CsilInstr::F64Add
                        | CsilInstr::F64Sub
                        | CsilInstr::IntToF64
                        | CsilInstr::ReadUserSlot { .. }
                ),
                "{:?}",
                main.code[pc]
            );
        }
    }

    #[test]
    fn quotients_the_typed_pass_lowers_exactly_are_left_alone() {
        for body in [
            "plot(n / 2)\n",
            "plot(math.round(n * 3 / 4))\n",
            "plot(int(n / 2) + 1)\n",
            "plot(math.floor(n / 2) * 2)\n",
            "plot(-math.ceil(n / 2))\n",
            "plot(n + 1)\n",
        ] {
            assert!(carry(&program(body), &BTreeSet::new()).is_none(), "{body}");
        }
    }

    #[test]
    fn an_integer_only_consumer_reads_an_explicit_whole_part() {
        // The front end hands an array index the whole part (an `int(...)`
        // after the quotient), so the carry never meets a consumer it does
        // not know, and the cast it leaves yields a plain int.
        let original = program("a = array.from(1, 2, 3, 4)\nplot(array.get(a, n / 2 + 0))\n");
        let cast = CsilInstr::CallPure {
            func: PureFunc::CastInt,
            argc: 1,
        };
        assert!(original.main().code.contains(&cast));
        let carried = carry(&original, &BTreeSet::new()).expect("carried");
        assert!(carried.program.main().code.contains(&cast));
    }

    #[test]
    fn a_carried_value_reaching_an_unknown_consumer_is_not_rewritten() {
        // Built by hand: lowering never leaves this shape. The quotient's
        // sum is replaced by a two-argument pure call this pass does not
        // list, so a carried value reaches a consumer it cannot vouch for.
        let mut original = program("k = n / 2 + 1\nplot(k)\n");
        let main = &mut original.functions[0];
        let add = main
            .code
            .iter()
            .position(|instr| matches!(instr, CsilInstr::IntAdd))
            .expect("int add");
        main.code[add] = CsilInstr::CallPure {
            func: PureFunc::MathPow,
            argc: 2,
        };
        assert!(carry(&original, &BTreeSet::new()).is_none());
    }
}
