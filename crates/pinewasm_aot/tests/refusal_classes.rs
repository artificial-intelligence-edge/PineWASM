//! Refusal classes: the control for the zero-internal-refusal gate in
//! `standalone_wasm_census.rs`.
//!
//! That gate only means something if a real internal refusal is classified as
//! one. The planted program below is accepted by the validator and is refused
//! by the backend with a type conflict, because one slot is stored as an int
//! and as a float. The front end cannot produce it; it is built by editing
//! compiled output.

use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, supports, AotError, RefusalClass,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram, F64Bits};

const SOURCE: &str = r#"//@version=6
indicator("refusal class control")
var int n = 0
n := 5
plot(close, "Backtest Signal")
"#;

fn compiled() -> CsilProgram {
    let result = compile(SOURCE, CompileOptions::default());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "control source must compile: {:?}",
        result.diagnostics
    );
    result.program.expect("program")
}

fn standalone(program: &CsilProgram) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
}

/// Store a float into the slot the program stores an int into, just before
/// the final instruction.
fn plant_mixed_slot(program: &mut CsilProgram) {
    let function = &mut program.functions[0];
    let slot = function
        .code
        .iter()
        .find_map(|instr| match instr {
            CsilInstr::StoreUserSlotCurrent { slot } => Some(*slot),
            _ => None,
        })
        .expect("the control source stores a user slot");
    let at = function.code.len() - 1;
    function.code.splice(
        at..at,
        [
            CsilInstr::PushF64(F64Bits::new(1.5)),
            CsilInstr::StoreUserSlotCurrent { slot },
        ],
    );
    let span = function.source_map[at];
    function.source_map.splice(at..at, [span, span]);
    // The edit no longer matches the stored budget reference.
    function.budget_reference = None;
}

#[test]
fn unmodified_control_source_is_accepted() {
    standalone(&compiled()).expect("the unmodified control program builds");
}

#[test]
fn planted_type_conflict_is_an_internal_refusal() {
    let mut program = compiled();
    plant_mixed_slot(&mut program);
    pinewasm_csil::validate(&program).expect("the validator accepts the planted program");
    let error = standalone(&program).expect_err("the backend must refuse a mixed-type slot");
    assert_eq!(error.kind(), "TypeConflict", "{error}");
    assert_eq!(error.class(), RefusalClass::Internal, "{error}");
}

#[test]
fn declared_limits_are_capability_refusals() {
    for error in [
        AotError::UnsupportedOpcode("PineIntDiv".into()),
        AotError::UnsupportedIntegerNaEncoding { pc: 0 },
        AotError::StandaloneWasmNotStandalone("program would import host.vm".into()),
        AotError::StandaloneWasmNoDesignatedPlot("program has 2 plots".into()),
        AotError::StrategyFeatureUnsupported {
            feature: "currency".into(),
        },
    ] {
        assert_eq!(error.class(), RefusalClass::Capability, "{error}");
    }
}

#[test]
fn table_delete_alias_invalidation_is_interpreter_only() {
    let result = compile(
        "//@version=6\nindicator(\"Deleted table\")\nvar table t = table.new(position.top_left, 1, 1)\nvar table alias = t\ntable.delete(t)\nplot(na(alias) ? 1 : 0)\n",
        CompileOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "table-delete source must compile: {:?}",
        result.diagnostics
    );
    let program = result.program.expect("program");
    let refusal =
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into());

    assert_eq!(supports(&program), Err(refusal));
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into())
    );
    assert_eq!(
        standalone(&program).unwrap_err(),
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into())
    );
}

#[test]
fn same_position_table_replacement_is_interpreter_only() {
    let result = compile(
        "//@version=6\nindicator(\"Table replacement\")\nvar table first = table.new(position.top_left, 1, 1)\nvar table alias = first\nvar array<table> saved = array.new_table()\narray.push(saved, first)\nif barstate.isfirst\n    table.cell(first, 0, 0, \"old\")\ntable replacement = table.new(position.top_left, 2, 1, force_overlay = true)\ntable.cell(replacement, 1, 0, \"new\")\nplot(na(first) ? 1 : 0)\nplot(na(alias) ? 1 : 0)\nplot(na(array.get(saved, 0)) ? 1 : 0)\n",
        CompileOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "replacement source must compile: {:?}",
        result.diagnostics
    );
    let program = result.program.expect("program");
    assert_eq!(
        supports(&program),
        Err(AotError::UnsupportedOpcode(
            "table position replacement requires interpreter".into()
        ))
    );
    assert_eq!(
        compile_program(&program).unwrap_err(),
        AotError::UnsupportedOpcode("table position replacement requires interpreter".into())
    );
}

#[test]
fn one_shot_persistent_table_initializer_remains_native_aot_supported() {
    let result = compile(
        "//@version=6\nindicator(\"One table\")\nvar table panel = table.new(position.top_left, 1, 1)\ntable.cell(panel, 0, 0, \"kept\")\nplot(close)\n",
        CompileOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "single-table source must compile: {:?}",
        result.diagnostics
    );
    let program = result.program.expect("program");
    assert!(supports(&program).is_ok());
    assert!(compile_program(&program).is_ok());
}

#[test]
fn repeatable_loop_and_helper_table_constructors_do_not_pass_the_one_shot_guard() {
    let sources = [
        "//@version=6\nindicator(\"Loop table\")\nvar table t = na\nfor i = 1 to 2\n    t := table.new(position.top_left, 1, 1)\nplot(close)\n",
        "//@version=6\nindicator(\"Helper table\")\nf() => table.new(position.top_left, 1, 1)\nfor i = 1 to 2\n    table t = f()\nplot(close)\n",
    ];
    for source in sources {
        let result = compile(source, CompileOptions::default());
        assert!(
            !result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "repeatable table source must compile: {:?}",
            result.diagnostics
        );
        let program = result.program.expect("program");
        assert!(matches!(
            supports(&program),
            Err(AotError::UnsupportedOpcode(message))
                if message == "table position replacement requires interpreter"
        ));
    }
}

#[test]
fn self_contradictions_are_internal_refusals() {
    for error in [
        AotError::TypeConflict("pc 9: operand is F64, needs I64".into()),
        AotError::InvalidProgram("type inference did not converge".into()),
        AotError::IrreducibleControlFlow,
        AotError::StandaloneWasmExportSurface("missing export".into()),
    ] {
        assert_eq!(error.class(), RefusalClass::Internal, "{error}");
    }
}
