//! Regression test for a function/tuple request form that the compiler must
//! accept.

use pinewasm_compiler::{
    compile, compile_and_run_with_libraries, compile_with_libraries, CompileOptions, MarketData,
    RunOptions, Value,
};

// Rational-quadratic example follows jdehorty/KernelFunctions/2, published under
// MPL-2.0: https://www.tradingview.com/script/e0Ek9x99-KernelFunctions/
// Copyright jdehorty. Keep the cardinality expression and accumulator loop: they
// are the program shapes the compiler must accept, not a pre-expanded form.
fn fixture() -> serde_json::Value {
    let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../pinewasm_lsp/fixtures/language_queries.json"
    ))
    .expect("shared language-query fixtures are valid JSON");
    fixtures
        .into_iter()
        .find(|fixture| fixture["name"] == "shared-htf-pure-helper-fixture")
        .expect("shared HTF helper fixture exists")["htfPureHelper"]
        .clone()
}

fn source() -> String {
    fixture()["source"]
        .as_str()
        .expect("shared HTF main source is a string")
        .into()
}

fn library() -> String {
    fixture()["library"]
        .as_str()
        .expect("shared HTF library source is a string")
        .into()
}

fn data() -> MarketData {
    let close: Vec<f64> = (0..360)
        .map(|i| 100.0 + (i as f64 / 24.0).sin() * 8.0 + i as f64 * 0.01)
        .collect();
    MarketData {
        open: close.clone(),
        high: close.iter().map(|x| x + 1.0).collect(),
        low: close.iter().map(|x| x - 1.0).collect(),
        volume: vec![1000.0; close.len()],
        time: (0..close.len())
            .map(|i| 1_704_067_200_000 + i as i64 * 14_400_000)
            .collect(),
        timeframe: "240".into(),
        close,
        ..MarketData::default()
    }
}

fn expected_kernel(daily: &[f64]) -> f64 {
    let mut weighted = 0.0;
    let mut total = 0.0;
    for i in 0..27 {
        let weight = (1.0 + (i * i) as f64 / 1024.0).powf(-8.0);
        weighted += daily[daily.len() - 1 - i] * weight;
        total += weight;
    }
    weighted / total
}

#[test]
fn imported_accumulator_loop_and_mixed_tuple_use_confirmed_daily_history() {
    let data = data();
    let libraries = [("test/Kernel/2".into(), library())];
    let result = compile_and_run_with_libraries(
        &source(),
        &libraries,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().unwrap().success,
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 360);
    let daily: Vec<f64> = data.close.chunks(6).map(|x| x[5]).collect();
    for i in 0..168 {
        assert_eq!(result.plots[1].values[i], Value::Float(0.0));
        assert_eq!(result.plots[2].values[i], Value::Float(0.0));
    }
    for i in 168..360 {
        let n = i / 6;
        let expected = expected_kernel(&daily[..n]);
        let prior = expected_kernel(&daily[..n - 1]);
        let Value::Float(actual) = result.plots[0].values[i] else {
            panic!("missing kernel on bar {i}");
        };
        assert!(
            (actual - expected).abs() < 1e-10,
            "bar {i}: {actual} != {expected}"
        );
        assert_eq!(
            result.plots[1].values[i],
            Value::Float(if expected > prior { 1.0 } else { 0.0 })
        );
        assert_eq!(
            result.plots[2].values[i],
            Value::Float(if expected < prior { 1.0 } else { 0.0 })
        );
    }
}

#[test]
fn float_boolean_literal_tuple_preserves_types_and_false_warmup() {
    let source = "//@version=6\nindicator(\"Mixed\")\n[a,b,c]=request.security(syminfo.tickerid,\"1D\",[close[1],close[1]>close[2],false],lookahead=barmerge.lookahead_on)\nplot(a)\nplot(b?1:0)\nplot(c?1:0)\n";
    let result = compile_and_run_with_libraries(
        source,
        &[],
        &data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().unwrap().success,
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert!(result.plots[2]
        .values
        .iter()
        .all(|v| *v == Value::Float(0.0)));
}

#[test]
fn existing_immutable_ta_tuple_helper_remains_supported() {
    let source = "//@version=6\nindicator(\"TA helper\")\nf()=>\n    x=ta.sma(close,2)\n    [x[1],x[1]>x[2]]\n[a,b]=request.security(syminfo.tickerid,\"1D\",f(),lookahead=barmerge.lookahead_on)\nplot(a)\nplot(b?1:0)\n";
    let feed = data();
    let result = compile_and_run_with_libraries(
        source,
        &[],
        &feed,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().unwrap().success,
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    for bar in 12..360 {
        let day = bar / 6;
        let expected = f64::midpoint(feed.close[day * 6 - 1], feed.close[(day - 1) * 6 - 1]);
        assert_eq!(result.plots[0].values[bar], Value::Float(expected));
    }
}

#[test]
fn descending_pure_loop_also_works_in_a_scalar_request() {
    let source = "//@version=6\nindicator(\"Descending\")\nf()=>\n    x=0.0\n    for i=3 to 0\n        x+=close[i]\n    x\na=request.security(syminfo.tickerid,\"1D\",f()[1],lookahead=barmerge.lookahead_on)\nplot(a)\n";
    let feed = data();
    let result = compile_and_run_with_libraries(
        source,
        &[],
        &feed,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().unwrap().success,
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    for bar in 24..360 {
        let day = bar / 6;
        let expected = (0..4)
            .rev()
            .map(|i| feed.close[(day - i) * 6 - 1])
            .sum::<f64>();
        assert_eq!(result.plots[0].values[bar], Value::Float(expected));
    }
}

#[test]
fn forming_daily_prices_cannot_change_the_confirmed_tuple() {
    let mut feed = data();
    let libraries = vec![("test/Kernel/2".into(), library())];
    let original = compile_and_run_with_libraries(
        &source(),
        &libraries,
        &feed,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    for i in 354..360 {
        feed.open[i] = 9000.0;
        feed.close[i] = 9000.0;
        feed.high[i] = 9001.0;
        feed.low[i] = 8999.0;
    }
    let changed = compile_and_run_with_libraries(
        &source(),
        &libraries,
        &feed,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(original.runtime_errors.is_empty() && changed.runtime_errors.is_empty());
    assert!(
        original.compile.as_ref().unwrap().success && changed.compile.as_ref().unwrap().success
    );
    for lane in 0..3 {
        assert_eq!(
            original.plots[lane].values[354..],
            changed.plots[lane].values[354..]
        );
    }
}

#[test]
fn unsupported_or_unbounded_helper_bodies_are_rejected() {
    for body in [
        "    var float x = 0.0\n    x += close\n    [x,x>0]",
        "    log.info(\"effect\")\n    [close,close>open]",
        "    x = 0.0\n    for i=0 to 300\n        x += close\n    [x,x>0]",
        "    x = 0.0\n    for i=0 to int(close)\n        x += close\n    [x,x>0]",
        "    x = close\n    for i=0 to 100\n        x := x+x\n    [x,x>0]",
        "    x = close\n    for i=0 to 3\n        x += x[1]\n    [x,x>0]",
    ] {
        let source = format!("//@version=6\nindicator(\"Rejected\")\nf()=>\n{body}\n[a,b]=request.security(syminfo.tickerid,\"1D\",f())\nplot(a)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "unexpected admission: {source}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0244")),
            "{:?}",
            result.diagnostics
        );
    }
}

#[test]
fn varip_helper_state_is_rejected_by_bounded_htf_expansion() {
    let source = r#"//@version=6
indicator("Varip helper")
f() =>
    varip float seed = 0.0
    seed := seed + close
    total = 0.0
    for i=0 to 2
        total := total + seed
    [total, total > 0]
[value, positive] = request.security(syminfo.tickerid, "1D", f())
plot(value)
plot(positive ? 1.0 : 0.0)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(
        !result.success,
        "varip helper state was expanded into a fresh series"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0244")),
        "expected a bounded HTF refusal, got {:?}",
        result.diagnostics
    );
}

#[test]
fn scalar_request_ignores_an_unused_pure_string_helper_local() {
    let source = r#"//@version=6
indicator("Scalar helper")
f() =>
    unused = str.tostring(close)
    close
requested = request.security(syminfo.tickerid, "1D", f())
plot(requested)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(
        result.success,
        "unused pure non-numeric helper local changed scalar HTF support: {:?}",
        result.diagnostics
    );
}

#[test]
fn unsupported_scalar_helper_reports_scalar_context() {
    let source = r#"//@version=6
indicator("Scalar helper effect")
f() =>
    log.info("effect")
    close
requested = request.security(syminfo.tickerid, "1D", f())
plot(requested)
"#;
    let result = compile(source, CompileOptions::default());
    assert!(
        !result.success,
        "effectful scalar helper prefix must fail closed"
    );
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("E0244"))
        .expect("scalar helper prefix should report E0244");
    assert!(diagnostic.message.contains("scalar"));
    assert!(!diagnostic.message.contains("tuple function"));
}

#[test]
fn library_var_global_is_rejected_in_an_htf_helper() {
    let library = r#"//@version=6
library("State")
var float carried = close
export read() => carried
"#;
    let source = r#"//@version=6
import test/State/1 as lib
indicator("Library state in HTF")
requested = request.security(syminfo.tickerid, "1D", lib.read())
plot(requested)
"#;
    let result = compile_with_libraries(
        source,
        &[("test/State/1".to_string(), library.to_string())],
        CompileOptions::default(),
    );
    assert!(
        !result.success,
        "library top-level var was reconstructed as a fresh HTF value"
    );
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.as_deref() == Some("E0244"))
        .expect("persistent HTF state should report E0244");
    assert!(
        diagnostic.message.contains("secondary"),
        "unsupported library state should have an explicit HTF diagnostic: {diagnostic:?}"
    );
}

#[test]
fn secondary_rsi_uses_selected_source_and_length_on_confirmed_daily_bars() {
    use pinewasm_compiler::{BuiltinSeries, InputValue};

    let intraday = data();
    let daily_close: Vec<_> = intraday.close.chunks_exact(6).map(|bars| bars[5]).collect();
    let daily_high: Vec<_> = intraday
        .high
        .chunks_exact(6)
        .map(|bars| bars.iter().copied().fold(f64::NEG_INFINITY, f64::max))
        .collect();
    let daily = MarketData {
        open: daily_close.clone(),
        high: daily_high,
        low: daily_close.clone(),
        volume: vec![6_000.0; daily_close.len()],
        time: intraday.time.iter().step_by(6).copied().collect(),
        timeframe: "1D".into(),
        close: daily_close,
        ..MarketData::default()
    };
    let script = r#"//@version=6
indicator("Requested RSI")
length = input.int(14, minval=1)
src = input.source(close)
tf = input.timeframe("1D")
plot(request.security(syminfo.tickerid, tf, ta.rsi(src, length)))
"#;
    for (length, source, source_name) in [
        (14, BuiltinSeries::Close, "close"),
        (7, BuiltinSeries::High, "high"),
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(length));
        options.inputs.insert(1, InputValue::Source(source));
        let requested = compile_and_run_with_libraries(
            script,
            &[],
            &intraday,
            CompileOptions::default(),
            &options,
        );
        assert!(
            requested.compile.as_ref().unwrap().success,
            "{:?}",
            requested.diagnostics
        );
        assert!(
            requested.runtime_errors.is_empty(),
            "{:?}",
            requested.runtime_errors
        );

        let reference = compile_and_run_with_libraries(
            &format!(
                "//@version=6\nindicator(\"Expected RSI\")\nplot(ta.rsi({source_name}, {length}))"
            ),
            &[],
            &daily,
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            reference.runtime_errors.is_empty(),
            "{:?}",
            reference.runtime_errors
        );
        for (i, actual) in requested.plots[0].values.iter().enumerate() {
            let expected = ((i + 1) / 6)
                .checked_sub(1)
                .map_or(&Value::Na, |bucket| &reference.plots[0].values[bucket]);
            assert_eq!(
                actual, expected,
                "bar {i}, source {source_name}, length {length}"
            );
        }
    }
}

#[test]
fn literal_helper_selectors_preserve_timeframe_and_wma_input_overrides() {
    use pinewasm_compiler::InputValue;

    let script = r#"//@version=6
indicator("Selected inputs")
tfA = input.timeframe("1D")
tfB = input.timeframe("1W")
lenA = input.int(2, minval=1)
lenB = input.int(9, minval=1)
requested(series string tf) => request.security(syminfo.tickerid, tf, close)
weighted(int length) => ta.wma(close, length)
selected(int index) =>
    tf = switch index
        0 => tfA
        1 => tfB
    length = index == 0 ? lenA : lenB
    [requested(tf), weighted(length)]
[tf0, wma0] = selected(0)
[tf1, wma1] = selected(1)
plot(tf0)
plot(wma0)
plot(tf1)
plot(wma1)
plot(requested(tfA))
plot(weighted(lenA))
plot(requested(tfB))
plot(weighted(lenB))
"#;
    let mut overridden = RunOptions::default();
    overridden.inputs.insert(0, InputValue::String("2D".into()));
    overridden.inputs.insert(1, InputValue::String("3D".into()));
    overridden.inputs.insert(2, InputValue::Int(4));
    overridden.inputs.insert(3, InputValue::Int(6));
    for options in [RunOptions::default(), overridden] {
        let result = compile_and_run_with_libraries(
            script,
            &[],
            &data(),
            CompileOptions::default(),
            &options,
        );
        assert!(
            result.compile.as_ref().unwrap().success,
            "{:?}",
            result.diagnostics
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        for (selected, direct) in result.plots[..4].iter().zip(&result.plots[4..]) {
            assert_eq!(selected.values, direct.values);
            assert!(selected.values.iter().any(|value| *value != Value::Na));
        }
        assert_ne!(result.plots[0].values, result.plots[2].values);
        assert_ne!(result.plots[1].values, result.plots[3].values);
    }
}

#[test]
fn bar_varying_selectors_do_not_specialize_to_one_input() {
    let script = r#"//@version=6
indicator("Per-bar selected inputs")
tfA = input.timeframe("1D")
tfB = input.timeframe("1W")
lenA = input.int(2, minval=1)
lenB = input.int(9, minval=1)
requested(series string tf) => request.security(syminfo.tickerid, tf, close)
selected(int index) =>
    tf = switch index
        0 => tfA
        => tfB
    length = index == 0 ? lenA : lenB
    [requested(tf), ta.wma(close, length)]
[tf, wma] = selected(bar_index % 2)
plot(tf)
plot(wma)
"#;
    let result = compile(script, CompileOptions::default());
    assert!(!result.success);
    for expected in ["E0242", "E0219"] {
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(expected)),
            "{:?}",
            result.diagnostics
        );
    }
}

/// A `ta.ema` length computed from inputs is a run-time length on the chart,
/// so the chart series follows the selected input. The secondary context of
/// a `request.security` compound has no run-time lengths: it keeps the length
/// the inputs' defaults give, in every shape of the call, as it did while
/// that length was folded at compile time, and reports each one (`W0010`).
/// The script compiles either way.
#[test]
fn a_computed_ema_length_in_a_request_keeps_the_length_its_defaults_give() {
    use pinewasm_compiler::InputValue;

    let script = r#"//@version=6
indicator("Requested computed EMA")
n = input.int(4, minval=1)
m = n * 2 - 3
ema(simple int length) => ta.ema(close, length)
e = ta.ema(close, n + 1)
plot(request.security(syminfo.tickerid, "1D", ta.ema(close, n + 1)))
plot(request.security(syminfo.tickerid, "1D", ta.ema(close, m)))
plot(request.security(syminfo.tickerid, "1D", ema(n + 1)))
plot(request.security(syminfo.tickerid, "1D", e))
plot(request.security(syminfo.tickerid, "1D", ta.ema(close, 5)))
plot(e)
plot(ta.ema(close, 5))
plot(ta.ema(close, 8))
"#;
    let market = data();
    for selected in [None, Some(7)] {
        let mut options = RunOptions::default();
        if let Some(n) = selected {
            options.inputs.insert(0, InputValue::Int(n));
        }
        let result = compile_and_run_with_libraries(
            script,
            &[],
            &market,
            CompileOptions::default(),
            &options,
        );
        assert!(
            result.compile.as_ref().unwrap().success,
            "{:?}",
            result.diagnostics
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        // Each of the four calculated lengths says that the request keeps it.
        let kept: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0010"))
            .collect();
        assert_eq!(kept.len(), 4, "{:?}", result.diagnostics);
        for warning in kept {
            assert!(
                warning.message.contains("inside a `request.*()` call"),
                "{warning:?}"
            );
        }
        let literal = &result.plots[4].values;
        assert!(
            literal.iter().any(|value| *value != Value::Na),
            "the daily EMA must produce values"
        );
        for requested in &result.plots[..4] {
            assert_eq!(&requested.values, literal, "selected: {selected:?}");
        }
        // On the chart the same call follows the input.
        let on_chart = if selected.is_some() { 7 } else { 6 };
        assert_eq!(
            result.plots[5].values, result.plots[on_chart].values,
            "selected: {selected:?}"
        );
        assert_ne!(result.plots[6].values, result.plots[7].values);
    }
}

#[test]
fn requested_enum_switch_runs_only_the_selected_average_on_daily_bars() {
    use pinewasm_compiler::InputValue;

    let script = r#"//@version=6
indicator("Requested average")
enum Ma
    sma
    wma
    ema
    unhandled
first = input.enum(Ma.sma)
other = input.enum(Ma.ema)
length = input.int(4, minval=1)
otherLength = input.int(6, minval=1)
requestedTf = input.timeframe("1D")
average(Ma choice, src, int n) =>
    switch choice
        Ma.sma => ta.sma(src, n)
        Ma.wma => ta.wma(src, n)
        Ma.ema => ta.ema(src, n)
requested(series string tf, Ma choice, src, int n) =>
    request.security(syminfo.tickerid, tf, average(choice, src, n))
pick(int index) =>
    choice = switch index
        0 => first
        1 => other
    selectedLength = switch index
        0 => length
        1 => otherLength
    requested(requestedTf, choice, close, selectedLength)
plot(pick(0))
plot(pick(1))
plot(request.security(syminfo.tickerid, "1D", ta.sma(close, length)))
plot(request.security(syminfo.tickerid, "1D", ta.wma(close, length)))
plot(request.security(syminfo.tickerid, "1D", ta.ema(close, length)))
plot(request.security(syminfo.tickerid, "1D", ta.sma(close, otherLength)))
plot(request.security(syminfo.tickerid, "1D", ta.wma(close, otherLength)))
plot(request.security(syminfo.tickerid, "1D", ta.ema(close, otherLength)))
"#;
    let market = data();
    for (first, second) in [(0, 2), (1, 0), (2, 1), (3, 1)] {
        let mut options = RunOptions::default();
        for (input, index) in [(0, first), (1, second)] {
            options.inputs.insert(
                input,
                InputValue::Enum {
                    name: "Ma".into(),
                    index,
                    member: ["sma", "wma", "ema", "unhandled"][index as usize].into(),
                    title: None,
                },
            );
        }
        options.inputs.insert(2, InputValue::Int(7));
        options.inputs.insert(3, InputValue::Int(11));
        let result = compile_and_run_with_libraries(
            script,
            &[],
            &market,
            CompileOptions::default(),
            &options,
        );
        assert!(
            result.compile.as_ref().unwrap().success,
            "{:?}",
            result.diagnostics
        );
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
        if first == 3 {
            assert!(result.plots[0]
                .values
                .iter()
                .all(|value| *value == Value::Na));
        } else {
            assert_eq!(
                result.plots[0].values,
                result.plots[first as usize + 2].values
            );
        }
        assert_eq!(
            result.plots[1].values,
            result.plots[second as usize + 5].values
        );
        assert_ne!(result.plots[0].values, result.plots[1].values);
    }
}
