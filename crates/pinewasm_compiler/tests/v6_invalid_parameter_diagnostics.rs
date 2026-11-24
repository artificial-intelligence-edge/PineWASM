//! One fixture per class of invalid parameter, each asserting its exact
//! diagnostic code, message, label and source span.
//!
//! The expected outcome of every case follows the parameter types in the
//! compiler's built-in signature table. For example `plot(title)` allows only
//! `const string`, `indicator(overlay)` only `const bool`,
//! `strategy(pyramiding)` only `const int`, and `timeframe.change(timeframe)`
//! allows `series string`.
//!
//! The codes follow `pinewasm_span::codes`:
//!
//! - wrong type: `E0205`, "`f` argument `p` must be T, found U";
//! - a right-typed value whose qualifier is stronger than the parameter
//!   allows: `E0206`, "`f` argument `p` must be const or weaker, found Q".
//!   This covers both a series/simple value where `const` is required and an
//!   `input` value given to a const-only parameter.
//!
//! Two families keep their own codes and are asserted as such:
//! `input.*` declaration arguments (`E0230`) and stateful `ta.*` const
//! arguments (`E0219`).
//!
//! Every case must report exactly one error, with a span that covers exactly
//! the offending argument, and no cascade (no second error, no spurious
//! "no declaration" `W0001`).

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, IncrementalSession, InputValue, MarketData,
    RunOptions, Tick, Value,
};
use pinewasm_span::Severity;
use std::collections::HashMap;

/// One invalid-parameter fixture.
struct Case {
    source: &'static str,
    /// The exact source text the primary label must cover.
    span_text: &'static str,
    /// Text that occurs exactly once in `source` and starts with
    /// `span_text`; it locates the span when `span_text` alone is not
    /// unique (a one-letter alias also appears where it is declared).
    /// Empty means `span_text` itself.
    anchor: &'static str,
    code: &'static str,
    message: &'static str,
    label: &'static str,
}

fn assert_case(case: &Case) {
    let result = compile(case.source, CompileOptions::default());
    assert!(!result.success, "expected a rejection: {}", case.source);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error for {:?}, got {:?}",
        case.source,
        result.diagnostics
    );
    let error = errors[0];
    assert_eq!(error.code.as_deref(), Some(case.code), "{}", case.source);
    assert_eq!(error.message, case.message, "{}", case.source);
    let label = error.labels.first().expect("a primary source label");
    assert_eq!(label.message, case.label, "{}", case.source);
    let anchor = if case.anchor.is_empty() {
        case.span_text
    } else {
        case.anchor
    };
    assert!(anchor.starts_with(case.span_text));
    assert_eq!(
        case.source.matches(anchor).count(),
        1,
        "anchor {anchor:?} must be unique in the fixture"
    );
    let start = case.source.find(anchor).unwrap() as u32;
    let end = start + case.span_text.len() as u32;
    assert_eq!(
        (label.span.start, label.span.end),
        (start, end),
        "span of {:?} in {:?}",
        case.span_text,
        case.source
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("W0001")),
        "a declaration error must not also report a missing declaration: {:?}",
        result.diagnostics
    );
}

// ---------------------------------------------------------------------------
// Class 1: a non-constant value where the parameter requires `const`.
// ---------------------------------------------------------------------------

#[test]
fn series_value_for_a_const_parameter_is_e0206() {
    let cases = [
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(close, title=str.tostring(close))\n",
            span_text: "str.tostring(close)",
            anchor: "",
            code: "E0206",
            message: "`plot` argument `title` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nhline(50, title=str.tostring(close))\n",
            span_text: "str.tostring(close)",
            anchor: "",
            code: "E0206",
            message: "`hline` argument `title` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\na = plot(close)\nb = plot(open)\nfill(a, b, fillgaps=close > open)\n",
            span_text: "close > open",
            anchor: "",
            code: "E0206",
            message: "`fill` argument `fillgaps` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(str.tostring(bar_index))\nplot(close)\n",
            span_text: "str.tostring(bar_index)",
            anchor: "",
            code: "E0206",
            message: "`indicator` argument `title` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nlibrary(str.tostring(bar_index))\nexport f() => 1\n",
            span_text: "str.tostring(bar_index)",
            anchor: "",
            code: "E0206",
            message: "`library` argument `title` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\", overlay=close > open)\nplot(close)\n",
            span_text: "close > open",
            anchor: "",
            code: "E0206",
            message: "`indicator` argument `overlay` must be const or weaker, found series",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\", precision=bar_index)\nplot(close)\n",
            span_text: "bar_index",
            anchor: "",
            code: "E0206",
            message: "`indicator` argument `precision` must be const or weaker, found series",
            label: "expected a const value",
        },
        // `syminfo.currency` is `simple string`.
        Case {
            source: "//@version=6\nstrategy(\"s\", currency=syminfo.currency)\nplot(close)\n",
            span_text: "syminfo.currency",
            anchor: "",
            code: "E0206",
            message: "`strategy` argument `currency` must be const or weaker, found simple",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nstrategy(\"s\", pyramiding=bar_index)\nplot(close)\n",
            span_text: "bar_index",
            anchor: "",
            code: "E0206",
            message: "`strategy` argument `pyramiding` must be const or weaker, found series",
            label: "expected a const value",
        },
        // `input.*` arguments keep their own declaration code.
        Case {
            source: "//@version=6\nindicator(\"t\")\nx = input.int(bar_index)\nplot(x)\n",
            span_text: "bar_index",
            anchor: "",
            code: "E0230",
            message: "`input.int` argument `defval` must be a const int expression",
            label: "expected a const int expression",
        },
    ];
    for case in &cases {
        assert_case(case);
    }
}

// ---------------------------------------------------------------------------
// Class 2: a value of the wrong type.
// ---------------------------------------------------------------------------

#[test]
fn wrong_type_is_e0205() {
    let cases = [
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(close, title=5)\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`plot` argument `title` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nplotshape(true, size=5)\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`plotshape` argument `size` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nindicator(\"t\", overlay=1)\nplot(close)\n",
            span_text: "1",
            anchor: "",
            code: "E0205",
            message: "`indicator` argument `overlay` must be bool, found int",
            label: "expected bool",
        },
        Case {
            source: "//@version=6\nindicator(5)\nplot(close)\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`indicator` argument `title` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nlibrary(5)\nexport f() => 1\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`library` argument `title` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nstrategy(\"s\", currency=5)\nplot(close)\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`strategy` argument `currency` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nstrategy(\"s\", pyramiding=\"x\")\nplot(close)\n",
            span_text: "\"x\"",
            anchor: "",
            code: "E0205",
            message: "`strategy` argument `pyramiding` must be int, found string",
            label: "expected int",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(request.security(syminfo.tickerid, 5, close))\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`request.security` argument `timeframe` must be string, found int",
            label: "expected string",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(array.size(request.security_lower_tf(syminfo.tickerid, 5, close)))\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`request.security_lower_tf` argument `timeframe` must be string, found int",
            label: "expected string",
        },
        // `timeframe.change` takes a `series string` timeframe, so only the
        // type is wrong here.
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(timeframe.change(5) ? 1 : 0)\n",
            span_text: "5",
            anchor: "",
            code: "E0205",
            message: "`timeframe.change` argument `timeframe` must be string, found int",
            label: "expected string",
        },
        // Stateful `ta.*` const arguments keep their own code.
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(ta.pivothigh(2, \"x\"))\n",
            span_text: "\"x\"",
            anchor: "",
            code: "E0219",
            message: "invalid `ta.pivothigh` argument `rightbars`: must be an int literal",
            label: "this argument is fixed at compile time",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nx = input.int(\"a\")\nplot(x)\n",
            span_text: "\"a\"",
            anchor: "",
            code: "E0230",
            message: "`input.int` argument `defval` must be a const int expression",
            label: "expected a const int expression",
        },
    ];
    for case in &cases {
        assert_case(case);
    }
}

// ---------------------------------------------------------------------------
// Class 3: an `input` value for a const-only parameter.
// ---------------------------------------------------------------------------

#[test]
fn input_value_for_a_const_only_parameter_is_e0206() {
    let cases = [
        Case {
            source: "//@version=6\nindicator(\"t\")\nplot(close, force_overlay=input.bool(true))\n",
            span_text: "input.bool(true)",
            anchor: "",
            code: "E0206",
            message: "`plot` argument `force_overlay` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nsel = input.bool(true)\nplot(close, force_overlay=sel ? true : false)\n",
            span_text: "sel ? true : false",
            anchor: "",
            code: "E0206",
            message: "`plot` argument `force_overlay` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nplotshape(true, size=input.string(size.small))\n",
            span_text: "input.string(size.small)",
            anchor: "",
            code: "E0206",
            message: "`plotshape` argument `size` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nplotchar(true, size=input.string(size.small))\n",
            span_text: "input.string(size.small)",
            anchor: "",
            code: "E0206",
            message: "`plotchar` argument `size` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\na = plot(close)\nb = plot(open)\nfill(a, b, fillgaps=input.bool(true))\n",
            span_text: "input.bool(true)",
            anchor: "",
            code: "E0206",
            message: "`fill` argument `fillgaps` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nT = input.string(\"x\")\nplot(close, title=T)\n",
            span_text: "T",
            anchor: "T)",
            code: "E0206",
            message: "`plot` argument `title` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nT = input.string(\"x\")\nindicator(T)\nplot(close)\n",
            span_text: "T",
            anchor: "T)",
            code: "E0206",
            message: "`indicator` argument `title` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nB = input.bool(false)\nindicator(\"t\", behind_chart=B)\nplot(close)\n",
            span_text: "B",
            anchor: "B)",
            code: "E0206",
            message: "`indicator` argument `behind_chart` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nN = input.int(2)\nstrategy(\"s\", pyramiding=N)\nplot(close)\n",
            span_text: "N",
            anchor: "N)",
            code: "E0206",
            message: "`strategy` argument `pyramiding` must be const or weaker, found input",
            label: "expected a const value",
        },
        Case {
            source: "//@version=6\nindicator(\"t\")\nx = input.int(input.int(1))\nplot(x)\n",
            span_text: "input.int(1)",
            anchor: "",
            code: "E0230",
            message: "`input.int` argument `defval` must be a const int expression",
            label: "expected a const int expression",
        },
    ];
    for case in &cases {
        assert_case(case);
    }
}

/// `plot.force_overlay` takes a const value.
/// Pine's variable-declaration rule includes control structures in qualifier
/// inference, so an input-controlled write cannot keep const qualification.
/// This test enforces that rule.
#[test]
fn input_conditioned_reassignment_reaching_a_const_only_parameter() {
    let source = "//@version=6\nindicator(\"t\")\nsel = input.bool(true)\nX = true\nif not sel\n    X := false\nplot(close, force_overlay=X)\n";
    let result = compile(source, CompileOptions::default());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0206")),
        "{:?}",
        result.diagnostics
    );
}

// ---------------------------------------------------------------------------
// Valid spellings that the parameter types allow: the checks above must not reject them.
// ---------------------------------------------------------------------------

fn compiles(source: &str) {
    let result = compile(source, CompileOptions::default());
    assert!(
        result.success,
        "expected {source:?} to compile, got {:?}",
        result.diagnostics
    );
}

#[test]
fn const_aliases_are_valid_for_const_parameters() {
    // `library(title)` is `const string`; a const alias is a const string.
    compiles("//@version=6\nconst string T = \"Lib\"\nlibrary(T)\nexport f() => 1\n");
    compiles("//@version=6\nconst string T = \"Ind\"\nindicator(T)\nplot(close)\n");
    compiles("//@version=6\nconst int P = 2\nindicator(\"t\", precision=P)\nplot(close)\n");
    compiles("//@version=6\nconst int N = 2\nstrategy(\"s\", pyramiding=N)\nplot(close)\n");
}

#[test]
fn an_untyped_na_title_is_reported_once() {
    // `indicator(na)` reports one E0220 and no follow-on W0001.
    let source = "//@version=6\nindicator(na)\nplot(close)\n";
    let result = compile(source, CompileOptions::default());
    let codes: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| d.code.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(codes, vec!["E0220"], "{:?}", result.diagnostics);
    let label = &result.diagnostics[0].labels[0];
    let start = source.find("na)").unwrap() as u32;
    assert_eq!((label.span.start, label.span.end), (start, start + 2));
}

// ---------------------------------------------------------------------------
// `timeframe.change` with a `series string` timeframe.
// ---------------------------------------------------------------------------

/// 2024-01-01T00:00:00Z, a Monday.
const START_MS: i64 = 1_704_067_200_000;
const HOUR_MS: i64 = 3_600_000;
/// Sixteen days of hourly bars.
const BARS: usize = 24 * 16;

fn hourly_market() -> MarketData {
    let close: Vec<f64> = (0..BARS).map(|i| 100.0 + (i % 7) as f64).collect();
    MarketData {
        open: close.clone(),
        high: close.iter().map(|c| c + 1.0).collect(),
        low: close.iter().map(|c| c - 1.0).collect(),
        close,
        volume: vec![1_000.0; BARS],
        time: (0..BARS as i64).map(|i| START_MS + i * HOUR_MS).collect(),
        timeframe: "60".into(),
        ..MarketData::default()
    }
}

fn flags(source: &str, options: &RunOptions) -> Vec<bool> {
    let run = compile_and_run(source, &hourly_market(), CompileOptions::default(), options);
    let compile = run.compile.as_ref().expect("compile result");
    assert!(compile.success, "{source}: {:?}", compile.diagnostics);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    run.plots[0]
        .values
        .iter()
        .map(|value| match value {
            Value::Int(v) => *v == 1,
            Value::Float(v) => *v == 1.0,
            other => panic!("unexpected plot value {other:?}"),
        })
        .collect()
}

fn literal(tf: &str) -> Vec<bool> {
    flags(
        &format!("//@version=6\nindicator(\"t\")\nplot(timeframe.change(\"{tf}\") ? 1 : 0)\n"),
        &RunOptions::default(),
    )
}

#[test]
fn timeframe_change_accepts_an_input_timeframe() {
    // Computed by hand from the rule that the flag is true on the first bar
    // of a new timeframe: with hourly UTC bars from midnight, a new day
    // starts on every 24th bar, and the first bar starts one.
    let daily: Vec<bool> = (0..BARS).map(|i| i % 24 == 0).collect();
    assert_eq!(literal("D"), daily);

    let source = "//@version=6\nindicator(\"t\")\ntf = input.timeframe(\"D\")\nplot(timeframe.change(tf) ? 1 : 0)\n";
    assert_eq!(flags(source, &RunOptions::default()), daily);

    // The input is evaluated on every run, not baked: overriding it to a
    // week gives the same bars as the literal "W".
    let mut inputs = HashMap::new();
    inputs.insert(0, InputValue::String("W".into()));
    let weekly = flags(
        source,
        &RunOptions {
            inputs,
            ..RunOptions::default()
        },
    );
    assert_eq!(weekly, literal("W"));
    assert_ne!(weekly, daily);
}

#[test]
fn timeframe_change_accepts_a_computed_timeframe_in_a_lazy_branch() {
    // Shape under test: the timeframe is an
    // `if` expression over inputs and `timeframe.period`, and the call sits
    // in the lazy arm of a ternary. That arm runs on every bar here, so the
    // result must equal the literal "D".
    let source = "//@version=6\nindicator(\"t\")\nuseDay = input.bool(true)\nTF = if useDay and timeframe.in_seconds(timeframe.period) <= timeframe.in_seconds(\"60\")\n    \"D\"\nelse\n    \"W\"\nc = TF == \"M\" ? false : timeframe.change(TF)\nplot(c ? 1 : 0)\n";
    assert_eq!(flags(source, &RunOptions::default()), literal("D"));

    // Two call sites keep separate state.
    let two = "//@version=6\nindicator(\"t\")\na = str.tostring(1) == \"1\" ? \"D\" : \"W\"\nb = str.tostring(1) == \"1\" ? \"W\" : \"D\"\nx = timeframe.change(a)\ny = timeframe.change(b)\nplot(x ? 1 : 0)\nplot(y ? 1 : 0)\n";
    let run = compile_and_run(
        two,
        &hourly_market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty());
    let as_flags = |values: &[Value]| -> Vec<bool> {
        values
            .iter()
            .map(|v| matches!(v, Value::Int(1)) || matches!(v, Value::Float(f) if *f == 1.0))
            .collect()
    };
    assert_eq!(as_flags(&run.plots[0].values), literal("D"));
    assert_eq!(as_flags(&run.plots[1].values), literal("W"));
}

#[test]
fn timeframe_change_in_a_loop_is_still_rejected() {
    let source = "//@version=6\nindicator(\"t\")\ntf = input.timeframe(\"D\")\nn = 0\nfor i = 0 to 1\n    if timeframe.change(tf)\n        n += 1\nplot(n)\n";
    let result = compile(source, CompileOptions::default());
    let error = result
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("an error");
    assert_eq!(error.code.as_deref(), Some("E0222"));
    assert_eq!(
        error.message,
        "stateful `timeframe.change` is not allowed inside a loop body"
    );
    let start = source.find("timeframe.change(tf)").unwrap() as u32;
    assert_eq!(
        (error.labels[0].span.start, error.labels[0].span.end),
        (start, start + "timeframe.change(tf)".len() as u32)
    );
}

/// Plot 0 of `source` as flags, failing on any compile or runtime error.
fn plot_flags(source: &str, market: &MarketData, plot: usize) -> Vec<bool> {
    let run = compile_and_run(
        source,
        market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let compile = run.compile.as_ref().expect("compile result");
    assert!(compile.success, "{source}: {:?}", compile.diagnostics);
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    run.plots[plot]
        .values
        .iter()
        .map(|v| matches!(v, Value::Int(1)) || matches!(v, Value::Float(f) if *f == 1.0))
        .collect()
}

#[test]
fn timeframe_change_with_a_runtime_timeframe_keeps_per_call_state_like_the_literal() {
    // Each shape gives the runtime-timeframe form state of its own per call
    // site and per function call, and runs it only when the call runs, as
    // the literal form does (both lower onto `time(timeframe)` with a
    // hidden `var` per call site). A shared or always-run hidden `var` would
    // differ from the literal on the skipped bars.
    let market = hourly_market();
    let shapes = [
        "c = bar_index % 5 == 0 ? false : timeframe.change(TF)\nplot(c ? 1 : 0)\n",
        "c = bar_index % 5 != 0 and timeframe.change(TF)\nplot(c ? 1 : 0)\n",
        "c = false\nif bar_index % 5 != 0\n    c := timeframe.change(TF)\nplot(c ? 1 : 0)\n",
        "f(x) => timeframe.change(x)\na = f(TF)\nb = bar_index % 3 == 0 ? f(TF) : false\nplot(b ? 1 : 0)\nplot(a ? 1 : 0)\n",
        "g(x) =>\n    var n = 0\n    if timeframe.change(x)\n        n += 1\n    n\nplot(bar_index % 2 == 0 ? g(TF) : -1)\n",
        "c = timeframe.change(TF)\nplot(c[1] ? 1 : 0)\n",
    ];
    for tf in ["D", "W", "240", "30S"] {
        for shape in shapes {
            let literal = format!(
                "//@version=6\nindicator(\"t\")\n{}",
                shape.replace("TF", &format!("\"{tf}\""))
            );
            let runtime = format!(
                "//@version=6\nindicator(\"t\")\ntf = bar_index >= 0 ? \"{tf}\" : \"1\"\n{}",
                shape.replace("TF", "tf")
            );
            assert_eq!(
                plot_flags(&runtime, &market, 0),
                plot_flags(&literal, &market, 0),
                "{tf}: {shape}"
            );
        }
    }
}

#[test]
fn timeframe_change_with_a_switching_timeframe_compares_bucket_opening_times() {
    // A bucket is identified by its opening time, the value `time(timeframe)`
    // returns, not by `floor(time / size)`: quotients taken with different
    // sizes are not comparable. Bars are hourly from 2024-01-01 00:00 UTC, so
    // bar `i` opens at hour `i`. The timeframe switches from "60" to "120" on
    // bar 25 (01:00 on day 2). That bar is inside the 2-hour bucket that
    // opened with bar 24, which is the bucket bar 24 reported, so no new
    // period starts on bar 25. From then on a change is reported on each
    // even-hour bar, where a 2-hour bucket opens.
    let market = hourly_market();
    let up = "//@version=6\nindicator(\"t\")\ntf = bar_index < 25 ? \"60\" : \"120\"\nplot(timeframe.change(tf) ? 1 : 0)\n";
    let expected: Vec<bool> = (0..BARS).map(|i| i < 25 || i % 2 == 0).collect();
    assert_eq!(plot_flags(up, &market, 0), expected);

    // Back from "120" to "60" on bar 25: bar 25 opens a new 1-hour bucket
    // (01:00) that differs from the 2-hour bucket bar 24 reported (00:00).
    let down = "//@version=6\nindicator(\"t\")\ntf = bar_index < 25 ? \"120\" : \"60\"\nplot(timeframe.change(tf) ? 1 : 0)\n";
    let expected: Vec<bool> = (0..BARS).map(|i| i >= 25 || i % 2 == 0).collect();
    assert_eq!(plot_flags(down, &market, 0), expected);
}

#[test]
fn timeframe_change_with_an_input_timeframe_matches_the_literal_on_realtime_ticks() {
    // Realtime ticks re-execute the forming bar: the hidden previous bucket
    // must roll back like the literal form's, so every tick of the first bar
    // of a new day reports a change and no later tick does.
    let run = |source: &str| -> (Vec<String>, Vec<Value>) {
        let program = compile(source, CompileOptions::default())
            .program
            .expect("compiles");
        let mut history = hourly_market();
        let committed = 47;
        for lane in [
            &mut history.open,
            &mut history.high,
            &mut history.low,
            &mut history.close,
            &mut history.volume,
        ] {
            lane.truncate(committed);
        }
        history.time.truncate(committed);
        let mut session = IncrementalSession::new(&program, &history, &RunOptions::default())
            .expect("history seeds a session");
        let mut samples = Vec::new();
        for bar in committed..committed + 28 {
            for tick in 0..3 {
                let forming = Tick {
                    bar_index: bar,
                    open: 100.0,
                    high: 101.0,
                    low: 99.0,
                    close: 100.0 + tick as f64,
                    volume: 10.0,
                    time: START_MS + bar as i64 * HOUR_MS,
                    trading_day: None,
                    is_bar_close: tick == 2,
                };
                let sample = session
                    .advance_samples(std::slice::from_ref(&forming))
                    .expect("the tick executes");
                samples.push(format!("{bar}.{tick}={:?}", sample[0].values[0]));
            }
        }
        (samples, session.result().plots[0].values.clone())
    };
    let literal = run("//@version=6\nindicator(\"t\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n");
    let input = run(
        "//@version=6\nindicator(\"t\")\ntf = input.timeframe(\"D\")\nplot(timeframe.change(tf) ? 1 : 0)\n",
    );
    assert_eq!(input, literal);
    // Bars 48 and 72 open a new UTC day: all three ticks of each are true.
    let changed: Vec<&String> = literal
        .0
        .iter()
        .filter(|s| s.ends_with("=Int(1)") || s.ends_with("=Float(1.0)"))
        .collect();
    assert_eq!(changed.len(), 6, "{:?}", literal.0);
}
