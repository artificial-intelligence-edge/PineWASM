//! A join can require two original diagnostic spans at one optimized PC.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, RunOptions, RuntimeErrorKind, SourceId, Span,
};
use pinewasm_csil::{BudgetReference, CsilInstr};
use pinewasm_hir::BuiltinSeries;

fn market(close: f64) -> MarketData {
    MarketData {
        open: vec![close],
        high: vec![close],
        low: vec![close],
        close: vec![close],
        volume: vec![1.0],
        time: vec![1],
        ..MarketData::default()
    }
}

#[test]
fn join_budget_failure_needs_path_dependent_original_span() {
    let result = compile(
        "//@version=6\nindicator(\"budget join\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    let mut source = result.program.expect("fixture compiles");
    assert!(
        !source.series_slots.is_empty(),
        "fixture supplies slot zero"
    );

    let spans: Vec<_> = (0..13)
        .map(|pc| Span::new(SourceId(1), pc, pc + 1))
        .collect();
    source.functions[0].code = vec![
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Close,
            offset: 0,
        },
        CsilInstr::PushF64(0.0.into()),
        CsilInstr::CmpGt,
        CsilInstr::JumpIfFalse { target: 7 },
        CsilInstr::PushBool(true),
        CsilInstr::Pop,
        CsilInstr::Jump { target: 12 },
        CsilInstr::PushNa,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        CsilInstr::PushBool(false),
        CsilInstr::BoolNot,
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    source.functions[0].source_map = spans.clone();
    source.functions[0].budget_reference = None;
    source.functions[0].max_stack = 2;

    // The dead-store pass removes PCs 7 and 8, then relocates both jumps.
    // Its current budget-map rule takes the first 11 original spans.
    let mut optimized = source.clone();
    optimized.functions[0].code = vec![
        CsilInstr::ReadBuiltin {
            source: BuiltinSeries::Close,
            offset: 0,
        },
        CsilInstr::PushF64(0.0.into()),
        CsilInstr::CmpGt,
        CsilInstr::JumpIfFalse { target: 7 },
        CsilInstr::PushBool(true),
        CsilInstr::Pop,
        CsilInstr::Jump { target: 10 },
        CsilInstr::PushBool(false),
        CsilInstr::BoolNot,
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    optimized.functions[0].source_map = [0, 1, 2, 3, 4, 5, 6, 9, 10, 11, 12]
        .map(|pc| spans[pc])
        .to_vec();
    optimized.functions[0].budget_reference = Some(BudgetReference {
        version: 1,
        code: source.main().code.clone(),
        source_map: source.main().source_map.clone(),
        max_stack: source.main().max_stack,
    });

    pinewasm_csil::validate(&source).expect("source diamond validates");
    pinewasm_csil::validate(&optimized).expect("optimized diamond validates");

    let generous = RunOptions {
        max_instructions_per_bar: 13,
        ..RunOptions::default()
    };
    for close in [1.0, -1.0] {
        for program in [&source, &optimized] {
            let outcome = run_program(program, &market(close), &generous);
            assert_eq!(outcome.bars_executed, 1, "close={close}");
            assert!(outcome.runtime_errors.is_empty(), "close={close}");
        }
    }

    let options = RunOptions {
        max_instructions_per_bar: 7,
        ..RunOptions::default()
    };
    let true_source = run_program(&source, &market(1.0), &options);
    let false_source = run_program(&source, &market(-1.0), &options);
    let true_optimized = run_program(&optimized, &market(1.0), &options);
    let false_optimized = run_program(&optimized, &market(-1.0), &options);

    for outcome in [
        &true_source,
        &false_source,
        &true_optimized,
        &false_optimized,
    ] {
        assert_eq!(outcome.bars_executed, 0);
        assert_eq!(outcome.runtime_errors.len(), 1);
        assert_eq!(
            outcome.runtime_errors[0].kind,
            RuntimeErrorKind::InstructionLimitExceeded
        );
    }
    assert_eq!(true_source.runtime_errors[0].instruction_offset, Some(12));
    assert_eq!(false_source.runtime_errors[0].instruction_offset, Some(10));
    assert_eq!(true_source.runtime_errors[0].span, Some(spans[12]));
    assert_eq!(false_source.runtime_errors[0].span, Some(spans[10]));
    assert_ne!(
        true_source.runtime_errors[0].span, false_source.runtime_errors[0].span,
        "the same budget cut reaches different original opcodes"
    );
    assert_eq!(
        true_optimized.runtime_errors[0].instruction_offset,
        Some(10)
    );
    assert_eq!(
        false_optimized.runtime_errors[0].instruction_offset,
        Some(10)
    );
    assert_eq!(
        false_optimized.runtime_errors[0].span,
        false_source.runtime_errors[0].span
    );
    assert_eq!(
        true_optimized.runtime_errors[0].span, true_source.runtime_errors[0].span,
        "one static optimized-PC span cannot describe both incoming paths"
    );
    let mut without_reference = optimized.clone();
    without_reference.functions[0].budget_reference = None;
    let fallback = run_program(&without_reference, &market(-1.0), &options);
    assert_eq!(fallback.runtime_errors[0].span, Some(spans[12]));
    assert_eq!(fallback.runtime_errors[0].instruction_offset, Some(10));

    let realtime_options = RunOptions {
        realtime: Some(pinewasm_compiler::RealtimeSim { ticks: vec![] }),
        ..options.clone()
    };
    let realtime = run_program(&optimized, &market(-1.0), &realtime_options);
    assert_eq!(realtime.runtime_errors[0].span, Some(spans[12]));

    let session_error =
        match pinewasm_vm::IncrementalSession::new(&optimized, &market(-1.0), &options) {
            Ok(_) => panic!("session should stop on the budget"),
            Err(error) => error,
        };
    assert_eq!(session_error.span, Some(spans[12]));
}
