use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

#[test]
fn input_defaults_and_active_order_two_ints_without_a_float_round_trip() {
    let pairs = [
        (9_007_199_254_740_993_i64, 9_007_199_254_740_992_i64),
        (i64::MAX, i64::MAX - 1),
        (-9_007_199_254_740_993_i64, -9_007_199_254_740_992_i64),
    ];
    for (lhs, rhs) in pairs {
        for op in ["<", "<=", ">", ">="] {
            let expected = match op {
                "<" => lhs < rhs,
                "<=" => lhs <= rhs,
                ">" => lhs > rhs,
                _ => lhs >= rhs,
            };
            let source = format!(
                "//@version=6\nindicator(\"Integer metadata order\")\na=input.int({lhs})\nb=input.int({rhs})\ndefault_value=input.bool({lhs}{op}{rhs})\nn=input.int(1,active=a{op}b)\nplot(default_value?1:0)\nplot(a{op}b?1:0)\n"
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
            let wanted = vec![Value::Float(if expected { 1.0 } else { 0.0 })];
            assert_eq!(result.plots[0].values, wanted, "default: {source}");
            assert_eq!(result.plots[1].values, wanted, "runtime: {source}");
            let program = result
                .compile
                .as_ref()
                .and_then(|c| c.program.as_ref())
                .expect("compiled program");
            let inputs = serde_json::to_value(&program.inputs).expect("input metadata");
            assert_eq!(
                inputs[3]["active"]["default"].as_bool(),
                Some(expected),
                "active: {source}"
            );
        }
    }
}
