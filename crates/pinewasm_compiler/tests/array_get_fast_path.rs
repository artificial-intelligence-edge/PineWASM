//! Successful scalar reads may bypass the call buffer; errors, aliasing,
//! string quotas, and full-width index checks must retain their behavior.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions, RunResult};

fn run(body: &str, options: RunOptions) -> RunResult {
    let source = format!("//@version=6\nindicator(\"Array read\")\n{body}\n");
    let data = MarketData {
        time: vec![1_700_000_000_000],
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        ..MarketData::default()
    };
    let result = compile_and_run(&source, &data, CompileOptions::default(), &options);
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    result
}
fn plot(result: &RunResult, index: usize) -> serde_json::Value {
    serde_json::to_value(&result.plots[index].values).unwrap()
}

#[test]
fn out_of_range_indices_are_rejected_for_reads_and_writes() {
    for index in [-3_i64, 1_i64 << 32, (1_i64 << 32) + 1, i64::MAX] {
        for operation in [
            format!("plot(array.get(a, {index}))"),
            format!("array.set(a, {index}, 3.0)\nplot(array.get(a, 0))"),
        ] {
            let result = run(
                &format!("a = array.from(1.0, 2.0)\n{operation}"),
                RunOptions::default(),
            );
            assert_eq!(result.runtime_errors.len(), 1);
            assert!(result.runtime_errors[0].message.contains("out of bounds"));
            assert!(result.runtime_errors[0]
                .message
                .contains(&index.to_string()));
        }
    }
}

#[test]
fn negative_indices_within_the_size_read_and_write_from_the_end() {
    let result = run(
        "a = array.from(1.0, 2.0)\narray.set(a, -2, 7.0)\nplot(array.get(a, -1))\nplot(array.get(a, -2))\nplot(array.get(a, 0))",
        RunOptions::default(),
    );
    assert!(result.runtime_errors.is_empty());
    assert_eq!(plot(&result, 0), serde_json::json!([2.0]));
    assert_eq!(plot(&result, 1), serde_json::json!([7.0]));
    assert_eq!(plot(&result, 2), serde_json::json!([7.0]));
}

#[test]
fn scalar_values_and_missing_elements_are_retained() {
    let result = run("a = array.from(-0.0, 2.5, float(na))\ni = array.from(7, -9)\nb = array.from(true, false)\nplot(array.get(a, 0))\nplot(array.get(a, 1))\nplot(array.get(a, 2))\nplot(array.get(i, 1))\nplot(array.get(b, 0) ? 1 : 0)", RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(plot(&result, 0), serde_json::json!([-0.0]));
    assert_eq!(plot(&result, 1), serde_json::json!([2.5]));
    assert_eq!(plot(&result, 2), serde_json::json!([null]));
    assert_eq!(plot(&result, 3), serde_json::json!([-9.0]));
    assert_eq!(plot(&result, 4), serde_json::json!([1.0]));
}

#[test]
fn object_elements_keep_reference_aliasing() {
    let result = run("type Point\n    float value\np = Point.new(2.0)\na = array.from(p)\nalias = array.get(a, 0)\nalias.value := 9.0\nplot(p.value)\nplot(array.get(a, 0).value)", RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(plot(&result, 0), serde_json::json!([9.0]));
    assert_eq!(plot(&result, 1), serde_json::json!([9.0]));
}

#[test]
fn missing_index_returns_na_but_missing_receiver_still_errors() {
    let result = run(
        "a = array.from(7.0)\nplot(array.get(a, int(na)))",
        RunOptions::default(),
    );
    assert!(result.runtime_errors.is_empty());
    assert_eq!(plot(&result, 0), serde_json::json!([null]));
    let result = run(
        "array<float> a = na\nplot(array.get(a, int(na)))",
        RunOptions::default(),
    );
    assert_eq!(result.runtime_errors.len(), 1);
    assert!(result.runtime_errors[0].message.contains("na"));
}

#[test]
fn string_reads_keep_clone_budget_and_contents() {
    let body = "a = array.from(\"abc\")\nlog.info(array.get(a, 0))\nplot(1)";
    let result = run(body, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(result.logs[0].message, "abc");
    let result = run(
        body,
        RunOptions {
            max_string_bytes_per_run: 8,
            ..RunOptions::default()
        },
    );
    assert_eq!(result.runtime_errors.len(), 1);
    assert!(result.runtime_errors[0].message.contains("string"));
    assert!(result.logs.is_empty());
}
