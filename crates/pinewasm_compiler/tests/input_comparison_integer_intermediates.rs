//! Consistency checks between the compiler and the runtime for integer intermediates.

use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, Value};

#[test]
fn safe_input_integers_keep_exact_arithmetic_intermediates_in_active_metadata() {
    // Every input default and arithmetic literal is JS-safe. Only the computed
    // intermediate crosses 2^53. The first case is the discriminating
    // example; subtraction and multiplication guard the same
    // checked integer path without a speculative division-policy change.
    let cases = [
        (
            "addition",
            9_007_199_254_740_991_i64,
            "(wide+2)>(wide+1)",
            true,
        ),
        (
            "subtraction",
            -9_007_199_254_740_991,
            "(wide-2)<(wide-1)",
            true,
        ),
        (
            "multiplication",
            3_002_399_751_580_331,
            "(wide*3)>(wide*3-1)",
            true,
        ),
        (
            "positive_subtraction",
            9_007_199_254_740_991,
            "(wide-(-2))>(wide-(-1))",
            true,
        ),
        (
            "add_overflow_comparison_is_false",
            9_007_199_254_740_991,
            "(wide*1024+2048)>0",
            false,
        ),
        (
            "sub_overflow_comparison_is_false",
            -9_007_199_254_740_991,
            "(wide*1024-2048)<0",
            false,
        ),
        (
            "mul_overflow_comparison_is_false",
            9_007_199_254_740_991,
            "(wide*2048)!=0",
            false,
        ),
    ];
    for (name, input, expression, expected) in cases {
        let source = format!(
            "//@version=6\nindicator(\"Integer active intermediates\")\nwide=input.int({input},\"wide\")\ncontrol=input.int(1,\"{name}\",active={expression})\nplot({expression}?1:0)\n"
        );
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![0.0],
            ..MarketData::default()
        };
        let run = compile_and_run(
            &source,
            &data,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.is_error()),
            "{name}: {:?}",
            run.diagnostics
        );
        assert!(
            run.runtime_errors.is_empty(),
            "{name}: {:?}",
            run.runtime_errors
        );
        assert_eq!(
            run.plots[0].values,
            vec![Value::Float(if expected { 1.0 } else { 0.0 })],
            "VM: {name}"
        );
        let program = run
            .compile
            .as_ref()
            .and_then(|result| result.program.as_ref())
            .expect("compiled program");
        let inputs = serde_json::to_value(&program.inputs).expect("input metadata");
        assert_eq!(
            inputs[1]["active"]["default"].as_bool(),
            Some(expected),
            "active: {name}"
        );
        assert_eq!(
            inputs[1]["active"]["dependencies"],
            serde_json::json!([0]),
            "dependency: {name}"
        );
        assert!(
            inputs[1]["active"]["expression"].is_object(),
            "serialized active expression: {name}"
        );
    }
}

#[test]
fn two_safe_input_factors_keep_adjacent_large_products_distinct() {
    // Exact products are 9007199456067585 and 9007199456067584. The former
    // rounds to the latter in binary64; both declared inputs are JS-safe.
    let source = "//@version=6\nindicator(\"Integer product active metadata\")\nleft=input.int(67108865)\nright=input.int(134217729)\ncontrol=input.int(1,active=left*right>(left-1)*(right+2))\nplot(left*right>(left-1)*(right+2)?1:0)\n";
    let data = MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![0.0],
        ..MarketData::default()
    };
    let run = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.is_error()),
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.plots[0].values, vec![Value::Float(1.0)]);
    let program = run
        .compile
        .as_ref()
        .and_then(|result| result.program.as_ref())
        .expect("compiled program");
    let inputs = serde_json::to_value(&program.inputs).expect("input metadata");
    assert_eq!(inputs[2]["active"]["default"].as_bool(), Some(true));
    assert_eq!(
        inputs[2]["active"]["dependencies"],
        serde_json::json!([0, 1])
    );
    assert!(inputs[2]["active"]["expression"].is_object());
}

#[test]
fn constant_boolean_defaults_keep_exact_add_sub_mul_intermediates() {
    for expression in [
        "(9007199254740991+2)>(9007199254740991+1)",
        "(-9007199254740991-2)<(-9007199254740991-1)",
        "(3002399751580331*3)>(3002399751580331*3-1)",
    ] {
        let source = format!("//@version=6\nindicator(\"Integer default intermediates\")\ncontrol=input.bool({expression})\nplot(control?1:0)\nplot({expression}?1:0)\n");
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![0.0],
            ..MarketData::default()
        };
        let run = compile_and_run(
            &source,
            &data,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.is_error()),
            "{expression}: {:?}",
            run.diagnostics
        );
        assert!(
            run.runtime_errors.is_empty(),
            "{expression}: {:?}",
            run.runtime_errors
        );
        for plot in &run.plots {
            assert_eq!(plot.values, vec![Value::Float(1.0)], "{expression}");
        }
    }
}
