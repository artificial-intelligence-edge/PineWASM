//! Select the literal implementation of compiler-owned search guards for a
//! target without the Rust heap service. The capability guard is not a Pine
//! value: only its structured guard/kernel role is recognized here. An
//! unfamiliar use retains the ordinary standalone refusal.

use pinewasm_csil::{CollectionFunc as C, CsilInstr as I, CsilProgram};
use std::collections::{BTreeMap, BTreeSet};

fn target(i: &I) -> Option<usize> {
    match i {
        I::Jump { target }
        | I::JumpIfFalse { target }
        | I::JumpIfExact { target, .. }
        | I::JumpIfSlotWritten { target, .. } => Some(*target as usize),
        _ => None,
    }
}

pub(crate) struct LiteralProgram {
    pub program: CsilProgram,
    /// Rewritten instruction to the supplied instruction, including placeholders.
    pub origin: Vec<usize>,
}

pub(crate) fn literal_program(program: &CsilProgram) -> Option<LiteralProgram> {
    if program.functions.len() != 1 {
        return None;
    }
    let function = program.main();
    let code = &function.code;
    // Include every explicit and fall-through edge, even unreachable ones:
    // validated cached CSIL need not have the compiler's single-entry blocks.
    let mut predecessors = vec![Vec::new(); code.len()];
    predecessors.first_mut()?.push(usize::MAX); // function entry
    for (from, instruction) in code.iter().enumerate() {
        if let Some(to) = target(instruction) {
            predecessors.get_mut(to)?.push(from);
        }
        if !matches!(instruction, I::Jump { .. } | I::Return) && from + 1 < code.len() {
            predecessors[from + 1].push(from);
        }
    }
    let mut replacements = BTreeMap::new();
    let mut paired_scans = BTreeSet::new();
    for (pc, instruction) in code.iter().enumerate() {
        if !matches!(
            instruction,
            I::CallCollection {
                func: C::InternalDistanceScanEligible,
                argc: 12
            }
        ) {
            continue;
        }
        let I::JumpIfFalse { target: otherwise } = code.get(pc + 1)? else {
            return None;
        };
        let otherwise = *otherwise as usize;
        if otherwise >= code.len() {
            return None;
        }
        let true_edge = pc + 2;
        let first = match code.get(true_edge)? {
            I::Jump { target } => *target as usize,
            _ => true_edge,
        };
        if first <= pc + 1 || first >= code.len() {
            return None;
        }
        let mut at = first;
        let mut scan = None;
        while at < code.len() {
            match &code[at] {
                I::CallCollection {
                    func: C::InternalDistanceScan,
                    argc: 10,
                } if scan.is_none() => {
                    scan = Some(at);
                    at += 1;
                    break;
                }
                I::ReadUserSlot { offset: 0, .. }
                | I::ReadInput { .. }
                | I::PushNa
                | I::IntToF64
                | I::GetField { .. }
                | I::PushI64(_)
                | I::PushF64(_) => at += 1,
                _ => return None,
            }
        }
        let scan = scan?;
        if !matches!(code.get(at)?, I::StoreUserSlotCurrent { .. }) {
            return None;
        }
        at += 1;
        let (join, end) = match code.get(at)? {
            I::Jump { target } => (*target as usize, at + 1),
            _ => (at, at), // the then arm may fall through to its join
        };
        if join >= code.len() || (first..end).contains(&join) || (first..end).contains(&otherwise) {
            return None;
        }
        // RPO may put the join before either arm. Prove the only route to
        // the neutralized kernel is this guard's true edge, including entry
        // to the conditional itself, a jump trampoline, and fall-through.
        if predecessors[pc + 1] != [pc] {
            return None;
        }
        let expected_entry = if first == true_edge {
            pc + 1
        } else {
            if predecessors[true_edge] != [pc + 1] {
                return None;
            }
            true_edge
        };
        if predecessors[first] != [expected_entry]
            || (first + 1..end).any(|at| predecessors[at] != [at - 1])
        {
            return None;
        }
        if !paired_scans.insert(scan) {
            return None;
        }
        replacements.insert(pc, (12usize, I::PushBool(false)));
        replacements.insert(scan, (10usize, I::PushNa));
    }
    if replacements.is_empty() {
        return None;
    }
    // Do not invent a lowering for standalone callers of an unguarded kernel,
    // a guard exposed as a value, or an unfamiliar compiler macro shape.
    for (pc, instruction) in code.iter().enumerate() {
        if matches!(
            instruction,
            I::CallCollection {
                func: C::InternalDistanceScan | C::InternalDistanceScanEligible,
                ..
            }
        ) && !replacements.contains_key(&pc)
        {
            return None;
        }
    }
    let mut next = Vec::new();
    let mut spans = Vec::new();
    let mut origin = Vec::new();
    let mut map = Vec::with_capacity(code.len() + 1);
    for (pc, instruction) in code.iter().enumerate() {
        map.push(u32::try_from(next.len()).ok()?);
        if let Some((pops, value)) = replacements.get(&pc) {
            next.extend(std::iter::repeat_n(I::Pop, *pops));
            spans.extend(std::iter::repeat_n(function.source_map[pc], *pops));
            origin.extend(std::iter::repeat_n(pc, *pops));
            next.push(value.clone());
        } else {
            next.push(instruction.clone());
        }
        spans.push(function.source_map[pc]);
        origin.push(pc);
    }
    map.push(u32::try_from(next.len()).ok()?);
    for instruction in &mut next {
        match instruction {
            I::Jump { target }
            | I::JumpIfFalse { target }
            | I::JumpIfExact { target, .. }
            | I::JumpIfSlotWritten { target, .. } => *target = *map.get(*target as usize)?,
            _ => {}
        }
    }
    let mut result = program.clone();
    result.functions[0].code = next;
    result.functions[0].source_map = spans;
    // A target implementation selection is a new lowering, not an erasure
    // certificate for the indexed program's finite-fuel execution.
    result.functions[0].budget_reference = None;
    pinewasm_csil::validate(&result).ok()?;
    Some(LiteralProgram {
        program: result,
        origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_compiler::{compile, CompileOptions};
    use pinewasm_vm::{run, MarketData, RunOptions};

    #[test]
    fn standalone_selects_literal_work_without_changing_values_or_inputs() {
        let source = include_str!("../../../tests/regression/indexed_distance_scan.pine");
        let primary = compile(source, CompileOptions::default()).program.unwrap();
        let literal = literal_program(&primary)
            .expect("compiler-owned structured guard")
            .program;
        assert_eq!(primary.inputs, literal.inputs);
        assert!(!literal.main().code.iter().any(|i| matches!(
            i,
            I::CallCollection {
                func: C::InternalDistanceScan | C::InternalDistanceScanEligible,
                ..
            }
        )));
        let close: Vec<_> = (0..257)
            .map(|i| 100.0 + (f64::from(i) * 0.19).sin())
            .collect();
        let data = MarketData {
            open: close.clone(),
            high: close.clone(),
            low: close.clone(),
            volume: vec![0.0; 257],
            close,
            ..MarketData::default()
        };
        for k in [1, 2, 8] {
            let mut options = RunOptions::default();
            options.inputs.insert(0, pinewasm_hir::InputValue::Int(k));
            let a = run(&primary, &data, &options);
            let b = run(&literal, &data, &options);
            assert!(a.errors.is_empty(), "{:?}", a.errors);
            assert!(b.errors.is_empty(), "{:?}", b.errors);
            assert_eq!(a.plots, b.plots);
        }
        let mut forged = primary;
        let pc = forged
            .main()
            .code
            .iter()
            .position(|i| {
                matches!(
                    i,
                    I::CallCollection {
                        func: C::InternalDistanceScanEligible,
                        ..
                    }
                )
            })
            .unwrap();
        forged.functions[0].code[pc + 1] = I::Pop;
        assert!(literal_program(&forged).is_none());
    }
    /// A small validated macro lets each hostile edge be exercised independently
    /// of source compiler block ordering and jump threading.
    fn cached_macro() -> CsilProgram {
        let mut p = compile(
            include_str!("../../../tests/regression/indexed_distance_scan.pine"),
            CompileOptions::default(),
        )
        .program
        .unwrap();
        let slot = p
            .main()
            .code
            .iter()
            .find_map(|i| match i {
                I::StoreUserSlotCurrent { slot } => Some(*slot),
                _ => None,
            })
            .unwrap();
        let mut code = vec![I::PushNa; 12];
        code.extend([
            I::CallCollection {
                func: C::InternalDistanceScanEligible,
                argc: 12,
            }, // 12
            I::JumpIfFalse { target: 15 }, // 13
            I::Jump { target: 18 },        // 14
            I::Jump { target: 31 },        // 15: literal arm
            I::Return,                     // 16: unreachable control-flow source
            I::Return,                     // 17: no fall-through into the kernel arm
        ]);
        code.extend(std::iter::repeat_n(I::PushNa, 10)); // 18..28
        code.extend([
            I::CallCollection {
                func: C::InternalDistanceScan,
                argc: 10,
            }, // 28
            I::StoreUserSlotCurrent { slot }, // 29
            I::Jump { target: 31 },           // 30
            I::Return,                        // 31
        ]);
        let span = p.main().source_map[0];
        p.functions[0].source_map = vec![span; code.len()];
        p.functions[0].code = code;
        p.functions[0].budget_reference = None;
        p.functions[0].max_stack = 32;
        pinewasm_csil::validate(&p).unwrap();
        assert!(literal_program(&p).is_some());
        p
    }

    #[test]
    fn cached_macro_rejects_every_outside_entry_to_the_neutralized_arm() {
        let mut conditional = cached_macro();
        conditional.functions[0].code[16] = I::Jump { target: 13 };
        let mut trampoline = cached_macro();
        trampoline.functions[0].code[16] = I::Jump { target: 14 };
        let mut false_into_true = cached_macro();
        false_into_true.functions[0].code[13] = I::JumpIfFalse { target: 14 };
        let mut fallthrough = cached_macro();
        fallthrough.functions[0].code[17] = I::PushBool(false);
        // Unreachable edges are still checked: a cached CSIL producer may later
        // make them reachable without changing the macro itself.
        for forged in [conditional, trampoline, false_into_true, fallthrough] {
            pinewasm_csil::validate(&forged).unwrap();
            assert!(literal_program(&forged).is_none());
        }
    }
    #[test]
    fn source_variants_and_two_scan_sites_keep_a_literal_target() {
        let base = include_str!("../../../tests/regression/indexed_distance_scan.pine");
        let begin = base.find("if bar_index >= start").unwrap();
        let end = base[begin..].find("plot(").unwrap() + begin;
        let second = base[begin..end]
            .replace("lastDistance", "secondThreshold")
            .replace("predictions", "secondPredictions")
            .replace("distances", "secondDistances");
        let two = format!("{base}\nvar secondPredictions = array.new_float()\nvar secondDistances = array.new_float()\nsecondThreshold = -1.0\n{second}\nplot(array.sum(secondPredictions))\n");
        for source in [
            base.replace("lastDistance = -1.0", "lastDistance = float(na)"),
            base.replace("k = input.int(8, \"Neighbors\")", "k = 7 / 2"),
            base.replace("f = input.int(5, \"Features\")", "f = 7 / 2"),
            two,
        ] {
            let result = compile(&source, CompileOptions::default());
            assert!(result.success, "{:?}", result.diagnostics);
            let program = result.program.unwrap();
            assert!(literal_program(&program).is_some(), "{source}");
        }
    }
}
