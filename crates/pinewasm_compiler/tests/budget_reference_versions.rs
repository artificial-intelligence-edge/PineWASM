//! Budget-reference versions: a version-1 reference names the three-stage
//! optimization pipeline, version 2 the current five-stage one (with
//! flow-sensitive aliasing and jump threading).
//! Validation replays exactly the pipeline the version names, so an artifact
//! built with the three-stage pipeline still validates and runs.

use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, Program, RunOptions};
use pinewasm_csil::{
    validate, BudgetReference, CsilInstr, CsilInstr as C, BUDGET_REFERENCE_VERSION,
    BUDGET_REFERENCE_VERSION_THREE_STAGE,
};

fn market() -> MarketData {
    let close: Vec<f64> = (0..20).map(|bar| 100.0 + f64::from(bar)).collect();
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        volume: vec![1.0; close.len()],
        time: (1..=close.len() as i64).collect(),
        close,
        ..MarketData::default()
    }
}

/// The reference run as a program of its own.
fn as_program(program: &Program) -> Program {
    let mut reference_program = program.clone();
    let main = &mut reference_program.functions[0];
    let reference = main.budget_reference.take().expect("reference");
    main.code = reference.code;
    main.source_map = reference.source_map;
    main.max_stack = reference.max_stack;
    reference_program
}

fn mismatch(program: &Program) -> bool {
    validate(program).is_err_and(|errors| {
        errors
            .iter()
            .any(|error| error.to_string().contains("does not validate or reproduce"))
    })
}

/// A loop whose body copies the counter into a parameter slot read twice and
/// whose loop entry is `JUMP_IF_FALSE exit; JUMP body` with `body` next, plus
/// a dead literal store the three-stage pipeline removes.
fn reference_code() -> Vec<CsilInstr> {
    vec![
        C::PushNa,                              // 0  dead literal store
        C::StoreUserSlotCurrent { slot: 2 },    // 1
        C::PushI64(0),                          // 2
        C::StoreUserSlotCurrent { slot: 0 },    // 3  i = 0
        C::ReadUserSlot { slot: 0, offset: 0 }, // 4  loop head
        C::PushI64(3),                          // 5
        C::CmpLt,                               // 6
        C::JumpIfFalse { target: 20 },          // 7
        C::Jump { target: 9 },                  // 8  jump to the next instruction
        C::ReadUserSlot { slot: 0, offset: 0 }, // 9  p = i (inlined argument)
        C::StoreUserSlotCurrent { slot: 1 },    // 10
        C::ReadUserSlot { slot: 1, offset: 0 }, // 11
        C::Pop,                                 // 12
        C::ReadUserSlot { slot: 1, offset: 0 }, // 13
        C::Pop,                                 // 14
        C::ReadUserSlot { slot: 0, offset: 0 }, // 15 i += 1
        C::PushI64(1),                          // 16
        C::IntAdd,                              // 17
        C::StoreUserSlotCurrent { slot: 0 },    // 18
        C::Jump { target: 4 },                  // 19
        C::ReadUserSlot { slot: 0, offset: 0 }, // 20
        C::IntToF64,                            // 21
        C::PlotF64 { plot_slot: 0 },            // 22
        C::Return,                              // 23
    ]
}

#[test]
fn a_cached_three_stage_artifact_still_validates_and_runs() {
    let compiled = compile(
        "//@version=6\nindicator(\"versions\")\na = close\nb = open\nc = high\nplot(a + b + c)\n",
        CompileOptions::default(),
    );
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let mut program = compiled.program.expect("program");
    assert!(program.series_slots.len() >= 3);
    let code = reference_code();
    let spans: Vec<_> = program.main().source_map[..1]
        .iter()
        .cycle()
        .take(code.len())
        .copied()
        .enumerate()
        .map(|(pc, span)| pinewasm_compiler::Span::new(span.source, pc as u32, pc as u32 + 1))
        .collect();

    // What the three-stage pipeline made of it: only the dead literal store
    // is gone; every jump moves down by two.
    let mut old_target: Vec<CsilInstr> = code[2..].to_vec();
    for instr in &mut old_target {
        if let C::Jump { target } | C::JumpIfFalse { target } = instr {
            *target -= 2;
        }
    }
    let main = &mut program.functions[0];
    main.code = old_target;
    main.source_map = spans[2..].to_vec();
    main.max_stack = 2;
    main.budget_reference = Some(BudgetReference {
        version: BUDGET_REFERENCE_VERSION_THREE_STAGE,
        code,
        source_map: spans,
        max_stack: 2,
    });

    validate(&program).expect("a version-1 artifact validates under the three-stage replay");
    let market = market();
    let options = RunOptions::default();
    let run = run_program(&program, &market, &options);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 20);
    let reference_run = run_program(&as_program(&program), &market, &options);
    assert_eq!(run.plots, reference_run.plots);

    // The five-stage pipeline would also drop the copy and the jump, so the
    // same artifact labelled version 2 does not reproduce.
    let mut relabelled = program.clone();
    relabelled.functions[0]
        .budget_reference
        .as_mut()
        .expect("reference")
        .version = BUDGET_REFERENCE_VERSION;
    assert!(mismatch(&relabelled));
}

#[test]
fn a_current_artifact_carries_a_five_stage_reference() {
    let compiled = compile(
        concat!(
            "//@version=6\nindicator(\"versions\")\n",
            "twice(p) => p + p\n",
            "s = 0.0\n",
            "for i = 0 to 4\n",
            "    s += twice(close + i)\n",
            "plot(s)\n",
        ),
        CompileOptions::default(),
    );
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.expect("program");
    let reference = program.main().budget_reference.as_ref().expect("reference");
    assert_eq!(reference.version, BUDGET_REFERENCE_VERSION);
    validate(&program).expect("a version-2 artifact validates under the five-stage replay");
    let market = market();
    let run = run_program(&program, &market, &RunOptions::default());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let reference_run = run_program(&as_program(&program), &market, &RunOptions::default());
    assert_eq!(run.plots, reference_run.plots);

    // The three-stage replay does not reproduce it.
    let mut relabelled = program.clone();
    relabelled.functions[0]
        .budget_reference
        .as_mut()
        .expect("reference")
        .version = BUDGET_REFERENCE_VERSION_THREE_STAGE;
    assert!(mismatch(&relabelled));

    // An unknown version is rejected outright.
    let mut unknown = program;
    unknown.functions[0]
        .budget_reference
        .as_mut()
        .expect("reference")
        .version = 3;
    assert!(validate(&unknown).is_err());
}

/// A program the new stages leave unchanged keeps the version-1 reference,
/// so its serialized artifact keeps the version-1 format.
#[test]
fn an_artifact_the_new_stages_leave_alone_keeps_version_one() {
    let compiled = compile(
        "//@version=6\nindicator(\"budget admission\")\nx = close\nplot(x)\n",
        CompileOptions::default(),
    );
    let program = compiled.program.expect("program");
    let reference = program.main().budget_reference.as_ref().expect("reference");
    assert_eq!(reference.version, BUDGET_REFERENCE_VERSION_THREE_STAGE);
    validate(&program).expect("validates");
}
