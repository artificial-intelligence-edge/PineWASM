//! Regression witness for budget attribution across a relocated jump over a
//! removed dead constant store. The two streams take the same executed path.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, RunOptions, RuntimeErrorKind, SourceId, Span,
};
use pinewasm_csil::{BudgetReference, CsilInstr};

#[test]
fn budget_failure_after_jump_over_dead_store_keeps_return_span() {
    let result = compile(
        "//@version=6\nindicator(\"budget jump\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    let mut source = result.program.expect("fixture compiles");
    assert!(
        !source.series_slots.is_empty(),
        "fixture supplies slot zero"
    );

    let spans: Vec<_> = (0..4)
        .map(|pc| Span::new(SourceId(1), pc, pc + 1))
        .collect();
    source.functions[0].code = vec![
        CsilInstr::Jump { target: 3 },
        CsilInstr::PushNa,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        CsilInstr::Return,
    ];
    source.functions[0].source_map = spans.clone();
    source.functions[0].budget_reference = None;
    source.functions[0].max_stack = 1;

    // This is the result of elide_dead_constant_stores on the four opcodes
    // above: the skipped PushNa/store pair is removed, and Jump 3 becomes
    // Jump 1. Its current budget-map rule truncates the original prefix.
    let mut optimized = source.clone();
    optimized.functions[0].code = vec![CsilInstr::Jump { target: 1 }, CsilInstr::Return];
    optimized.functions[0].source_map = vec![spans[0], spans[3]];
    optimized.functions[0].budget_reference = Some(BudgetReference {
        version: 1,
        code: source.main().code.clone(),
        source_map: source.main().source_map.clone(),
        max_stack: source.main().max_stack,
    });

    pinewasm_csil::validate(&source).expect("original stream validates");
    pinewasm_csil::validate(&optimized).expect("optimized stream validates");

    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![1],
        ..MarketData::default()
    };
    let options = RunOptions {
        max_instructions_per_bar: 1,
        ..RunOptions::default()
    };
    let before = run_program(&source, &market, &options);
    let after = run_program(&optimized, &market, &options);

    for outcome in [&before, &after] {
        assert_eq!(outcome.bars_executed, 0);
        assert_eq!(outcome.runtime_errors.len(), 1);
        assert_eq!(
            outcome.runtime_errors[0].kind,
            RuntimeErrorKind::InstructionLimitExceeded
        );
    }
    assert_eq!(before.runtime_errors[0].instruction_offset, Some(3));
    assert_eq!(after.runtime_errors[0].instruction_offset, Some(1));
    assert_eq!(before.runtime_errors[0].span, Some(spans[3]));
    assert_eq!(
        after.runtime_errors[0].span, before.runtime_errors[0].span,
        "the relocated Return is the next executed opcode on both paths"
    );
}
