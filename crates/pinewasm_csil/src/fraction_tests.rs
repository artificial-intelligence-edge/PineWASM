//! The whole-part rewrite (`fraction.rs`) and its operand-flow analysis
//! (`flow.rs`), on hand-built code and on lowered programs.

use super::compile_to_csil;
use crate::flow::Flow;
use crate::fraction::{
    fractional_code, leaves_unchanged, truncate_for_integer_consumers, whole_part_producers,
    Outcome,
};
use crate::{CollectionFunc, CsilInstr, PureFunc};
use pinewasm_parse::parse;
use pinewasm_semantics::analyze;
use pinewasm_span::{has_errors, Span};

const CAST: CsilInstr = CsilInstr::CallPure {
    func: PureFunc::CastInt,
    argc: 1,
};

fn get(argc: u8) -> CsilInstr {
    CsilInstr::CallCollection {
        func: CollectionFunc::ArrayGet,
        argc,
    }
}

/// Run the rewrite over `code` with one distinct span per instruction and
/// `slots` series slots; return the new code and spans.
fn rewrite(code: &[CsilInstr], slots: usize) -> (Outcome, Vec<CsilInstr>, Vec<Span>) {
    let mut code = code.to_vec();
    let mut spans: Vec<Span> = (0..code.len())
        .map(|pc| Span::new(pinewasm_span::SourceId(0), pc as u32, pc as u32 + 1))
        .collect();
    let outcome = truncate_for_integer_consumers(&mut code, &mut spans, &[], slots);
    (outcome, code, spans)
}

fn script(body: &str) -> String {
    format!("//@version=6\nindicator(\"D\")\nn = input.int(7, \"N\")\n{body}")
}

#[test]
fn code_without_a_pine_int_division_is_left_exactly_as_it_is() {
    // Every other int opcode feeding an integer-only consumer, the truncating
    // division included.
    for op in [
        CsilInstr::IntAdd,
        CsilInstr::IntSub,
        CsilInstr::IntMul,
        CsilInstr::IntDiv,
        CsilInstr::IntRem,
    ] {
        let code = vec![
            CsilInstr::PushNa,
            CsilInstr::PushI64(7),
            CsilInstr::PushI64(2),
            op,
            get(2),
            CsilInstr::Pop,
            CsilInstr::Return,
        ];
        assert!(leaves_unchanged(&code));
        assert_eq!(whole_part_producers(&code, &[], 0), Ok(None));
        let (outcome, after, spans) = rewrite(&code, 0);
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(after, code);
        assert_eq!(spans.len(), code.len());
    }
}

#[test]
fn a_quotient_read_only_by_an_integer_consumer_gets_a_cast_with_its_span() {
    let code = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    assert!(!leaves_unchanged(&code));
    assert_eq!(
        whole_part_producers(&code, &[], 0),
        Ok(Some(vec![false, false, false, true, false, false, false]))
    );
    let (outcome, after, spans) = rewrite(&code, 0);
    assert_eq!(outcome, Outcome::Rewritten { casts: 1 });
    let mut expected = code.clone();
    expected.insert(4, CAST);
    assert_eq!(after, expected);
    assert_eq!(spans[4], spans[3]);
    assert_eq!(spans.len(), after.len());
    // Rewriting again changes nothing: the cast is now the only reader.
    let (again, twice, _) = rewrite(&after, 0);
    assert_eq!(again, Outcome::Unchanged);
    assert_eq!(twice, after);
}

#[test]
fn a_quotient_that_something_reads_as_a_number_is_left_alone() {
    // `Dup` sends the quotient to an integer-only consumer and to a
    // comparison: truncating it would change the comparison.
    let code = vec![
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        CsilInstr::Dup,
        CsilInstr::PushI64(3),
        CsilInstr::CmpGt,
        CsilInstr::Pop,
        CsilInstr::PushNa,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let (outcome, after, _) = rewrite(&code, 0);
    assert_eq!(outcome, Outcome::Unchanged);
    assert_eq!(after, code);
    // `Dup` sends the quotient to a slot and to an integer-only consumer:
    // the slot must keep the fraction, so the quotient is left alone.
    let code = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        CsilInstr::Dup,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let (outcome, after, _) = rewrite(&code, 1);
    assert_eq!(outcome, Outcome::Unchanged);
    assert_eq!(after, code);
    // Stored to a slot: the slot keeps the fraction, and the read that feeds
    // the integer-only consumer is truncated instead.
    let code = vec![
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        CsilInstr::PushNa,
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    assert_eq!(
        whole_part_producers(&code, &[], 1),
        Ok(Some(vec![
            false, false, false, false, false, true, false, false, false
        ]))
    );
}

#[test]
fn branch_targets_are_relocated_past_an_inserted_cast() {
    let code = vec![
        CsilInstr::PushBool(true),
        CsilInstr::JumpIfFalse { target: 8 },
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Jump { target: 9 },
        CsilInstr::Return,
    ];
    let (outcome, after, _) = rewrite(&code, 0);
    assert_eq!(outcome, Outcome::Rewritten { casts: 1 });
    assert_eq!(after[1], CsilInstr::JumpIfFalse { target: 9 });
    assert_eq!(after[6], CAST);
    assert_eq!(after[9], CsilInstr::Jump { target: 10 });
    assert_eq!(after[10], CsilInstr::Return);
}

#[test]
fn an_unreachable_instruction_is_skipped_not_fatal() {
    // pcs 1 and 2 follow a jump and are never reached; the flow records no
    // operands for them.
    let code = vec![
        CsilInstr::Jump { target: 3 },
        CsilInstr::Select,
        CsilInstr::Pop,
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let flow = Flow::build_code(&code, &[], 0).expect("flow");
    assert_eq!(
        flow.reached,
        vec![true, false, false, true, true, true, true, true, true, true]
    );
    assert!(flow.operands[1].is_empty());
    let (outcome, after, _) = rewrite(&code, 0);
    assert_eq!(outcome, Outcome::Rewritten { casts: 1 });
    assert_eq!(after[7], CAST);
    assert_eq!(after[0], CsilInstr::Jump { target: 3 });
}

#[test]
fn code_the_flow_cannot_describe_is_left_as_lowered_and_reported() {
    // A stack underflow, a branch out of range, a slot outside the table and
    // mismatched span counts each make the rewrite decline.
    let underflow = vec![CsilInstr::PineIntDiv, get(2), CsilInstr::Return];
    let out_of_range = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Jump { target: 99 },
    ];
    let no_slot = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::StoreUserSlotCurrent { slot: 5 },
        CsilInstr::Return,
    ];
    for code in [underflow, out_of_range, no_slot] {
        assert!(Flow::build_code(&code, &[], 0).is_none());
        assert_eq!(whole_part_producers(&code, &[], 0), Err(()));
        let (outcome, after, _) = rewrite(&code, 0);
        assert_eq!(outcome, Outcome::NotAnalyzed);
        assert_eq!(after, code);
    }
    assert!(Flow::build_code(&[], &[], 0).is_none());
    let mut code = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let before = code.clone();
    let mut spans = Vec::new();
    assert_eq!(
        truncate_for_integer_consumers(&mut code, &mut spans, &[], 0),
        Outcome::NotAnalyzed
    );
    assert_eq!(code, before);
}

#[test]
fn the_fraction_spreads_through_arithmetic_forwarding_calls_and_slots() {
    let code = vec![
        CsilInstr::PushI64(7),                          // 0
        CsilInstr::PushI64(2),                          // 1
        CsilInstr::PineIntDiv,                          // 2 seed
        CsilInstr::PushI64(1),                          // 3
        CsilInstr::IntAdd,                              // 4 arithmetic
        CsilInstr::StoreUserSlotCurrent { slot: 0 },    // 5
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 }, // 6 slot read
        CsilInstr::CallPure {
            func: PureFunc::MathAbs,
            argc: 1,
        }, // 7 forward
        CsilInstr::IntToF64,                            // 8 numeric: stops here
        CsilInstr::Pop,                                 // 9
        CsilInstr::ReadUserSlot { slot: 1, offset: 0 }, // 10 another slot
        CsilInstr::Pop,                                 // 11
        CsilInstr::Return,                              // 12
    ];
    let flow = Flow::build_code(&code, &[], 2).expect("flow");
    let all = fractional_code(&code, &flow, |_| true).expect("fractional");
    assert_eq!(
        all.nodes,
        vec![false, false, true, false, true, false, true, true, false, false, false, false, false]
    );
    assert_eq!(all.slots, vec![true, false]);
    // A backend that lowers this quotient exactly seeds nothing.
    let none = fractional_code(&code, &flow, |_| false).expect("fractional");
    assert!(!none.nodes.contains(&true));
    assert_eq!(none.slots, vec![false, false]);
}

#[test]
fn a_consumer_on_neither_list_leaves_the_value_alone() {
    // `Alert` is in no class of `classify` and not on the whole-part list:
    // the quotient reaches it as it is, and because it reads the quotient,
    // an integer-only reader of the same value does not truncate it either.
    let unknown_only = vec![
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        CsilInstr::PushI64(0),
        CsilInstr::Alert,
        CsilInstr::Return,
    ];
    let (outcome, after, _) = rewrite(&unknown_only, 0);
    assert_eq!(outcome, Outcome::Unchanged);
    assert_eq!(after, unknown_only);
    let shared = vec![
        CsilInstr::PushNa,
        CsilInstr::PushI64(7),
        CsilInstr::PushI64(2),
        CsilInstr::PineIntDiv,
        CsilInstr::Dup,
        CsilInstr::PushI64(0),
        CsilInstr::Alert,
        get(2),
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let (outcome, after, _) = rewrite(&shared, 0);
    assert_eq!(outcome, Outcome::Unchanged);
    assert_eq!(after, shared);
}

#[test]
fn the_budget_reference_and_the_optimized_code_both_carry_the_cast() {
    // `k` is a single-use slot the optimizer erases, so the program has a
    // budget reference. The cast is in the reference (it was inserted before
    // the reference was taken) and in the optimized code, and the program
    // validates: the reference still optimizes to the code.
    let program = compile_to_csil(&script(
        "k = n / 2\na = array.new_float(k, 1.0)\nplot(array.size(a))\n",
    ));
    crate::validate(&program).expect("validates");
    let casts = |code: &[CsilInstr]| code.iter().filter(|instr| **instr == CAST).count();
    let reference = program.main().budget_reference.as_ref().expect("reference");
    assert_eq!(casts(&reference.code), 1);
    assert_eq!(casts(&program.main().code), 1);
    assert!(reference.code.len() > program.main().code.len());
    let division = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, CsilInstr::PineIntDiv))
        .expect("division");
    assert_eq!(program.main().code[division + 1], CAST);
}

/// Every `.pine` test program under `tests/` that compiles.
fn corpus() -> Vec<(String, pinewasm_mir::MirBody)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut out = Vec::new();
    for dir in ["compile-pass", "regression"] {
        let mut paths: Vec<_> = std::fs::read_dir(root.join(dir))
            .expect("corpus directory")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "pine"))
            .collect();
        paths.sort();
        for path in paths {
            let source = std::fs::read_to_string(&path).expect("source");
            let parsed = parse(&source);
            if has_errors(&parsed.diagnostics) {
                continue;
            }
            let analysis = analyze(&parsed.program);
            if has_errors(&analysis.diagnostics) {
                continue;
            }
            out.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                pinewasm_mir::lower(&analysis.hir),
            ));
        }
    }
    out
}

#[test]
fn the_rewrite_is_the_identity_on_corpus_programs_without_a_pine_int_division() {
    let corpus = corpus();
    assert!(
        corpus.len() >= 40,
        "test programs shrank to {}",
        corpus.len()
    );
    let (mut without, mut with, mut rewritten) = (0, 0, 0);
    for (name, mir) in &corpus {
        let (code, spans) = crate::lower::stackified_for_tests(mir);
        let (mut after, mut after_spans) = (code.clone(), spans.clone());
        let outcome = truncate_for_integer_consumers(
            &mut after,
            &mut after_spans,
            &mir.metadata.ta_states,
            mir.metadata.series_slots.len(),
        );
        // The stackifier's output is always analyzable.
        assert_ne!(outcome, Outcome::NotAnalyzed, "{name}");
        let changed = matches!(outcome, Outcome::Rewritten { .. });
        if leaves_unchanged(&code) {
            without += 1;
            assert_eq!(outcome, Outcome::Unchanged, "{name}");
            assert_eq!(after, code, "{name}");
            assert_eq!(after_spans, spans, "{name}");
        } else {
            with += 1;
            rewritten += usize::from(changed);
            // Only casts were added, each with its producer's span.
            assert_eq!(after.len(), after_spans.len(), "{name}");
            let kept: Vec<&CsilInstr> = after.iter().filter(|instr| **instr != CAST).collect();
            let original: Vec<&CsilInstr> = code.iter().filter(|instr| **instr != CAST).collect();
            assert_eq!(kept.len(), original.len(), "{name}");
        }
        // The shipped pipeline validates either way.
        // The shipped lowering of this program did not skip its analysis.
        // (The process-wide `not_analyzed_count` is moved by tests that
        // lower damaged programs in parallel, so the outcome of this very
        // lowering is checked instead.)
        let (program, shipped) = crate::lower::lower_mir_outcome_for_tests(mir).expect("lowers");
        assert_eq!(shipped, outcome, "{name}");
        assert_eq!(program, crate::lower_mir(mir).expect("lowers"), "{name}");
        crate::validate(&program).unwrap_or_else(|errors| panic!("{name}: {errors:?}"));
    }
    // The split is pinned so the test cannot pass by seeing no programs of
    // either kind.
    assert!(without >= 30, "{without} programs without a division");
    assert!(with >= 1, "{with} programs with a division");
    println!("test programs: {without} without PINE_INT_DIV, {with} with, {rewritten} rewritten");
}

#[test]
fn a_lowering_whose_rewrite_is_skipped_is_counted() {
    // No source program makes the analysis decline, so damage one: drop the
    // TA state table of a real MIR body that calls `ta.rsi`. The flow cannot
    // tell how many operands the call pops, the rewrite is skipped, and
    // lowering still
    // returns a program (the validator rejects it later). The skip must be
    // visible in the counter rather than silent.
    let parsed = parse(&script(
        "a = array.from(10, 20, 30, 40, 50)\nplot(array.get(a, n / 2) + ta.rsi(close, 14))\n",
    ));
    let analysis = analyze(&parsed.program);
    assert!(!has_errors(&analysis.diagnostics));
    let mut mir = pinewasm_mir::lower(&analysis.hir);

    let (mut code, mut spans) = crate::lower::stackified_for_tests(&mir);
    assert!(matches!(
        truncate_for_integer_consumers(
            &mut code,
            &mut spans,
            &mir.metadata.ta_states,
            mir.metadata.series_slots.len(),
        ),
        Outcome::Rewritten { casts: 1 }
    ));

    assert!(!mir.metadata.ta_states.is_empty());
    mir.metadata.ta_states.clear();
    let before = crate::fraction::not_analyzed_count();
    let program = crate::lower_mir(&mir).expect("lowering does not fail");
    assert!(crate::fraction::not_analyzed_count() > before);
    assert_eq!(
        crate::lower::lower_mir_outcome_for_tests(&mir).map(|(_, outcome)| outcome),
        Ok(Outcome::NotAnalyzed)
    );
    // Left as lowered: no cast was inserted.
    assert!(!program.main().code.contains(&CAST));
    assert!(crate::validate(&program).is_err());
}
