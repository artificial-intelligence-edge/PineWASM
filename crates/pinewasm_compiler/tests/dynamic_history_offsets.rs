//! Source-emitted dynamic user-history regression.

use pinewasm_compiler::{MarketData, RunOptions, RuntimeErrorKind, Value};
use pinewasm_csil::{CsilInstr, InputValue, ValidationError};

const SOURCE: &str = "//@version=6\n\
indicator(\"Dynamic history proof\", max_bars_back=2)\n\
x = close + 1.0\n\
i = input.int(0)\n\
plot(x[i])\n";

#[test]
fn source_emitted_dynamic_history_matches_offset_outcomes() {
    let parsed = pinewasm_parse::parse(SOURCE);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    assert!(
        analysis.diagnostics.is_empty(),
        "{:?}",
        analysis.diagnostics
    );
    let mir = pinewasm_mir::lower(&analysis.hir);
    let program = pinewasm_csil::lower_mir(&mir).expect("lower source to CSIL");
    pinewasm_csil::validate(&program).expect("source program validates");
    // Boundary: the validator accepts a bounded declaration above the
    // computed peak, while the strict fixture profile requires exactly 2.
    assert_eq!(program.main().max_stack, 2);
    let mut overdeclared = program.clone();
    overdeclared.functions[0].max_stack = 3;
    pinewasm_csil::validate(&overdeclared)
        .expect("Rust validator permits safe stack overdeclaration");
    let dynamic_pc = program
        .main()
        .code
        .iter()
        .position(|instruction| matches!(instruction, CsilInstr::ReadUserSlotDynamic { .. }))
        .expect("source must emit dynamic user-slot read") as u32;
    assert_eq!(program.metadata.max_bars_back, 2);

    let closes = vec![10.0, 20.0, 30.0, 40.0, 50.0];
    let data = MarketData {
        open: closes.clone(),
        high: closes.clone(),
        low: closes.clone(),
        close: closes.clone(),
        volume: vec![1.0; closes.len()],
        ..MarketData::default()
    };
    // Rust's abstract `Any` on a user-slot read satisfies the styled plot's static
    // requirement. The VM still needs its runtime check when that slot holds
    // an integer. This valid CSIL program is outside the strict fixture profile.
    let mut uncertain_slot = program.clone();
    uncertain_slot.functions[0].budget_reference = None;
    uncertain_slot.functions[0].code[0] = CsilInstr::PushI64(7);
    uncertain_slot.functions[0].code[1] = CsilInstr::PushI64(1);
    uncertain_slot.functions[0].code[2] = CsilInstr::IntAdd;
    pinewasm_csil::validate(&uncertain_slot)
        .expect("Rust validator permits Any from a dynamic user-slot read");
    assert!(matches!(
        program.main().code[dynamic_pc as usize + 1],
        CsilInstr::PushColor { .. }
    ));
    assert!(matches!(
        program.main().code[dynamic_pc as usize + 2],
        CsilInstr::PlotStyledF64 { .. }
    ));
    let uncertain_outcome = pinewasm_vm::run(&uncertain_slot, &data, &RunOptions::default());
    assert_eq!(uncertain_outcome.bars_executed, 0);
    assert_eq!(uncertain_outcome.errors.len(), 1);
    assert_eq!(
        uncertain_outcome.errors[0].kind,
        RuntimeErrorKind::TypeMismatch
    );
    assert_eq!(
        uncertain_outcome.errors[0].instruction_offset,
        Some(dynamic_pc + 2)
    );

    // Both edges reach PC 5 with one operand. The validator joins F64 and
    // Int to Any and permits PLOT_F64; only the float path can plot safely.
    // This synthetic branch is not produced by SOURCE.
    let mut joined = program.clone();
    joined.functions[0].budget_reference = None;
    joined.functions[0].code = vec![
        CsilInstr::PushBool(true),
        CsilInstr::JumpIfFalse { target: 4 },
        CsilInstr::PushF64(pinewasm_csil::F64Bits::new(3.5)),
        CsilInstr::Jump { target: 5 },
        CsilInstr::PushI64(7),
        CsilInstr::PlotF64 { plot_slot: 0 },
        CsilInstr::Return,
    ];
    joined.functions[0].source_map = vec![program.main().source_map[0]; 7];
    joined.functions[0].max_stack = 1;
    pinewasm_csil::validate(&joined).expect("equal-height F64/Int branch join is accepted as Any");
    let float_branch = pinewasm_vm::run(&joined, &data, &RunOptions::default());
    assert!(float_branch.errors.is_empty(), "{:?}", float_branch.errors);
    assert_eq!(float_branch.plots[0].values, vec![Value::Float(3.5); 5]);
    joined.functions[0].code[0] = CsilInstr::PushBool(false);
    pinewasm_csil::validate(&joined).expect("constant predicate does not alter the Any join");
    let int_branch = pinewasm_vm::run(&joined, &data, &RunOptions::default());
    assert_eq!(int_branch.bars_executed, 0);
    assert_eq!(int_branch.errors.len(), 1);
    assert_eq!(int_branch.errors[0].kind, RuntimeErrorKind::TypeMismatch);
    assert_eq!(int_branch.errors[0].instruction_offset, Some(5));
    joined.functions[0].code[4] = CsilInstr::PushF64(pinewasm_csil::F64Bits::new(7.0));
    pinewasm_csil::validate(&joined).expect("both float branches validate");
    let safe_false_branch = pinewasm_vm::run(&joined, &data, &RunOptions::default());
    assert!(
        safe_false_branch.errors.is_empty(),
        "{:?}",
        safe_false_branch.errors
    );
    assert_eq!(
        safe_false_branch.plots[0].values,
        vec![Value::Float(7.0); 5]
    );
    // Moving the taken edge straight to the plot gives that predecessor
    // height zero and the fallthrough predecessor height one. The validator
    // rejects this inconsistent join.
    let mut inconsistent_height = joined.clone();
    inconsistent_height.functions[0].code[1] = CsilInstr::JumpIfFalse { target: 5 };
    let height_errors = pinewasm_csil::validate(&inconsistent_height)
        .expect_err("unequal operand heights at a merge must be rejected");
    assert!(height_errors.iter().any(|error| matches!(
        error,
        ValidationError::AtInstruction { message, .. }
            if message.contains("inconsistent stack heights")
    )));

    let expected = [
        (-1, vec![Value::Na; 5]),
        (
            0,
            vec![11.0, 21.0, 31.0, 41.0, 51.0]
                .into_iter()
                .map(Value::Float)
                .collect(),
        ),
        (
            1,
            vec![
                Value::Na,
                Value::Float(11.0),
                Value::Float(21.0),
                Value::Float(31.0),
                Value::Float(41.0),
            ],
        ),
        (
            2,
            vec![
                Value::Na,
                Value::Na,
                Value::Float(11.0),
                Value::Float(21.0),
                Value::Float(31.0),
            ],
        ),
    ];
    for (offset, plots) in expected {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(offset));
        let outcome = pinewasm_vm::run(&program, &data, &options);
        assert!(
            outcome.errors.is_empty(),
            "offset={offset}: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.bars_executed, 5, "offset={offset}");
        assert_eq!(outcome.plots[0].values, plots, "offset={offset}");
    }

    for offset in [3, i64::MAX] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(offset));
        let outcome = pinewasm_vm::run(&program, &data, &options);
        assert_eq!(outcome.bars_executed, 0, "offset={offset}");
        assert_eq!(outcome.errors.len(), 1, "offset={offset}");
        let error = &outcome.errors[0];
        assert_eq!(error.kind, RuntimeErrorKind::HistoryBufferLimitExceeded);
        assert_eq!(error.bar_index, Some(0));
        assert_eq!(error.instruction_offset, Some(dynamic_pc));
        assert!(error.span.is_some(), "dynamic read retains its source span");
        assert_eq!(
            error.span,
            program.main().source_map.get(dynamic_pc as usize).copied()
        );
        assert!(
            error.message.contains(&format!(
                "history offset {offset} exceeds the max_bars_back limit of 2"
            )),
            "{error:?}"
        );
    }

    // The validator's `Int` abstract type includes `na`. Replacing the one
    // offset producer with PushNa remains valid CSIL and exercises the VM's
    // non-integer fallback without relying on a source-level coercion.
    let mut missing = program.clone();
    missing.functions[0].budget_reference = None;
    let input_read = missing
        .main()
        .code
        .iter()
        .position(|instruction| matches!(instruction, CsilInstr::ReadInput { .. }))
        .expect("dynamic offset reads the input");
    missing.functions[0].code[input_read] = CsilInstr::PushNa;
    pinewasm_csil::validate(&missing).expect("na offset remains valid CSIL");
    let outcome = pinewasm_vm::run(&missing, &data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.plots[0].values, vec![Value::Na; 5]);

    // A bool offset is invalid CSIL; the VM still handles it defensively.
    let mut unexpected = program.clone();
    unexpected.functions[0].budget_reference = None;
    unexpected.functions[0].code[input_read] = CsilInstr::PushBool(true);
    assert!(pinewasm_csil::validate(&unexpected).is_err());
    let outcome = pinewasm_vm::run(&unexpected, &data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.plots[0].values, vec![Value::Na; 5]);

    // A statically typed int offset can carry a fractional value; history
    // offsets truncate it toward zero.
    let mut fractional = program.clone();
    fractional.functions[0].budget_reference = None;
    fractional.functions[0].code[input_read] = CsilInstr::PushF64(pinewasm_csil::F64Bits::new(1.5));
    pinewasm_csil::validate(&fractional).expect("fractional offset is valid CSIL");
    let outcome = pinewasm_vm::run(&fractional, &data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(
        outcome.plots[0].values,
        vec![
            Value::Na,
            Value::Float(11.0),
            Value::Float(21.0),
            Value::Float(31.0),
            Value::Float(41.0)
        ]
    );
}
