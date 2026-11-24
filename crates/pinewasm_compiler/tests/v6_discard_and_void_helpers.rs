//! Three language rules that PineWASM applies:
//!
//! - `_` names an unused variable: it may be declared any number of times,
//!   in one scope or a tuple, and can never be read or reassigned;
//! - a helper called as a statement may end in a void effect call;
//! - `str.tostring(enum)` returns the member's title, or its name.

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, MarketData, RunOptions, RunResult,
};

fn market(closes: &[f64]) -> MarketData {
    let bars = closes.len();
    MarketData {
        open: vec![100.0; bars],
        high: closes.iter().map(|c| c.max(100.0) + 1.0).collect(),
        low: closes.iter().map(|c| c.min(100.0) - 1.0).collect(),
        close: closes.to_vec(),
        volume: vec![1000.0; bars],
        time: (0..bars)
            .map(|bar| 1_700_000_000_000 + bar as i64 * 60_000)
            .collect(),
        ..MarketData::default()
    }
}

fn run(source: &str, closes: &[f64], options: RunOptions) -> RunResult {
    let result = compile_and_run(source, &market(closes), CompileOptions::default(), &options);
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{source}\n{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{source}\n{:?}",
        result.runtime_errors
    );
    result
}

fn values(result: &RunResult, plot: usize) -> serde_json::Value {
    serde_json::to_value(&result.plots[plot].values).unwrap()
}

fn assert_rejected(source: &str, code: &str) {
    let result = compile(source, CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted:\n{source}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.is_error() && d.code.as_deref() == Some(code)),
        "{source}\nexpected {code}, got {:?}",
        result.diagnostics
    );
}

#[test]
fn underscore_may_be_declared_repeatedly_and_never_read() {
    let source = "//@version=6\nindicator(\"Discard\")\n\
        pair() => [close, open]\n\
        triple() => [high, low, close]\n\
        [_, lo, _] = triple()\n\
        [_, o] = pair()\n\
        _ = 1\n\
        _ = \"text\"\n\
        total = 0.0\n\
        for _ = 1 to 3\n    total += 1\n\
        plot(lo + o + total)\n";
    let result = run(source, &[100.0, 100.0], RunOptions::default());
    // low 99 + open 100 + three loop iterations.
    assert_eq!(values(&result, 0), serde_json::json!([202.0, 202.0]));
}

#[test]
fn underscore_cannot_be_read_or_reassigned() {
    assert_rejected(
        "//@version=6\nindicator(\"Read\")\n_ = 1\nplot(_)\n",
        "E0201",
    );
    assert_rejected(
        "//@version=6\nindicator(\"Reassign\")\n_ = 1\n_ := 2\nplot(close)\n",
        "E0217",
    );
}

#[test]
fn helpers_called_as_statements_may_end_in_a_void_effect() {
    let source = "//@version=6\nstrategy(\"Void tail\")\n\
        enter() =>\n    strategy.entry(\"L\", strategy.long)\n\
        leave() =>\n    strategy.close(\"L\")\n\
        note(string text) =>\n    log.info(text)\n\
        both() =>\n    note(\"bar\")\n\
        if bar_index == 1\n    enter()\n\
        if bar_index == 3\n    leave()\n\
        both()\n\
        plot(strategy.position_size)\n";
    let result = run(
        source,
        &[100.0, 101.0, 102.0, 103.0, 104.0],
        RunOptions::default(),
    );
    assert_eq!(result.logs.len(), 5);
    assert!(
        !result.strategy.as_ref().unwrap().trades.is_empty(),
        "the entry and exit fill"
    );
}

#[test]
fn branching_and_long_chains_of_void_tails_run_as_statements() {
    // A tail `if`/`switch` whose branches end in void effects, and a chain
    // of helpers longer than any fixed walk depth, each ending in `log.info`.
    let mut source = String::from(
        "//@version=6\nindicator(\"Void chains\")\n\
         report(int n) =>\n\
         \x20   if n > 1\n\
         \x20       log.info(\"high\")\n\
         \x20   else\n\
         \x20       switch n\n\
         \x20           0 => log.warning(\"zero\")\n\
         \x20           => log.info(\"one\")\n\
         step0() =>\n    log.info(\"chain\")\n",
    );
    for level in 1..=20 {
        source.push_str(&format!("step{level}() =>\n    step{}()\n", level - 1));
    }
    source.push_str("report(bar_index)\nstep20()\nplot(close)\n");
    let result = run(&source, &[100.0, 101.0, 102.0], RunOptions::default());
    let messages: Vec<_> = result.logs.iter().map(|log| log.message.as_str()).collect();
    assert_eq!(
        messages,
        ["zero", "chain", "one", "chain", "high", "chain"],
        "{source}"
    );
}

#[test]
fn recursive_branching_helpers_are_rejected_promptly() {
    // Every branch calls the helper again. Checking for a void tail must not
    // walk the branches exponentially before recursion is reported.
    let mut source = String::from("//@version=6\nindicator(\"Recursive\")\nf(int n) =>\n");
    for branch in 1..=6 {
        let keyword = if branch == 1 { "if" } else { "else if" };
        source.push_str(&format!("    {keyword} n == {branch}\n        f(n)\n"));
    }
    source.push_str("    else\n        f(n)\nf(1)\nplot(close)\n");
    assert_rejected(&source, "E0224");
}

#[test]
fn a_void_helper_still_has_no_value() {
    assert_rejected(
        "//@version=6\nindicator(\"Value\")\n\
         note() =>\n    log.info(\"x\")\n\
         x = note()\nplot(close)\n",
        "E0212",
    );
}

#[test]
fn str_tostring_of_an_enum_is_the_member_title() {
    let source = "//@version=6\nindicator(\"Enum text\")\n\
        enum Side\n    up = \"Rising\"\n    flat\n    down = \"Falling\"\n\
        side = close > open ? Side.up : close < open ? Side.down : Side.flat\n\
        var Side unset = na\n\
        const string named = str.tostring(Side.flat)\n\
        if str.tostring(side) != (close > open ? \"Rising\" : close < open ? \"Falling\" : \"flat\")\n\
        \x20   runtime.error(\"wrong title\")\n\
        if named != \"flat\" or str.tostring(unset) != \"NaN\"\n\
        \x20   runtime.error(\"wrong const or na text\")\n\
        plot(close)\n";
    run(source, &[101.0, 99.0, 100.0], RunOptions::default());
}

#[test]
fn a_helper_may_end_by_discarding_a_value() {
    let source = "//@version=6\nindicator(\"Discard tail\")\n\
        touch() =>\n    var int calls = 0\n    calls += 1\n    _ = close\n\
        maybe(bool flag) =>\n    if flag\n        _ = open\n    else\n        _ = high\n\
        touch()\n\
        maybe(close > open)\n\
        plot(close)\n";
    run(source, &[100.0, 101.0], RunOptions::default());
    // A discard gives the helper no value to use.
    assert_rejected(
        "//@version=6\nindicator(\"Discard value\")\n\
         f() =>\n    _ = close\n\
         x = f()\nplot(close)\n",
        "E0227",
    );
}

#[test]
fn a_const_alias_of_an_enum_member_folds_to_its_title() {
    let source = "//@version=6\nindicator(\"Enum alias\")\n\
        enum Side\n    up = \"Rising\"\n    down\n\
        const Side choice = Side.up\n\
        const string label = str.tostring(choice)\n\
        length = input.int(3, title = str.tostring(choice))\n\
        if label != \"Rising\"\n    runtime.error(label)\n\
        plot(close + length)\n";
    run(source, &[100.0, 101.0], RunOptions::default());
}

#[test]
fn void_tails_through_methods_and_object_calls_run_as_statements() {
    let source = "//@version=6\nindicator(\"Method tails\")\n\
        type Logger\n    string tag\n\
        method say(Logger this) =>\n    log.info(this.tag)\n\
        emit(Logger l) =>\n    l.say()\n\
        emit_new() =>\n    Logger.new(\"fresh\").say()\n\
        emit(Logger.new(\"a\"))\n\
        emit_new()\n\
        plot(close)\n";
    let result = run(source, &[100.0, 101.0], RunOptions::default());
    let messages: Vec<_> = result.logs.iter().map(|log| log.message.as_str()).collect();
    assert_eq!(messages, ["a", "fresh", "a", "fresh"]);
}

#[test]
fn parameters_named_underscore_are_unused() {
    let source = "//@version=6\nindicator(\"Parameters\")\n\
        pick(float _, float b, int _) => b\n\
        plot(pick(close, open, 3))\n";
    let result = run(source, &[101.0, 99.0], RunOptions::default());
    assert_eq!(values(&result, 0), serde_json::json!([100.0, 100.0]));
    assert_rejected(
        "//@version=6\nindicator(\"Read\")\nf(int _) => _ * 2\nplot(f(1))\n",
        "E0201",
    );
}

#[test]
fn a_large_enum_writes_its_titles_without_deep_nesting() {
    // A non-constant member selects its title through a balanced choice, so
    // its depth grows with log2 of the member count.
    let members: String = (0..5000)
        .map(|i| format!("    m{i} = \"T{i}\"\n"))
        .collect();
    let source = format!(
        "//@version=6\nindicator(\"Large enum\")\n\
         enum Big\n{members}\
         var Big unset = na\n\
         value = bar_index == 0 ? Big.m4321 : bar_index == 1 ? Big.m0 : unset\n\
         text = str.tostring(value)\n\
         expected = bar_index == 0 ? \"T4321\" : bar_index == 1 ? \"T0\" : \"NaN\"\n\
         if text != expected\n\
         \x20   runtime.error(text)\n\
         plot(close)\n"
    );
    run(&source, &[100.0, 101.0, 102.0], RunOptions::default());
}

#[test]
fn a_tuple_result_standing_alone_is_rejected_not_lowered() {
    // A tuple can only be destructured; written as a statement, directly or
    // as a helper's tail, it is E0225 rather than reaching lowering.
    let header = "//@version=6\nindicator(\"Tuple\")\n\
        method bounds(array<float> a) => [a.min(), a.max()]\n\
        var arr = array.from(1.0, 2.0)\n";
    for tail in [
        "f(array<float> a) =>\n    a.bounds()\nf(arr)\n",
        "f(array<float> a) =>\n    bounds(a)\nf(arr)\n",
        "f(array<float> a, bool c) =>\n    if c\n        a.bounds()\nf(arr, true)\n",
        "arr.bounds()\n",
    ] {
        assert_rejected(&format!("{header}{tail}plot(close)\n"), "E0225");
    }
}
