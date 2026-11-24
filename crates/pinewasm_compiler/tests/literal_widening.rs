//! Literal conversion is compile-time work; inputs and integer arithmetic
//! retain their runtime semantics. Compare against explicit input widening so
//! the expected floating-point bits come from the unfused interpreter path.

use pinewasm_compiler::{
    compile, run_program_profiled, CompileOptions, InputValue, MarketData, Program, RunOptions,
};
use pinewasm_csil::CsilInstr;

fn program(expression: &str, declaration: &str) -> Program {
    let source =
        format!("//@version=6\nindicator(\"widening\")\n{declaration}\nplot({expression})\n");
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.unwrap()
}

fn data() -> MarketData {
    let close = vec![0.0, -0.0, 0.25, -0.25, 100.0];
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        volume: vec![1.0; close.len()],
        close,
        ..MarketData::default()
    }
}

#[test]
fn literal_widening_matches_runtime_integer_conversion_bits() {
    let dynamic = program("close + value", "value = input.int(0)");
    assert!(dynamic.main().code.contains(&CsilInstr::IntToF64));
    let data = data();
    for value in [
        0,
        1,
        42,
        (1_i64 << 53) - 1,
        1_i64 << 53,
        (1_i64 << 53) + 1,
        i64::MAX,
    ] {
        let folded = program(&format!("close + {value}"), "");
        assert!(!folded.main().code.contains(&CsilInstr::IntToF64));
        let mut options = RunOptions::default();
        options
            .inputs
            .insert(dynamic.inputs[0].id, InputValue::Int(value));
        let expected = run_program_profiled(&dynamic, &data, &options, None);
        let actual = run_program_profiled(&folded, &data, &RunOptions::default(), None);
        assert!(expected.runtime_errors.is_empty());
        assert!(actual.runtime_errors.is_empty());
        for (actual, expected) in actual.plots[0].values.iter().zip(&expected.plots[0].values) {
            let (
                pinewasm_compiler::Value::Float(actual),
                pinewasm_compiler::Value::Float(expected),
            ) = (actual, expected)
            else {
                panic!("expected floats")
            };
            assert_eq!(actual.to_bits(), expected.to_bits(), "integer {value}");
        }
    }
}

#[test]
fn integer_overflow_is_not_reassociated_into_float_arithmetic() {
    let program = program("close + (9223372036854775807 + 1)", "");
    assert!(program.main().code.contains(&CsilInstr::IntAdd));
    assert!(program.main().code.contains(&CsilInstr::IntToF64));
    let output = run_program_profiled(&program, &data(), &RunOptions::default(), None);
    assert!(output.runtime_errors.is_empty());
    assert!(output.plots[0]
        .values
        .iter()
        .all(|v| matches!(v, pinewasm_compiler::Value::Na)));
}

#[test]
fn dynamic_inputs_and_history_still_change_between_runs_and_bars() {
    let program = program("close + value[1]", "value = input.int(2)");
    assert!(program.main().code.contains(&CsilInstr::IntToF64));
    let data = data();
    for value in [2, -3, 9_007_199_254_740_993] {
        let mut options = RunOptions::default();
        options
            .inputs
            .insert(program.inputs[0].id, InputValue::Int(value));
        let output = run_program_profiled(&program, &data, &options, None);
        assert!(output.runtime_errors.is_empty());
        assert_eq!(output.plots[0].values[0], pinewasm_compiler::Value::Na);
        for (bar, actual) in output.plots[0].values.iter().enumerate().skip(1) {
            let pinewasm_compiler::Value::Float(actual) = actual else {
                panic!("expected float")
            };
            assert_eq!(actual.to_bits(), (data.close[bar] + value as f64).to_bits());
        }
    }
}
