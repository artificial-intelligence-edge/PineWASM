//! Cumulative run-budget failures use the same offset-indexed span map.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, RunOptions, RuntimeErrorKind, SourceId, Span,
};
use pinewasm_csil::{BudgetReference, CsilInstr};
use pinewasm_hir::LogLevel;
use pinewasm_mir::Value;

#[test]
fn cumulative_run_budget_at_relocated_return_keeps_original_span_and_completed_plot() {
    let result = compile(
        "//@version=6\nindicator(\"run budget span\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    assert!(result.success, "{:?}", result.diagnostics);
    let mut source = result.program.expect("fixture compiles");
    assert!(
        !source.series_slots.is_empty(),
        "fixture supplies slot zero"
    );
    assert_eq!(source.plots.len(), 1, "fixture supplies one plot lane");

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

    // The internal dead-store pass removes PCs 1 and 2, relocates the jump,
    // and currently truncates the original budget span vector to length two.
    let mut optimized = source.clone();
    optimized.functions[0].code = vec![CsilInstr::Jump { target: 1 }, CsilInstr::Return];
    optimized.functions[0].source_map = vec![spans[0], spans[3]];
    optimized.functions[0].budget_reference = Some(BudgetReference {
        version: 1,
        code: source.main().code.clone(),
        source_map: source.main().source_map.clone(),
        max_stack: source.main().max_stack,
    });

    pinewasm_csil::validate(&source).expect("source validates");
    pinewasm_csil::validate(&optimized).expect("optimized shape validates");

    let market = MarketData {
        open: vec![1.0, 2.0],
        high: vec![1.0, 2.0],
        low: vec![1.0, 2.0],
        close: vec![1.0, 2.0],
        volume: vec![1.0, 1.0],
        time: vec![1, 2],
        ..MarketData::default()
    };
    let generous = RunOptions {
        max_instructions_per_bar: 10,
        max_instructions_per_run: 4,
        ..RunOptions::default()
    };
    for program in [&source, &optimized] {
        let result = run_program(program, &market, &generous);
        assert!(result.runtime_errors.is_empty());
        assert_eq!(result.bars_executed, 2);
        assert_eq!(result.plots.len(), 1);
        assert_eq!(result.plots[0].values, vec![Value::Na, Value::Na]);
    }

    // Each bar takes two dispatches. The third allowed unit is the second
    // bar's Jump; the run ceiling refuses Return on that bar.
    let options = RunOptions {
        max_instructions_per_bar: 10,
        max_instructions_per_run: 3,
        ..RunOptions::default()
    };
    let before = run_program(&source, &market, &options);
    let after = run_program(&optimized, &market, &options);
    for result in [&before, &after] {
        assert_eq!(result.bars_executed, 1);
        assert_eq!(result.plots.len(), 1);
        assert_eq!(result.plots[0].values, vec![Value::Na]);
        assert_eq!(result.runtime_errors.len(), 1);
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::RunInstructionLimitExceeded
        );
        assert_eq!(result.runtime_errors[0].bar_index, Some(1));
    }
    assert_eq!(before.runtime_errors[0].instruction_offset, Some(3));
    assert_eq!(after.runtime_errors[0].instruction_offset, Some(1));
    assert_eq!(before.runtime_errors[0].span, Some(spans[3]));
    assert_eq!(
        after.runtime_errors[0].span, before.runtime_errors[0].span,
        "cumulative-budget diagnostics must identify the reached Return"
    );
}

#[test]
fn replay_uses_direct_origin_when_optimization_moves_failure_to_another_bar() {
    let result = compile(
        "//@version=6\nindicator(\"run budget fallback\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    let mut optimized = result.program.expect("fixture compiles");
    let spans: Vec<_> = (0..5)
        .map(|pc| Span::new(SourceId(1), pc, pc + 1))
        .collect();
    optimized.functions[0].code = vec![CsilInstr::PushNa, CsilInstr::Pop, CsilInstr::Return];
    optimized.functions[0].source_map = vec![spans[0], spans[3], spans[4]];
    optimized.functions[0].max_stack = 1;
    optimized.functions[0].budget_reference = Some(BudgetReference {
        version: 1,
        code: vec![
            CsilInstr::PushNa,
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::Pop,
            CsilInstr::Return,
        ],
        source_map: spans,
        max_stack: 1,
    });
    pinewasm_csil::validate(&optimized).expect("pair-erasure reference validates");
    let market = MarketData {
        open: vec![1.0, 2.0],
        high: vec![1.0, 2.0],
        low: vec![1.0, 2.0],
        close: vec![1.0, 2.0],
        volume: vec![1.0, 1.0],
        time: vec![1, 2],
        ..MarketData::default()
    };
    let options = RunOptions {
        max_instructions_per_run: 3,
        ..RunOptions::default()
    };
    let outcome = run_program(&optimized, &market, &options);
    assert_eq!(outcome.bars_executed, 1);
    assert_eq!(
        outcome.runtime_errors[0].kind,
        RuntimeErrorKind::RunInstructionLimitExceeded
    );
    assert_eq!(outcome.runtime_errors[0].bar_index, Some(1));
    assert_eq!(outcome.runtime_errors[0].instruction_offset, Some(0));
    assert_eq!(
        outcome.runtime_errors[0].span,
        Some(Span::new(SourceId(1), 0, 1))
    );
}

#[test]
fn replay_uses_direct_origin_when_same_bar_visible_logs_differ() {
    let result = compile(
        "//@version=6\nindicator(\"budget log fallback\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    let mut optimized = result.program.expect("fixture compiles");
    let string_index = optimized.constants.len() as u32;
    optimized.constants.push("logged".to_string());
    let spans: Vec<_> = (0..7)
        .map(|pc| Span::new(SourceId(1), pc, pc + 1))
        .collect();
    optimized.functions[0].code = vec![
        CsilInstr::PushNa,
        CsilInstr::Pop,
        CsilInstr::PushStr(string_index),
        CsilInstr::Log {
            level: LogLevel::Info,
            argc: 1,
        },
        CsilInstr::Return,
    ];
    optimized.functions[0].source_map = [0, 3, 4, 5, 6].map(|pc| spans[pc]).to_vec();
    optimized.functions[0].max_stack = 1;
    optimized.functions[0].budget_reference = Some(BudgetReference {
        version: 1,
        code: vec![
            CsilInstr::PushNa,
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::Pop,
            CsilInstr::PushStr(string_index),
            CsilInstr::Log {
                level: LogLevel::Info,
                argc: 1,
            },
            CsilInstr::Return,
        ],
        source_map: spans.clone(),
        max_stack: 1,
    });
    pinewasm_csil::validate(&optimized).expect("pair erasure validates");
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
        max_instructions_per_bar: 4,
        ..RunOptions::default()
    };
    let outcome = run_program(&optimized, &market, &options);
    assert_eq!(
        outcome.runtime_errors[0].kind,
        RuntimeErrorKind::InstructionLimitExceeded
    );
    assert_eq!(outcome.runtime_errors[0].bar_index, Some(0));
    assert_eq!(outcome.runtime_errors[0].instruction_offset, Some(4));
    assert_eq!(outcome.runtime_errors[0].span, Some(spans[6]));
    assert_eq!(
        outcome.logs.len(),
        1,
        "optimized path emitted one visible log"
    );
}
