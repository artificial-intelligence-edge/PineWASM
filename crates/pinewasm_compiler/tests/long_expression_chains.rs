//! Long operator chains, long postfix chains and deep nesting end as an
//! ordinary diagnostic or a successful compile, never as a stack overflow.
//!
//! The browser package overflowed its 1 MiB stack on
//! `x = close + close + ...` with 500 terms: the parser's operator loop built
//! a tree one level deeper per term without recursing, and the analyzer then
//! recursed once per level. The same stack is reached by nesting spread over
//! user functions, because a call is analyzed by inlining the callee's body.
//!
//! Each case compiles on a thread with an explicit stack, so that an
//! overflow aborts this test binary instead of depending on how much stack
//! the harness happens to give a test.

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, CompileResult, MarketData, RunOptions,
};

/// Well past the 500 terms that first overflowed the browser package.
const LONG: usize = 10_000;

/// Rust's default stack for a spawned thread, stated so that `RUST_MIN_STACK`
/// cannot hide a regression. An unoptimized build needs a little under three
/// fifths of it for the deepest input the compiler accepts (a 198-term chain;
/// a 190-level ternary needs about half).
const STACK_BYTES: usize = 2 * 1024 * 1024;

const TOO_DEEP: &str = "nested too deeply";

fn on_bounded_stack<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(work)
        .expect("spawn the compile thread")
        .join()
        .expect("the compile thread panicked")
}

fn script(body: &str) -> String {
    format!(
        "//@version=6\n\
         indicator(\"Chains\")\n\
         type Link\n    float v = 0\n    Link next\n\
         method inc(float v) => v + 1\n\
         first() => Link.new()\n\
         {body}\n"
    )
}

fn compile_script(body: &str) -> CompileResult {
    let source = script(body);
    on_bounded_stack(move || compile(&source, CompileOptions::default()))
}

fn chain(term: &str, link: &str, terms: usize) -> String {
    vec![term; terms].join(link)
}

#[track_caller]
fn assert_refused(what: &str, body: &str) {
    let result = compile_script(body);
    assert!(!result.success, "{what}: compiled");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains(TOO_DEEP)),
        "{what}: expected a `{TOO_DEEP}` diagnostic, got {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
}

/// Compile and run `body` over two bars with `close` 2.0 and 3.0, returning
/// the first plot as JSON.
#[track_caller]
fn plotted(what: &str, body: &str) -> String {
    let source = script(body);
    let result = on_bounded_stack(move || {
        let market = MarketData {
            open: vec![1.0, 1.0],
            high: vec![3.0, 3.0],
            low: vec![1.0, 1.0],
            close: vec![2.0, 3.0],
            volume: vec![1.0; 2],
            ..MarketData::default()
        };
        compile_and_run(
            &source,
            &market,
            CompileOptions::default(),
            &RunOptions::default(),
        )
    });
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{what}: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{what}: {:?}",
        result.runtime_errors
    );
    serde_json::to_string(&result.plots[0].values).expect("plot values serialize")
}

#[test]
fn the_reported_sum_is_refused_with_a_diagnostic() {
    // The defect as reported: 500 terms, and far beyond it.
    for terms in [500, LONG, 60_000] {
        let body = format!("x = {}\nplot(x)", chain("close", " + ", terms));
        assert_refused(&format!("{terms} terms"), &body);
    }
}

#[test]
fn long_operator_chains_are_refused_with_a_diagnostic() {
    for operator in ["-", "*", "/", "%"] {
        let body = format!(
            "x = {}\nplot(x)",
            chain("close", &format!(" {operator} "), LONG)
        );
        assert_refused(operator, &body);
    }
    for operator in ["and", "or"] {
        let body = format!(
            "x = {}\nplot(x ? 1 : 0)",
            chain("close > open", &format!(" {operator} "), LONG)
        );
        assert_refused(operator, &body);
    }
    for operator in ["==", "!=", "<", "<=", ">", ">="] {
        let body = format!(
            "x = {}\nplot(x ? 1 : 0)",
            chain("close", &format!(" {operator} "), LONG)
        );
        assert_refused(operator, &body);
    }
    let mixed = chain("close * open - high / low % 7", " + ", LONG);
    assert_refused("mixed arithmetic", &format!("x = {mixed}\nplot(x)"));
    let logic = chain(
        "close > open and not (high < low) or close == open",
        " or ",
        LONG,
    );
    assert_refused("mixed logic", &format!("x = {logic}\nplot(x ? 1 : 0)"));
    let text = chain("\"a\" + str.tostring(close)", " + ", LONG);
    assert_refused(
        "string concatenation",
        &format!("x = {text}\nplot(str.length(x))"),
    );
}

#[test]
fn long_postfix_and_ternary_chains_are_refused_with_a_diagnostic() {
    let methods = format!("x = (close){}\nplot(x)", ".inc()".repeat(LONG));
    assert_refused("method chain", &methods);
    let unknown = format!("x = close{}\nplot(x)", ".nope()".repeat(LONG));
    assert_refused("unknown method chain", &unknown);
    let fields = format!("x = first(){}.v\nplot(x)", ".next".repeat(LONG));
    assert_refused("field chain", &fields);
    let history = format!("x = close{}\nplot(x)", "[1]".repeat(LONG));
    assert_refused("history chain", &history);
    let ternary = format!(
        "x = {}close\nplot(x)",
        "close > open ? open : ".repeat(LONG)
    );
    assert_refused("ternary chain", &ternary);
    let negated = format!("x = {}close\nplot(x)", "- ".repeat(LONG));
    assert_refused("prefix chain", &negated);
    let grouped = format!("x = {}close{}\nplot(x)", "(".repeat(LONG), ")".repeat(LONG));
    assert_refused("nested parentheses", &grouped);
    let calls = format!(
        "x = {}close{}\nplot(x)",
        "math.abs(".repeat(LONG),
        ")".repeat(LONG)
    );
    assert_refused("nested calls", &calls);
}

#[test]
fn long_argument_lists_do_not_nest() {
    // Arguments are siblings, so their count costs no stack. A built-in with
    // a fixed arity answers with its ordinary arity diagnostic.
    let result = compile_script(&format!(
        "x = math.max({})\nplot(x)",
        chain("close", ", ", LONG)
    ));
    assert!(!result.success);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("argument")),
        "{:?}",
        result.diagnostics.first()
    );
    assert!(result
        .diagnostics
        .iter()
        .all(|d| !d.message.contains(TOO_DEEP)));

    // `array.from` takes up to 32 values; more is its own diagnostic, at any
    // count.
    let wide = |count| {
        format!(
            "a = array.from({})\nplot(array.size(a))",
            chain("close + open * 2", ", ", count)
        )
    };
    assert_eq!(plotted("array.from", &wide(32)), "[32.0,32.0]");
    let result = compile_script(&wide(LONG));
    assert!(!result.success);
    assert!(result
        .diagnostics
        .iter()
        .all(|d| !d.message.contains(TOO_DEEP)));
}

#[test]
fn the_longest_accepted_chains_compile_and_run() {
    // 198 terms is the longest chain `x = ...` holds (see the parser's
    // `MAX_EXPR_DEPTH`): the analyzer is at its deepest here.
    let sum = format!("x = {}\nplot(x)", chain("close", " + ", 198));
    assert_eq!(plotted("sum", &sum), "[396.0,594.0]");
    assert_refused(
        "sum",
        &format!("x = {}\nplot(x)", chain("close", " + ", 199)),
    );

    // Each comparison is itself two levels tall, so one term fewer fits.
    let all = format!(
        "x = {}\nplot(x ? 1 : 0)",
        chain("close > open", " and ", 197)
    );
    assert_eq!(plotted("and", &all), "[1.0,1.0]");

    let text = format!("x = {}\nplot(str.length(x))", chain("\"ab\"", " + ", 198));
    assert_eq!(plotted("concatenation", &text), "[396.0,396.0]");

    // A method call is charged two units of the analyzer's budget.
    let methods = format!("x = (close){}\nplot(x)", ".inc()".repeat(90));
    assert_eq!(plotted("methods", &methods), "[92.0,93.0]");

    let ternary = format!("x = {}close\nplot(x)", "close < open ? open : ".repeat(190));
    assert_eq!(plotted("ternary", &ternary), "[2.0,3.0]");

    let mut nested = String::from("var float x = 0\n");
    for depth in 0..90 {
        nested.push_str(&"    ".repeat(depth));
        nested.push_str("if close > open\n");
    }
    nested.push_str(&"    ".repeat(90));
    nested.push_str("x := close\nplot(x)");
    assert_eq!(plotted("nested blocks", &nested), "[2.0,3.0]");
}

/// `depth` collection keywords nested around `int`: `open` is the text up to
/// the nested argument (`array<`, `map<string, `) and `close` what ends it.
fn nested_type(open: &str, close: &str, depth: usize) -> String {
    format!("{}int{}", open.repeat(depth), close.repeat(depth))
}

/// A type nested too deeply is refused as a type: with the type diagnostic,
/// and without the statement being read again as an over-long expression.
#[track_caller]
fn assert_type_refused(what: &str, body: &str) {
    let result = compile_script(body);
    assert!(!result.success, "{what}: compiled");
    let messages: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages.contains(&"type is nested too deeply"),
        "{what}: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("expression")),
        "{what}: {messages:?}"
    );
}

/// The three collection keywords, each nested through the argument that can
/// hold a collection.
const TYPE_NESTINGS: [(&str, &str); 3] = [("array<", ">"), ("matrix<", ">"), ("map<string, ", ">")];

#[test]
fn deeply_nested_types_are_refused_with_a_diagnostic() {
    // A declared type is an axis of its own: one level per `<...>` argument
    // list, wherever the type is written. One level past the parser's cap
    // (`MAX_TYPE_DEPTH`, 200) and far past it are both a diagnostic.
    for (open, close) in TYPE_NESTINGS {
        for depth in [201, 60_000] {
            let ty = nested_type(open, close, depth);
            let what = |position: &str| format!("{open}...{close}, {depth} levels, {position}");
            assert_type_refused(
                &what("field"),
                &format!("type Deep\n    {ty} held\n    int count\nplot(close)"),
            );
            assert_type_refused(
                &what("parameter"),
                &format!("deep({ty} held) => 1\nplot(close)"),
            );
            assert_type_refused(&what("variable"), &format!("{ty} held = na\nplot(close)"));
            assert_type_refused(
                &what("persistent variable"),
                &format!("var {ty} held = na\nplot(close)"),
            );
        }
    }
}

#[test]
fn the_deepest_accepted_types_compile_and_run() {
    // 200 levels is the deepest type the parser admits: the analyzer resolves
    // every level of it here, and each later pass carries the result.
    for (open, close) in TYPE_NESTINGS {
        let ty = nested_type(open, close, 200);
        let variable = format!("{ty} held = na\nplot(na(held) ? 1 : 0)");
        assert_eq!(plotted(open, &variable), "[1.0,1.0]");
        let field =
            format!("type Deep\n    {ty} held\ndeep = Deep.new()\nplot(na(deep.held) ? 1 : 0)");
        assert_eq!(plotted(open, &field), "[1.0,1.0]");
        let parameter = format!("missing({ty} held) => na(held) ? 1 : 0\nplot(missing(na))");
        assert_eq!(plotted(open, &parameter), "[1.0,1.0]");
    }

    // The same type at the bottom of the deepest block nesting the analyzer
    // admits: the two axes are spent together.
    let ty = nested_type("array<", ">", 200);
    let mut nested = String::from("var float x = 0\n");
    for depth in 0..90 {
        nested.push_str(&"    ".repeat(depth));
        nested.push_str("if close > open\n");
    }
    nested.push_str(&"    ".repeat(90));
    nested.push_str(&format!("{ty} held = na\n"));
    nested.push_str(&"    ".repeat(90));
    nested.push_str("x := na(held) ? close : 0\nplot(x)");
    assert_eq!(plotted("nested blocks", &nested), "[2.0,3.0]");
}

/// `levels` functions, each calling the previous one from the deepest point
/// of a `terms`-term sum.
fn summing_functions(levels: usize, terms: usize) -> String {
    let mut body = String::from("f0() => close\n");
    for level in 1..=levels {
        body.push_str(&format!(
            "f{level}() => f{}(){}\n",
            level - 1,
            " + close".repeat(terms - 1)
        ));
    }
    body.push_str(&format!("plot(f{levels}())"));
    body
}

#[test]
fn nesting_spread_over_function_calls_is_refused_with_a_diagnostic() {
    // Every function here is within the parser's caps; the depth comes from
    // inlining one inside the next. Each of these overflowed the browser
    // package's stack.
    for (levels, terms) in [(3, 190), (10, 100), (60, 20)] {
        assert_refused(
            &format!("{levels} functions of {terms} terms"),
            &summing_functions(levels, terms),
        );
    }

    let mut blocks = String::from("var float acc = 0\ng0() =>\n    acc := acc + 1\n    acc\n");
    for level in 1..=4 {
        blocks.push_str(&format!("g{level}() =>\n"));
        for depth in 0..100 {
            blocks.push_str(&"    ".repeat(depth + 1));
            blocks.push_str("if close > open\n");
        }
        blocks.push_str(&"    ".repeat(101));
        blocks.push_str(&format!("g{}()\n    acc\n", level - 1));
    }
    blocks.push_str("plot(g4())");
    assert_refused("4 functions of 100 nested blocks", &blocks);
}

#[test]
fn moderate_nesting_spread_over_function_calls_still_compiles() {
    // Four functions of 40 terms: 160 levels in all, under the budget. Each
    // adds 39 terms to the one it calls: 157 times `close`.
    assert_eq!(
        plotted("4 functions of 40 terms", &summing_functions(4, 40)),
        "[314.0,471.0]"
    );
    // 60 functions deep is close to the separate 64-level call limit.
    assert_eq!(
        plotted("60 functions of 2 terms", &summing_functions(60, 2)),
        "[122.0,183.0]"
    );
}

#[test]
fn flat_else_if_chains_keep_the_block_limit_they_had_before_the_budget() {
    // `else if` is stored as an else block holding one `if`. The parser caps
    // that chain with its block-nesting limit (200), as it did before the
    // analyzer budget, and the analyzer does not charge the link again: a
    // flat chain of 190 arms compiles, and 3000 arms is still the parser's
    // diagnostic, never a stack overflow.
    let chain = |arms: usize| {
        let mut body = String::from("x = 0.0\nif close > 0\n    x := 0\n");
        for arm in 1..arms {
            body.push_str(&format!("else if close > {arm}\n    x := {arm}\n"));
        }
        body.push_str("plot(x)");
        body
    };
    assert_eq!(plotted("190 arms", &chain(190)), "[0.0,0.0]");
    let result = compile_script(&chain(3000));
    assert!(!result.success);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains(TOO_DEEP)),
        "{:?}",
        result.diagnostics.first()
    );
}

/// `functions` functions, each a flat `if / else if` chain of `arms` arms whose
/// final `else` calls the next function. Every link counts toward one call path.
fn chains_calling_each_other(functions: usize, arms: usize) -> String {
    let mut body = String::new();
    for function in 0..functions {
        body.push_str(&format!("f{function}() =>\n"));
        body.push_str("    if close > 0\n        x = 0\n");
        for arm in 1..arms {
            body.push_str(&format!("    else if close > {arm}\n        x = {arm}\n"));
        }
        // The call sits inside the last arm, so every link is still open when
        // the next function's chain is analyzed.
        if function + 1 < functions {
            body.push_str(&format!("    else\n        f{}()\n", function + 1));
        } else {
            body.push_str("    else\n        x = 1\n");
        }
        body.push_str("    0\n");
    }
    body.push_str("plot(f0())");
    body
}

#[test]
fn else_if_chains_summed_through_calls_are_refused_with_a_diagnostic() {
    // Six chains of 40 arms put 39 links per function on one call path: 234
    // links, over the shared budget. A diagnostic, not a stack overflow (this overflowed the browser stack
    // when the links were uncharged).
    let result = compile_script(&chains_calling_each_other(6, 40));
    assert!(!result.success);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains(TOO_DEEP)),
        "{:?}",
        result.diagnostics.first()
    );
}

#[test]
fn else_if_chains_under_the_cap_summed_through_calls_still_compile() {
    // Four chains of 40 arms: 156 links on one path, under the shared budget.
    assert_eq!(
        plotted("4 chains of 40 arms", &chains_calling_each_other(4, 40)),
        "[0.0,0.0]"
    );
}

#[test]
fn else_if_links_and_expressions_share_one_budget() {
    // A 180-arm chain whose deepest arm holds a 60-term sum: each alone is
    // under its old limit, but together they are over the shared budget.
    // They must be refused with a diagnostic, not overflow the stack.
    let mut body = String::from("x = 0.0\nif close > 0\n    x := 0\n");
    for arm in 1..180 {
        body.push_str(&format!("else if close > {arm}\n    x := {arm}\n"));
    }
    body.push_str(&format!("else\n    x := {}\n", chain("close", " + ", 60)));
    body.push_str("plot(x)");
    let result = compile_script(&body);
    assert!(!result.success);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains(TOO_DEEP)),
        "{:?}",
        result.diagnostics.first()
    );
}
