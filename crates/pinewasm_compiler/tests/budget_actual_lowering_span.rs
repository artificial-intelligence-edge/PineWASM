//! Exercise the real MIR-to-CSIL dead-store optimizer and its VM budget span.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, PreparedProgram, RunOptions,
    RuntimeErrorKind, SourceId, Span,
};
use pinewasm_csil::{lower_mir, validate, CsilInstr};
use pinewasm_hir::{default_max_bars_back, DrawingCaps, SeriesSlot};
use pinewasm_mir::{
    BlockId, MirBlock, MirBody, MirInstr, MirMetadata, SlotDef, Terminator, Value, ValueId,
};

#[test]
fn actual_dead_store_pass_preserves_jump_budget_failure_span() {
    let spans: Vec<_> = (0..5)
        .map(|pc| Span::new(SourceId(1), pc, pc + 1))
        .collect();
    let mir = MirBody {
        blocks: vec![
            MirBlock {
                id: BlockId(0),
                instrs: vec![],
                terminator: Terminator::Jump {
                    target: BlockId(2),
                    span: spans[0],
                },
            },
            MirBlock {
                id: BlockId(1),
                instrs: vec![
                    MirInstr::Const {
                        dst: ValueId(0),
                        value: Value::Na,
                        span: spans[1],
                    },
                    MirInstr::StoreUserSeriesCurrent {
                        slot: SeriesSlot(0),
                        value: ValueId(0),
                        span: spans[2],
                    },
                ],
                // A `Return` (not a jump to the next block, which jump
                // threading would remove) keeps the relocated jump alive.
                terminator: Terminator::Return { span: spans[3] },
            },
            MirBlock {
                id: BlockId(2),
                instrs: vec![],
                terminator: Terminator::Return { span: spans[4] },
            },
        ],
        entry: BlockId(0),
        value_count: 1,
        metadata: MirMetadata {
            indicator: None,
            title: Some("budget actual lowering".into()),
            overlay: false,
            strategy: None,
            drawing_caps: DrawingCaps::default(),
            max_bars_back: default_max_bars_back(),
            series_slots: vec![SlotDef {
                name: "unused".into(),
                persistent: false,
                varip: false,
                lane: None,
            }],
            plots: vec![],
            visuals: vec![],
            fills: vec![],
            ta_states: vec![],
            ta_input_bindings: vec![],
            inputs: vec![],
            object_types: vec![],
        },
    };

    let optimized = lower_mir(&mir).expect("real CSIL lowering and optimizer succeed");
    assert_eq!(
        optimized.main().code,
        vec![
            CsilInstr::Jump { target: 2 },
            CsilInstr::Return,
            CsilInstr::Return,
        ],
        "dead constant store pair is actually removed and jump relocated"
    );
    assert_eq!(
        optimized.main().source_map,
        vec![spans[0], spans[3], spans[4]]
    );
    assert_eq!(
        optimized
            .main()
            .budget_reference
            .as_ref()
            .unwrap()
            .source_map,
        spans
    );

    // The unoptimized MIR emission is the same five-opcode stream before
    // lower_mir's internal elide_dead_constant_stores call.
    let mut source = optimized.clone();
    source.functions[0].code = vec![
        CsilInstr::Jump { target: 4 },
        CsilInstr::PushNa,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        CsilInstr::Return,
        CsilInstr::Return,
    ];
    source.functions[0].source_map = spans.clone();
    source.functions[0].budget_reference = None;
    validate(&source).expect("pre-optimization stream validates");
    validate(&optimized).expect("actual optimized stream validates");
    let decoded = pinewasm_csil::CsilProgram::from_json(&optimized.to_json()).unwrap();
    validate(&decoded).expect("serialized reference validates");
    assert_eq!(decoded, optimized);
    let mut forged = decoded.clone();
    forged.functions[0].budget_reference.as_mut().unwrap().code[0] = CsilInstr::Return;
    assert!(
        validate(&forged).is_err(),
        "forged reference rewrite is rejected"
    );
    let mut malformed = decoded;
    malformed.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .source_map
        .pop();
    assert!(
        validate(&malformed).is_err(),
        "short reference map is rejected"
    );
    malformed.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .source_map
        .push(spans[4]);
    malformed.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .version = 99;
    assert!(
        validate(&malformed).is_err(),
        "unknown reference version is rejected"
    );
    let mut oversized = optimized.clone();
    oversized.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .code[0] = CsilInstr::BuildFootprint {
        selector: "x".repeat(pinewasm_csil::MAX_BUDGET_REFERENCE_BYTES + 1),
    };
    let errors = validate(&oversized).expect_err("oversized nested selector is rejected");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("oversized budget reference")),
        "errors: {errors:?}"
    );
    let mut reserved = optimized.clone();
    reserved.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .code
        .reserve_exact(
            pinewasm_csil::MAX_BUDGET_REFERENCE_BYTES / std::mem::size_of::<CsilInstr>() + 1,
        );
    let errors = validate(&reserved).expect_err("reserved instruction capacity is rejected");
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("oversized budget reference")));
    let mut selector_capacity = optimized.clone();
    let mut selector = String::with_capacity(pinewasm_csil::MAX_BUDGET_REFERENCE_BYTES + 1);
    selector.push('x');
    selector_capacity.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .code[0] = CsilInstr::BuildFootprint { selector };
    let errors = validate(&selector_capacity).expect_err("reserved string capacity is rejected");
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("oversized budget reference")));

    let market = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![1],
        ..MarketData::default()
    };
    let generous = RunOptions {
        max_instructions_per_bar: 5,
        ..RunOptions::default()
    };
    for program in [&source, &optimized] {
        let outcome = run_program(program, &market, &generous);
        assert_eq!(outcome.bars_executed, 1);
        assert!(outcome.runtime_errors.is_empty());
    }
    let mut reserved_program = optimized.clone();
    reserved_program.functions[0]
        .budget_reference
        .as_mut()
        .unwrap()
        .code
        .reserve_exact(8 * 1024 * 1024 / std::mem::size_of::<CsilInstr>() + 1);
    let memory_options = RunOptions {
        max_vm_bytes_per_run: 8 * 1024 * 1024,
        ..generous.clone()
    };
    let memory_outcome = run_program(&reserved_program, &market, &memory_options);
    assert_eq!(
        memory_outcome.runtime_errors[0].kind,
        RuntimeErrorKind::VmMemoryLimitExceeded
    );
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
    assert_eq!(before.runtime_errors[0].instruction_offset, Some(4));
    assert_eq!(after.runtime_errors[0].instruction_offset, Some(2));
    assert_eq!(before.runtime_errors[0].span, Some(spans[4]));
    assert_eq!(
        after.runtime_errors[0].span, before.runtime_errors[0].span,
        "actual optimized jump reaches Return, not the removed Store's source"
    );
}

/// Local diagnostic benchmark: `run_program` validates every call while
/// `PreparedProgram` validates once. Timings are informational, not a CI gate.
#[test]
#[ignore = "manual diagnostic benchmark; timing is not a CI gate"]
fn repeated_run_validation_cost_with_budget_reference() {
    let compiled = compile(
        "//@version=6\nindicator(\"budget admission\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    let program = compiled.program.expect("fixture compiles");
    assert!(program.main().budget_reference.is_some());
    let prepared = PreparedProgram::new(program.clone()).expect("prepared admission");
    let data = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![1],
        ..MarketData::default()
    };
    let options = RunOptions::default();
    let count = 200;
    let start = std::time::Instant::now();
    for _ in 0..count {
        std::hint::black_box(run_program(&program, &data, &options));
    }
    let public_elapsed = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..count {
        std::hint::black_box(prepared.run_profiled(&data, &options, None));
    }
    let prepared_elapsed = start.elapsed();
    eprintln!(
        "{count} one-bar runs: run_program {public_elapsed:?}; prepared {prepared_elapsed:?}"
    );
}
