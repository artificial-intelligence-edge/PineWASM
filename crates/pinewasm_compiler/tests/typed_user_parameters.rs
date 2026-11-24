//! A user function's declared parameter types: what a call may pass for a
//! typed parameter, and what its default may be.
//!
//! Two rules meet here. `int / int` is statically `int` and keeps its
//! fraction, so a quotient is a valid `int` argument and a valid `int`
//! default. A value whose type is `float` is not. A default of the wrong
//! type is a mistake in the definition, so it is an error whether or not any
//! call omits the argument.

use pinewasm_compiler::{
    compile, compile_and_run, compile_with_libraries, CompileOptions, CompileResult, MarketData,
    RunOptions, Severity, SourceId, Value,
};

const HEADER: &str = "//@version=6\nindicator(\"Typed parameters\")\n";

fn script(body: &str) -> String {
    format!("{HEADER}{body}\n")
}

/// Six bars whose closes are 10, 11, ..., 15.
fn rising_data() -> MarketData {
    let close: Vec<f64> = (0..6).map(|bar| 10.0 + f64::from(bar)).collect();
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![1.0; 6],
        ..MarketData::default()
    }
}

/// The values of the script's only plot, one per bar.
fn plotted(body: &str) -> Vec<Value> {
    let source = script(body);
    let run = compile_and_run(
        &source,
        &rising_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().is_some_and(|c| c.success),
        "{body}: {:?}",
        run.diagnostics
    );
    assert!(
        run.runtime_errors.is_empty(),
        "{body}: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.plots.len(), 1, "{body}");
    run.plots[0].values.clone()
}

/// The messages of the errors `result` reports, in order.
fn error_messages(result: &CompileResult) -> Vec<String> {
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

/// `body` reports exactly one error: `E0205` with `message`, whose primary
/// label says `expected <expected>` and covers exactly `span_text` where it
/// follows `before` (`before` + `span_text` occurs once in the script).
fn assert_e0205(body: &str, message: &str, expected: &str, before: &str, span_text: &str) {
    let source = script(body);
    let result = compile(&source, CompileOptions::default());
    assert!(!result.success, "expected a rejection: {body}");
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error for {body:?}, got {:?}",
        result.diagnostics
    );
    let error = errors[0];
    assert_eq!(error.code.as_deref(), Some("E0205"), "{body}");
    assert_eq!(error.message, message, "{body}");
    let label = error.labels.first().expect("a primary source label");
    assert_eq!(label.message, format!("expected {expected}"), "{body}");
    let anchor = format!("{before}{span_text}");
    assert_eq!(source.matches(&anchor).count(), 1, "{body}: {anchor:?}");
    let start = source.find(&anchor).unwrap() + before.len();
    assert_eq!(label.span.source, SourceId::MAIN, "{body}");
    assert_eq!(
        (label.span.start as usize, label.span.end as usize),
        (start, start + span_text.len()),
        "span of {span_text:?} in {body:?}"
    );
}

/// [`assert_e0205`] for a default, written `= <default>` in `body`.
fn assert_mistyped_default(body: &str, message: &str, expected: &str, default: &str) {
    assert_e0205(body, message, expected, "= ", default);
}

#[test]
fn an_int_quotient_is_an_int_argument_and_keeps_its_fraction() {
    // `7 / 2` is statically int, so each call binds; its value is 3.5.
    assert_eq!(
        plotted("f(int a) => a * 2\nplot(f(7 / 2))"),
        vec![Value::Float(7.0); 6]
    );
    assert_eq!(
        plotted("f(int a = 7 / 2) => a\nplot(f())"),
        vec![Value::Float(3.5); 6]
    );
    // Through a variable, a named argument and a `float` parameter as well.
    assert_eq!(
        plotted("f(int a) => a * 2\nint n = 7 / 2\nplot(f(a = n))"),
        vec![Value::Float(7.0); 6]
    );
    assert_eq!(
        plotted("f(float a) => a * 2\nplot(f(7 / 2))"),
        vec![Value::Float(7.0); 6]
    );
    assert_eq!(
        plotted("f(float a = 7 / 2) => a\nplot(f())"),
        vec![Value::Float(3.5); 6]
    );
}

#[test]
fn an_integer_only_use_of_an_int_parameter_reads_the_whole_part() {
    // A size and a history offset can only use a whole number: 3 of 3.5.
    assert_eq!(
        plotted("f(int n) => array.new_float(n)\nplot(array.size(f(7 / 2)))"),
        vec![Value::Float(3.0); 6]
    );
    assert_eq!(
        plotted("f(int n) => close[n]\nplot(f(7 / 2))"),
        vec![
            Value::Na,
            Value::Na,
            Value::Na,
            Value::Float(10.0),
            Value::Float(11.0),
            Value::Float(12.0),
        ]
    );
}

#[test]
fn a_float_is_not_an_int_argument() {
    // The same calls with a `float` operand: the quotient is a float.
    for (body, param, argument) in [
        ("f(int a) => a * 2\nplot(f(7 / 2.0))", "a", "7 / 2.0"),
        (
            "f(int a) => a * 2\nplot(f(float(7) / 2))",
            "a",
            "float(7) / 2",
        ),
        ("f(int a) => a * 2\nplot(f(3.5))", "a", "3.5"),
        ("f(int a) => a * 2\nfloat n = 3\nplot(f(n))", "a", "n"),
        (
            "f(int n) => array.new_float(n)\nplot(array.size(f(7 / 2.0)))",
            "n",
            "7 / 2.0",
        ),
        ("f(int n) => close[n]\nplot(f(close))", "n", "close"),
    ] {
        assert_e0205(
            body,
            &format!("`f` argument `{param}` must be int, found float"),
            "int",
            "f(",
            argument,
        );
    }
}

#[test]
fn a_mistyped_default_is_an_error_at_the_definition() {
    let message = "`f` default for parameter `a` must be int, found float";
    // Never called, called with the argument, and called without it.
    for calls in ["plot(close)", "plot(f(1))", "plot(f())"] {
        assert_mistyped_default(
            &format!("f(int a = 3.5) => a\n{calls}"),
            message,
            "int",
            "3.5",
        );
    }
}

#[test]
fn every_call_that_omits_a_mistyped_default_reports_that_default() {
    // The calls that evaluate the default report it; nothing else does.
    let source = script("f(int a = 3.5) => a\nplot(f() + f())\nplot(f())\nplot(f(1))");
    let result = compile(&source, CompileOptions::default());
    assert!(!result.success);
    let default = source.find("3.5").unwrap() as u32;
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 3, "{:?}", result.diagnostics);
    for error in errors {
        assert_eq!(error.code.as_deref(), Some("E0205"));
        assert_eq!(
            error.message,
            "`f` default for parameter `a` must be int, found float"
        );
        let span = error.labels.first().expect("a primary source label").span;
        assert_eq!((span.start, span.end), (default, default + 3));
    }
}

#[test]
fn a_mistyped_default_adds_no_second_error_where_the_call_is_an_argument() {
    // The call's own error is the only one: the rule for the argument it
    // sits in adds none of its own.
    for (source, message, default) in [
        (
            "//@version=6\nf(string s = 1) => s\nindicator(f())\nplot(close)\n",
            "`f` default for parameter `s` must be string, found int",
            "1",
        ),
        (
            "//@version=6\nindicator(\"Scale\", scale = f())\nf(string s = 2) => s\nplot(close)\n",
            "`f` default for parameter `s` must be string, found int",
            "2",
        ),
        (
            "//@version=6\nf(int a = 1.5) => a\nstrategy(\"Pyramiding\", pyramiding = f())\nplot(close)\n",
            "`f` default for parameter `a` must be int, found float",
            "1.5",
        ),
        (
            "//@version=6\nindicator(\"Title\")\nf(string s = 3) => s\nplot(close, title = f())\n",
            "`f` default for parameter `s` must be string, found int",
            "3",
        ),
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(!result.success, "{source}");
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{source}: {:?}", result.diagnostics);
        assert_eq!(errors[0].code.as_deref(), Some("E0205"), "{source}");
        assert_eq!(errors[0].message, message, "{source}");
        let span = errors[0].labels.first().expect("a primary source label").span;
        let start = source.find(&format!("= {default})")).unwrap() + 2;
        assert_eq!(
            (span.start as usize, span.end as usize),
            (start, start + default.len()),
            "{source}"
        );
    }
}

#[test]
fn a_default_is_checked_against_every_kind_of_declared_type() {
    for (definition, param, expected, found, span_text) in [
        ("f(int a = \"x\") => a", "a", "int", "string", "\"x\""),
        ("f(int a = true) => a", "a", "int", "bool", "true"),
        ("f(int a = -1.5) => a", "a", "int", "float", "-1.5"),
        ("f(int a = (2.5)) => a", "a", "int", "float", "(2.5)"),
        ("f(int a = 7 / 2.0) => a", "a", "int", "float", "7 / 2.0"),
        ("f(int a = 1 < 2) => a", "a", "int", "bool", "1 < 2"),
        (
            "f(int a = true ? 1 : 2.5) => a",
            "a",
            "int",
            "float",
            "true ? 1 : 2.5",
        ),
        ("f(simple int a = 2.5) => a", "a", "int", "float", "2.5"),
        ("f(float a = \"x\") => a", "a", "float", "string", "\"x\""),
        ("f(bool b = 1) => b", "b", "bool", "int", "1"),
        ("f(string s = 1) => s", "s", "string", "int", "1"),
        ("f(color c = 1) => c", "c", "color", "int", "1"),
        (
            "f(string s = #ff0000) => s",
            "s",
            "string",
            "color",
            "#ff0000",
        ),
        ("f(array<int> a = 5) => 1", "a", "array<int>", "int", "5"),
        ("f(line l = 5) => 1", "l", "line", "int", "5"),
        ("f(Point p = 5) => 1", "p", "Point", "int", "5"),
        ("f(Side e = 5) => 1", "e", "Side", "int", "5"),
    ] {
        assert_mistyped_default(
            &format!("type Point\n    float x\nenum Side\n    long\n    short\n{definition}\nplot(close)"),
            &format!("`f` default for parameter `{param}` must be {expected}, found {found}"),
            expected,
            span_text,
        );
    }
}

#[test]
fn every_mistyped_default_of_a_definition_is_reported() {
    let source = script("f(int a = 1.5, float b = 2, string s = 3) => a\nplot(close)");
    let result = compile(&source, CompileOptions::default());
    assert_eq!(
        error_messages(&result),
        [
            "`f` default for parameter `a` must be int, found float",
            "`f` default for parameter `s` must be string, found int",
        ]
    );
}

#[test]
fn a_method_default_is_checked_like_a_function_default() {
    // Called with `k`, and without it. Either way the message names the
    // method as its definition does, not as the call spells it (`int.scaled`).
    for call in ["x.scaled(2)", "x.scaled()"] {
        assert_mistyped_default(
            &format!("method scaled(int self, int k = 2.5) => self * k\nx = 1\nplot({call})"),
            "`scaled` default for parameter `k` must be int, found float",
            "int",
            "2.5",
        );
    }
}

#[test]
fn a_method_receiver_default_is_checked_though_no_call_can_omit_it() {
    // Never called, and called: a call always passes the receiver.
    for calls in ["plot(close)", "x = 1\nplot(x.same())"] {
        assert_mistyped_default(
            &format!("method same(int self = 1.5) => self\n{calls}"),
            "`same` default for parameter `self` must be int, found float",
            "int",
            "1.5",
        );
    }
    assert_mistyped_default(
        "method count(array<float> values = 5) => array.size(values)\nplot(close)",
        "`count` default for parameter `values` must be array<float>, found int",
        "array<float>",
        "5",
    );
    // A receiver default of the receiver's type is accepted, as before.
    assert_eq!(
        plotted("method same(float self = 1) => self\nx = 2.5\nplot(x.same())"),
        vec![Value::Float(2.5); 6]
    );
}

#[test]
fn the_default_of_an_overload_no_call_selects_is_checked() {
    assert_mistyped_default(
        "f(int a) => 1\nf(string s, int k = 2.5) => 2\nplot(f(1))",
        "`f` default for parameter `k` must be int, found float",
        "int",
        "2.5",
    );
    let fitting = compile(
        &script("f(int a) => 1\nf(string s, int k = 2) => 2\nplot(f(1))"),
        CompileOptions::default(),
    );
    assert!(fitting.success, "{:?}", fitting.diagnostics);
}

#[test]
fn a_rejected_redefinition_is_not_checked_for_its_defaults() {
    // The second `f` is no function: only the first one's default counts.
    let source = script("f(int a = 1.5) => a\nf(int a = 2.5) => a\nplot(close)");
    let result = compile(&source, CompileOptions::default());
    assert_eq!(
        error_messages(&result),
        [
            "function `f` is already defined",
            "`f` default for parameter `a` must be int, found float",
        ]
    );
    let default = result
        .diagnostics
        .iter()
        .find(|d| d.message.contains("default for parameter"))
        .and_then(|d| d.labels.first())
        .expect("the default's label")
        .span;
    assert_eq!(default.start as usize, source.find("1.5").unwrap());
}

#[test]
fn an_unevaluated_default_is_reported_after_the_rest_of_the_script() {
    // The call is refused for its own reason before it reaches the default,
    // so the default is checked once the statements are done.
    let result = compile(
        &script("f(int a = 1.5, int b = 2) => a + b\nplot(f(1, 2, 3))\nplot(missing)"),
        CompileOptions::default(),
    );
    assert_eq!(
        error_messages(&result),
        [
            "`f` expects 0 to 2 argument(s), found 3",
            "unknown identifier `missing`",
            "`f` default for parameter `a` must be int, found float",
        ]
    );
}

#[test]
fn a_default_that_is_not_well_typed_is_left_to_the_call_that_evaluates_it() {
    // What is wrong with these is not the declared type. Unevaluated, they
    // compile as before; evaluated, the analyzer reports its own error.
    for (definition, evaluated) in [
        (
            "f(int a = 1 and 2) => 0",
            "operator `and` cannot be applied to int and int",
        ),
        (
            "f(int a = not 1) => 0",
            "unary `not` requires bool, found int",
        ),
        (
            "f(int a = \"a\" < \"b\") => 0",
            "operator `<` cannot be applied to string and string",
        ),
        (
            "f(bool b = 1 ? 2 : 3) => 0",
            "`?:` condition must be bool, found int",
        ),
        // `na` takes its type from where it is used.
        (
            "f(string s = true ? 1 : na) => 0",
            "`f` default for parameter `s` must be string, found int",
        ),
        // The declared type does not exist, so nothing holds the default.
        ("f(Missing m = 1) => 0", "unknown field type `Missing`"),
    ] {
        let unevaluated = compile(
            &script(&format!("{definition}\nplot(close)")),
            CompileOptions::default(),
        );
        assert!(
            unevaluated.success,
            "{definition}: {:?}",
            unevaluated.diagnostics
        );
        let called = compile(
            &script(&format!("{definition}\nplot(f())")),
            CompileOptions::default(),
        );
        assert_eq!(error_messages(&called), [evaluated], "{definition}");
    }
}

#[test]
fn a_default_of_the_declared_type_is_accepted_whether_or_not_it_is_used() {
    for definition in [
        "f(int a = 7) => a",
        "f(int a = -7) => a",
        "f(int a = 7 / 2) => a",
        "f(int a = na) => a",
        "f(float a = 7) => a",
        "f(float a = 7 / 2) => a",
        "f(float a = 2.5) => a",
        "f(series float a = 1) => a",
        "f(bool b = 1 < 2) => b",
        "f(bool b = not true) => b",
        "f(string s = \"a\" + \"b\") => s",
        "f(color c = #ff0000) => c",
        "f(color c = color.red) => c",
        "f(array<int> a = na) => 1",
        "f(a = 3.5, b = 1) => a + b",
    ] {
        let result = compile(
            &script(&format!("{definition}\nplot(close)")),
            CompileOptions::default(),
        );
        assert!(result.success, "{definition}: {:?}", result.diagnostics);
    }
    assert_eq!(
        plotted("f(float a = 7, int b = -2) => a + b\nplot(f())"),
        vec![Value::Float(5.0); 6]
    );
}

#[test]
fn a_default_that_names_a_value_is_checked_where_a_call_omits_it() {
    // `close` has no type until it is evaluated. No call omits `a`: accepted.
    let unused = compile(
        &script("f(int a = close) => a\nplot(f(1))"),
        CompileOptions::default(),
    );
    assert!(unused.success, "{:?}", unused.diagnostics);
    // A call omits it: an error at the default.
    assert_mistyped_default(
        "f(int a = close) => a\nplot(f())",
        "`f` default for parameter `a` must be int, found float",
        "int",
        "close",
    );
}

#[test]
fn a_library_default_is_checked_whether_or_not_the_importer_calls_it() {
    let library = "//@version=6\nlibrary(\"Defaults\")\nexport scale(float x, int k = 2.5) => x * k\nexport twice(float x) => x * 2\nhalf(float x, string unit = 1) => x / 2\nexport method clamp(float self, bool strict = 0) => self\nexport method same(int self = 0.5) => self\n";
    let expected = [
        "`scale` default for parameter `k` must be int, found float",
        "`half` default for parameter `unit` must be string, found int",
        "`clamp` default for parameter `strict` must be bool, found int",
        "`same` default for parameter `self` must be int, found float",
    ];
    let alone = compile(library, CompileOptions::default());
    assert_eq!(error_messages(&alone), expected);

    // The importer calls none of them, then calls `scale` without `k`: the
    // call reports that default, and the rest follow as before.
    for call in ["d.twice(close)", "d.scale(close)"] {
        let main = script(&format!("import me/Defaults/1 as d\nplot({call})"));
        let imported = compile_with_libraries(
            &main,
            &[("me/Defaults/1".to_string(), library.to_string())],
            CompileOptions::default(),
        );
        assert!(!imported.success, "{call}");
        assert_eq!(error_messages(&imported), expected, "{call}");
        let covered: Vec<&str> = imported
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|error| {
                let label = error.labels.first().expect("a primary source label");
                assert_eq!(label.span.source, SourceId(1), "{call}");
                &library[label.span.start as usize..label.span.end as usize]
            })
            .collect();
        assert_eq!(covered, ["2.5", "1", "0", "0.5"], "{call}");
    }
}

#[test]
fn a_library_default_is_held_to_a_type_the_library_declares() {
    let library = "//@version=6\nlibrary(\"Shapes\")\nexport type Level\n    float price\nexport enum Side\n    long\n    short\nexport method raise(Level self = 1) => self\nexport pick(Side side = 2) => 0\nexport keep(Level level = na) => level\n";
    // The importer calls none of them.
    let imported = compile_with_libraries(
        &script("import me/Shapes/1 as s\nplot(close)"),
        &[("me/Shapes/1".to_string(), library.to_string())],
        CompileOptions::default(),
    );
    assert!(!imported.success);
    let errors: Vec<_> = imported
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 2, "{:?}", imported.diagnostics);
    for (error, (function, param, default)) in errors
        .iter()
        .zip([("raise", "self", "1"), ("pick", "side", "2")])
    {
        assert_eq!(error.code.as_deref(), Some("E0205"));
        // The message names the library's type; how is not this test's to pin.
        assert!(
            error.message.starts_with(&format!(
                "`{function}` default for parameter `{param}` must be "
            )) && error.message.ends_with(", found int"),
            "{}",
            error.message
        );
        let label = error.labels.first().expect("a primary source label");
        assert_eq!(label.span.source, SourceId(1));
        assert_eq!(
            &library[label.span.start as usize..label.span.end as usize],
            default
        );
    }
}
