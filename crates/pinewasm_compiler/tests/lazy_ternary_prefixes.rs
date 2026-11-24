//! A ternary must guard an inlined arm's statements as well as its return.

use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RealtimeSim, RunOptions, RunResult, Tick,
};
use pinewasm_csil::CsilInstr;
use pinewasm_hir::{InputValue, PureFunc};

fn market(bars: usize) -> MarketData {
    MarketData {
        open: vec![100.0; bars],
        high: vec![101.0; bars],
        low: vec![99.0; bars],
        close: vec![100.0; bars],
        volume: vec![1000.0; bars],
        time: (0..bars)
            .map(|bar| 1_700_000_000_000 + bar as i64 * 60_000)
            .collect(),
        ..MarketData::default()
    }
}

fn run(source: &str, bars: usize, options: RunOptions) -> RunResult {
    let result = compile_and_run(source, &market(bars), CompileOptions::default(), &options);
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    result
}

fn logs(result: &RunResult) -> Vec<&str> {
    result
        .logs
        .iter()
        .map(|entry| entry.message.as_str())
        .collect()
}

fn values(result: &RunResult, plot: usize) -> serde_json::Value {
    serde_json::to_value(&result.plots[plot].values).unwrap()
}

#[test]
fn unselected_loop_has_zero_math_dispatches() {
    for condition in ["true", "kind == \"cheap\"", "bar_index >= 0"] {
        let source = format!(
            "//@version=6\nindicator(\"Lazy loop\")\nkind = \"cheap\"\nheavy(float x) =>\n    float total = 0.0\n    for i = 0 to 19\n        total += math.sin(x + i)\n    total\nplot({condition} ? close : heavy(close))\n"
        );
        let result = run(
            &source,
            3,
            RunOptions {
                profile: true,
                ..RunOptions::default()
            },
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        assert_eq!(values(&result, 0), serde_json::json!([100.0, 100.0, 100.0]));
        let program = result.compile.as_ref().unwrap().program.as_ref().unwrap();
        let profile = result.profile.as_ref().unwrap();
        let sine_count: f64 = program.functions[0]
            .code
            .iter()
            .zip(&profile.count)
            .filter_map(|(instruction, count)| {
                matches!(
                    instruction,
                    CsilInstr::CallPure {
                        func: PureFunc::MathSin,
                        ..
                    }
                )
                .then_some(count)
            })
            .sum();
        assert_eq!(sine_count, 0.0, "unselected loop ran for {condition}");
    }
}

#[test]
fn selected_branch_keeps_its_loop() {
    let result = run(
        "//@version=6\nindicator(\"Selected loop\")\nf() =>\n    float total = 0.0\n    for i = 0 to 19\n        total += 1.0\n    total\nplot(false ? close : f())\n",
        2, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(values(&result, 0), serde_json::json!([20.0, 20.0]));
}

#[test]
fn condition_precedes_only_selected_call_and_its_arguments() {
    let source = "//@version=6\nindicator(\"Call order\")\ncondition() =>\n    log.info(\"condition\")\n    true\nargument() =>\n    log.info(\"argument\")\n    2.0\narm(string name, float value) =>\n    log.info(name)\n    value\nplot(condition() ? arm(\"then\", 1.0) : arm(\"else\", argument()))\n";
    let result = run(source, 2, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(logs(&result), ["condition", "then", "condition", "then"]);
    assert_eq!(values(&result, 0), serde_json::json!([1.0, 1.0]));
}

#[test]
fn nested_series_conditions_select_one_entire_arm_per_bar() {
    let source = "//@version=6\nindicator(\"Nested branches\")\nf(string name) =>\n    log.info(name)\n    7.0\nplot(bar_index % 2 == 0 ? f(\"even\") : bar_index % 3 == 0 ? f(\"three\") : f(\"other\"))\n";
    let result = run(source, 6, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(
        logs(&result),
        ["even", "other", "even", "three", "even", "other"]
    );
    assert_eq!(
        values(&result, 0),
        serde_json::json!([7.0, 7.0, 7.0, 7.0, 7.0, 7.0])
    );
}

#[test]
fn na_inference_does_not_hoist_the_other_arms_prefix() {
    for (expression, expected) in [
        (
            "bar_index % 2 == 0 ? na : f()",
            serde_json::json!([null, 5.0, null, 5.0]),
        ),
        (
            "bar_index % 2 == 0 ? f() : na",
            serde_json::json!([5.0, null, 5.0, null]),
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Typed na\")\nf() =>\n    log.info(\"selected\")\n    5.0\nplot({expression})\n");
        let result = run(&source, 4, RunOptions::default());
        assert!(result.runtime_errors.is_empty());
        assert_eq!(logs(&result), ["selected", "selected"]);
        assert_eq!(values(&result, 0), expected);
    }
}

#[test]
fn unselected_collection_mutation_is_suppressed() {
    let source = "//@version=6\nindicator(\"Lazy mutation\")\nadd(array<float> target, float value) =>\n    array.push(target, value)\n    value\nvar values = array.new_float()\nplot(bar_index % 2 == 0 ? add(values, 1.0) : add(values, 2.0))\nplot(array.size(values))\nplot(array.sum(values))\n";
    let result = run(source, 4, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(values(&result, 1), serde_json::json!([1.0, 2.0, 3.0, 4.0]));
    assert_eq!(values(&result, 2), serde_json::json!([1.0, 3.0, 4.0, 6.0]));
}

#[test]
fn errors_in_a_prefix_are_observed_only_when_selected() {
    for (condition, should_error) in [("true", false), ("false", true)] {
        let source = format!("//@version=6\nindicator(\"Lazy error\")\nbroken() =>\n    values = array.new_float()\n    value = array.get(values, 0)\n    value + 1.0\nplot({condition} ? close : broken())\n");
        let result = run(&source, 1, RunOptions::default());
        assert_eq!(!result.runtime_errors.is_empty(), should_error);
        if !should_error {
            assert_eq!(values(&result, 0), serde_json::json!([100.0]));
        }
    }
}

#[test]
fn input_qualifier_and_host_override_survive_the_join_slot() {
    let source = "//@version=6\nindicator(\"Input branches\")\nchoose = input.bool(true)\nf() =>\n    log.info(\"selected\")\n    2\nlength = choose ? f() : 3\nplot(ta.sma(close, length))\n";
    for choice in [true, false] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Bool(choice));
        let result = run(source, 4, options);
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        assert_eq!(result.logs.len(), if choice { 4 } else { 0 });
        assert_eq!(
            values(&result, 0),
            if choice {
                serde_json::json!([null, 100.0, 100.0, 100.0])
            } else {
                serde_json::json!([null, null, 100.0, 100.0])
            }
        );
    }
}

#[test]
fn branch_local_persistent_state_advances_only_on_selected_bars() {
    let source = "//@version=6\nindicator(\"Branch history\")\nf() =>\n    var int count = 0\n    count += 1\n    count\nvalue = bar_index % 2 == 0 ? f() : 0\nplot(value)\nplot(value[1])\n";
    let result = run(source, 4, RunOptions::default());
    assert!(result.runtime_errors.is_empty());
    assert_eq!(values(&result, 0), serde_json::json!([1.0, 0.0, 2.0, 0.0]));
    assert_eq!(values(&result, 1), serde_json::json!([null, 1.0, 0.0, 2.0]));
}

#[test]
fn realtime_varip_retains_selected_ticks_and_var_rolls_back() {
    let source = "//@version=6\nindicator(\"Branch ticks\")\nintrabar() =>\n    varip int count = 0\n    count += 1\n    count\ncommitted() =>\n    var int count = 0\n    count += 1\n    count\nplot(close > 100 ? intrabar() : 0)\nplot(close > 100 ? committed() : 0)\n";
    let ticks = [101.0, 99.0, 103.0]
        .into_iter()
        .enumerate()
        .map(|(ordinal, close)| Tick {
            bar_index: 1,
            open: 100.0,
            high: 103.0,
            low: 99.0,
            close,
            volume: 1000.0,
            time: 1_700_000_060_000,
            trading_day: None,
            is_bar_close: ordinal == 2,
        })
        .collect();
    let result = run(
        source,
        1,
        RunOptions {
            realtime: Some(RealtimeSim { ticks }),
            realtime_trace: true,
            ..RunOptions::default()
        },
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(values(&result, 0), serde_json::json!([0.0, 2.0]));
    assert_eq!(values(&result, 1), serde_json::json!([0.0, 1.0]));
    let samples: Vec<_> = result
        .realtime_trace
        .as_ref()
        .unwrap()
        .iter()
        .map(|sample| &sample.values)
        .collect();
    assert_eq!(
        serde_json::to_value(samples).unwrap(),
        serde_json::json!([[1.0, 1.0], [0.0, 0.0], [2.0, 1.0]])
    );
}
