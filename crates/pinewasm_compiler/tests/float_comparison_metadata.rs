use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

#[test]
fn float_input_defaults_and_active_defaults_agree_with_script_comparisons() {
    // Checks that input defaults and active defaults agree with script comparisons.
    let pairs = [
        (
            "0.0000000001",
            "0.0",
            [true, false, false, true, false, true],
        ),
        (
            "1.0000000001",
            "1.0",
            [false, true, false, false, true, true],
        ),
        (
            "1.00000000005",
            "1.0",
            [true, false, false, true, false, true],
        ),
        (
            "0.29999999985",
            "0.3",
            [false, true, true, true, false, false],
        ),
    ];
    for (lhs, rhs, expected) in pairs {
        for (op, wanted) in ["==", "!=", "<", "<=", ">", ">="].into_iter().zip(expected) {
            let source = format!(
                "//@version=6\nindicator(\"Float metadata comparisons\")\na=input.float({lhs})\nb=input.float({rhs})\ndefault_value=input.bool({lhs}{op}{rhs})\nn=input.int(1,active=a{op}b)\nplot(default_value?1:0)\nplot(a{op}b?1:0)\n"
            );
            let data = MarketData {
                open: vec![1.0],
                high: vec![1.0],
                low: vec![1.0],
                close: vec![1.0],
                volume: vec![1.0],
                ..MarketData::default()
            };
            let result = compile_and_run(
                &source,
                &data,
                CompileOptions::default(),
                &RunOptions::default(),
            );
            assert!(
                result.diagnostics.iter().all(|d| !d.is_error()),
                "{source}: {:?}",
                result.diagnostics
            );
            assert!(
                result.runtime_errors.is_empty(),
                "{source}: {:?}",
                result.runtime_errors
            );
            let expected = vec![Value::Float(if wanted { 1.0 } else { 0.0 })];
            assert_eq!(result.plots[0].values, expected, "default: {source}");
            assert_eq!(result.plots[1].values, expected, "runtime: {source}");
            let program = result
                .compile
                .as_ref()
                .and_then(|c| c.program.as_ref())
                .expect("compiled program");
            let inputs = serde_json::to_value(&program.inputs).expect("input metadata");
            assert_eq!(
                inputs[3]["active"]["default"].as_bool(),
                Some(wanted),
                "active: {source}"
            );
        }
    }
}
