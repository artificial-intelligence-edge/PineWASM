//! A user function's body and its default arguments resolve free names where
//! the function is defined, not where it is called.
//!
//! Pine v6 allows function definitions only at the top level, and a body sees
//! its own parameters and locals plus the globals declared above it. PineWASM
//! inlines every call at its call site, so the expanded body runs inside the
//! caller's scopes. These tests pin that the caller's parameters and block
//! locals stay invisible to the body and to an omitted parameter's default,
//! that a global declared below the definition stays invisible too, and that
//! arguments the caller writes still evaluate in the caller's scope. Calls to
//! other user functions do not depend on definition order.
//! `function_lexical_scope.rs` in `pinewasm_aot` runs the eligible scripts on
//! Standalone WebAssembly.

use pinewasm_compiler::{
    compile, compile_and_run, compile_and_run_with_libraries, compile_with_libraries,
    CompileOptions, MarketData, RunOptions, RunResult, Value,
};

const BARS: usize = 3;

fn market() -> MarketData {
    MarketData {
        open: vec![10.0; BARS],
        high: vec![12.0; BARS],
        low: vec![8.0; BARS],
        close: vec![11.0, 9.0, 10.0],
        volume: vec![100.0; BARS],
        time: (0..BARS)
            .map(|bar| 1_700_000_000_000 + bar as i64 * 60_000)
            .collect(),
        ..MarketData::default()
    }
}

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Function scope\")\n{body}\n")
}

/// Every plot holds one constant value; `expected[i]` is plot `i`'s value on
/// every bar.
fn assert_constant_plots(case: &str, run: &RunResult, expected: &[f64]) {
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{case}: {:?}",
        run.diagnostics
    );
    assert!(
        run.runtime_errors.is_empty(),
        "{case}: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.bars_executed as usize, BARS, "{case}");
    let actual: Vec<Vec<f64>> = run
        .plots
        .iter()
        .map(|plot| {
            plot.values
                .iter()
                .map(|value| match value {
                    Value::Float(value) => *value,
                    Value::Int(value) => *value as f64,
                    value => panic!("{case}: unexpected nonnumeric value {value:?}"),
                })
                .collect()
        })
        .collect();
    let expected: Vec<Vec<f64>> = expected.iter().map(|value| vec![*value; BARS]).collect();
    assert_eq!(actual, expected, "{case}");
}

fn run_main(case: &str, body: &str, expected: &[f64]) {
    let run = compile_and_run(
        &source(body),
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_constant_plots(case, &run, expected);
}

#[test]
fn a_body_reads_the_global_not_a_caller_parameter_of_the_same_name() {
    // `g`'s parameter `x` is a local of `g`. `f` was defined at the top level,
    // so its `x` is the global one.
    run_main(
        "body",
        "x = 1.0\nf() => x\ng(x) => f()\nplot(g(2.0), \"g\")",
        &[1.0],
    );
}

#[test]
fn a_default_reads_the_global_not_a_caller_parameter_of_the_same_name() {
    // A default belongs to the definition. Both binder paths that supply one
    // are covered: an omitted trailing parameter, and a parameter that a later
    // named argument skips.
    run_main(
        "omitted trailing parameter",
        "x = 1.0\nf(a = x) => a\ng(x) => f()\nplot(g(2.0))",
        &[1.0],
    );
    run_main(
        "parameter skipped by a named argument",
        "x = 1.0\nf(a = x, b = 0.0) => a + b\ng(x) => f(b = 0.5)\nplot(g(2.0))",
        &[1.5],
    );
}

#[test]
fn renaming_the_caller_parameter_leaves_both_results_unchanged() {
    // Without a name collision, both scope readings give the same result.
    run_main(
        "body without name collision",
        "x = 1.0\nf() => x\ng(y) => f()\nplot(g(2.0))",
        &[1.0],
    );
    run_main(
        "default without name collision",
        "x = 1.0\nf(a = x) => a\ng(y) => f()\nplot(g(2.0))",
        &[1.0],
    );
}

#[test]
fn arguments_written_by_the_caller_still_evaluate_in_the_caller() {
    // Only the default is the definition's. An argument the caller writes is
    // the caller's expression, so `g`'s `x` is passed in explicitly here.
    run_main(
        "explicit argument",
        "x = 1.0\nf(a = x) => a\ng(x) => f(x)\nplot(g(2.0))",
        &[2.0],
    );
    run_main(
        "parameter shadowing a global",
        "x = 1.0\nf(x) => x\nplot(f(2.0))",
        &[2.0],
    );
}

#[test]
fn a_block_local_in_the_caller_is_invisible_to_the_body_and_the_default() {
    run_main(
        "block local",
        "x = 1.0\nf() => x\nd(a = x) => a\nfloat body = na\nfloat dflt = na\nif bar_index >= 0\n    x = 5.0\n    body := f()\n    dflt := d()\nplot(body)\nplot(dflt)",
        &[1.0, 1.0],
    );
}

#[test]
fn every_level_of_inlining_resolves_at_its_own_definition() {
    // `h` and the method are expanded inside `g`, which binds `x`; neither
    // definition can see it. The locals each body declares stay its own.
    run_main(
        "intermediate helper",
        "x = 1.0\nf() => x\nh() => f()\ng(x) => h()\nplot(g(2.0))",
        &[1.0],
    );
    run_main(
        "method",
        "type Holder\n    float w\nx = 1.0\nmethod scaled(Holder self) => self.w * x\ng(x) =>\n    Holder h = Holder.new(3.0)\n    h.scaled()\nplot(g(2.0))",
        &[3.0],
    );
    run_main(
        "body local shadowing a global",
        "x = 1.0\nf() =>\n    x = 4.0\n    x\ng(x) => f()\nplot(g(2.0))",
        &[4.0],
    );
}

#[test]
fn a_name_only_a_caller_declares_is_unknown_in_the_body() {
    let text = source("f() => y\ng(y) => f()\nplot(g(2.0))");
    let compiled = compile(&text, CompileOptions::default());
    assert!(!compiled.success, "the body must not see `g`'s parameter");
    let at = text.find("=> y").expect("body reference") as u32 + 3;
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0201")
                && d.message.contains("`y`")
                && d.labels
                    .iter()
                    .any(|l| l.span.start == at && l.span.end == at + 1)
        }),
        "expected E0201 at the body's `y`: {:?}",
        compiled.diagnostics
    );
}

const SCOPE_LIBRARY: &str = r#"//@version=6
library("Scope")
float k = 1.0
float offset = 10.0
bump() => 100.0
peek() => k
export run(float k) => peek()
export shift(float v, float delta = offset) => v + delta
export jump(float v, float delta = bump()) => v + delta
"#;

fn run_with_library(case: &str, body: &str, expected: &[f64]) {
    let run = compile_and_run_with_libraries(
        &source(&format!("import me/Scope/1 as lib\n{body}")),
        &[("me/Scope/1".into(), SCOPE_LIBRARY.into())],
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_constant_plots(case, &run, expected);
}

#[test]
fn a_library_helper_reads_its_library_global_not_an_exported_parameter() {
    // `peek` is inlined inside `run`, whose parameter is also named `k`.
    run_with_library("library body", "plot(lib.run(2.0))", &[1.0]);
}

#[test]
fn a_library_default_resolves_in_the_library_namespace() {
    // The importer's same-named global must not replace the library's, and a
    // library-private helper must stay callable from a default.
    run_with_library(
        "importer shadows the library global",
        "float offset = 1.0\nplot(lib.shift(1.0))\nplot(lib.jump(1.0))\nplot(offset)",
        &[11.0, 101.0, 1.0],
    );
    run_with_library(
        "importer declares neither name",
        "plot(lib.shift(1.0))\nplot(lib.jump(1.0))",
        &[11.0, 101.0],
    );
}

#[test]
fn a_rejected_call_does_not_resolve_a_skipped_default_in_the_caller() {
    // `b = 1.0` skips `a`, whose default the binder fills in, and leaves `c`
    // without a value, so the call fails. Only the arity error belongs to the
    // call: the default is the library's and never resolves in the importer.
    let library = "//@version=6\nlibrary(\"Arity\")\nbump() => 100.0\nexport sum(float a = bump(), float b, float c) => a + b + c\n";
    let main = source("import me/Arity/1 as lib\nplot(lib.sum(b = 1.0))");
    let compiled = compile_with_libraries(
        &main,
        &[("me/Arity/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert!(!compiled.success);
    let codes: Vec<_> = compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.as_deref())
        .collect();
    assert_eq!(codes, [Some("E0224")], "{:?}", compiled.diagnostics);
}

/// `text` must fail to compile with a `code` diagnostic labelled at the first
/// `needle`, `skip` bytes into it and `len` bytes long.
fn assert_error_at(case: &str, text: &str, code: &str, needle: &str, skip: usize, len: usize) {
    let compiled = compile(text, CompileOptions::default());
    assert!(!compiled.success, "{case}: must not compile");
    let at = (text.find(needle).expect("needle in source") + skip) as u32;
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some(code)
                && d.labels
                    .iter()
                    .any(|l| l.span.start == at && l.span.end == at + len as u32)
        }),
        "{case}: expected {code} at {at}: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn a_global_declared_below_the_definition_is_unknown_in_the_body() {
    let text = source("f() => y\ny = 3.0\nplot(f())");
    assert_error_at("body", &text, "E0201", "=> y", 3, 1);
    let compiled = compile(&text, CompileOptions::default());
    let unknown = compiled
        .diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("E0201"))
        .expect("E0201");
    assert!(
        unknown
            .notes
            .iter()
            .any(|note| note.contains("declared below this function")),
        "the diagnostic should say why `y` is not visible: {unknown:?}"
    );
}

#[test]
fn a_global_declared_below_the_definition_is_unknown_in_a_default() {
    // Both binder paths that supply a default.
    assert_error_at(
        "omitted trailing parameter",
        &source("f(a = y) => a\ny = 3.0\nplot(f())"),
        "E0201",
        "= y)",
        2,
        1,
    );
    assert_error_at(
        "parameter skipped by a named argument",
        &source("f(a = y, b = 0.0) => a + b\ny = 3.0\nplot(f(b = 1.0))"),
        "E0201",
        "= y,",
        2,
        1,
    );
}

#[test]
fn a_global_declared_below_is_unknown_even_when_the_caller_sees_it() {
    // `g` is defined below `y`, so it may read `y`; `f` is defined above, so
    // it may not, although it is inlined into `g` after `y`'s declaration.
    let text = source("f() => y\ny = 3.0\ng() => f() + y\nplot(g())");
    assert_error_at(
        "helper inlined below the global",
        &text,
        "E0201",
        "=> y",
        3,
        1,
    );
    // The method form follows the same rule.
    let text = source(
        "type Holder\n    float w\nmethod scaled(Holder self) => self.w * k\nk = 2.0\nh = Holder.new(3.0)\nplot(h.scaled())",
    );
    assert_error_at("method", &text, "E0201", "* k", 2, 1);
}

#[test]
fn each_function_sees_the_globals_above_its_own_definition() {
    // `f` is defined below `x` and may read it, even though its caller `g` is
    // defined above `x`. A global between two definitions is visible to the
    // later one only.
    run_main(
        "callee below the global, caller above",
        "g() => f()\nx = 1.0\nf() => x\nplot(g())",
        &[1.0],
    );
    run_main(
        "global between two definitions",
        "f() => 2.0\nx = 1.0\ng() => x + f()\nplot(g())",
        &[3.0],
    );
}

#[test]
fn a_global_declared_below_with_a_built_in_name_is_the_built_in_in_the_body() {
    // `f` cannot see the later global `close`, so it reads the built-in, and a
    // script may not both shadow a built-in and use it (`E0211`).
    let text = source("f() => close\nclose = 3.0\nplot(f())");
    assert_error_at("shadow and use", &text, "E0211", "close = 3.0", 0, 5);
}

#[test]
fn reassigning_a_global_declared_below_is_still_a_global_write() {
    let text = source("f() =>\n    y := 1.0\n    y\ny = 0.0\nplot(f())");
    assert_error_at("reassignment", &text, "E0217", "y := 1.0", 0, 1);
    let compiled = compile(&text, CompileOptions::default());
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0217")
                && d.message.contains("global variable `y`")
                && d.notes
                    .iter()
                    .any(|note| note.contains("declared below this function"))
        }),
        "{:?}",
        compiled.diagnostics
    );
}

#[test]
fn a_library_function_sees_only_the_library_globals_above_it() {
    // The importer's own `late`, declared above the call, must not fill in.
    let library = "//@version=6\nlibrary(\"Late\")\nexport get() => late\nfloat late = 2.0\n";
    let main = source("import me/Late/1 as lib\nfloat late = 5.0\nplot(lib.get())");
    let compiled = compile_with_libraries(
        &main,
        &[("me/Late/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert!(!compiled.success, "{:?}", compiled.diagnostics);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0201") && d.message.contains("`late`")),
        "{:?}",
        compiled.diagnostics
    );
    // Declared above the definition, it is the library's global.
    let library = "//@version=6\nlibrary(\"Late\")\nfloat late = 2.0\nexport get() => late\n";
    let run = compile_and_run_with_libraries(
        &main,
        &[("me/Late/1".into(), library.into())],
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_constant_plots("library global above", &run, &[2.0]);
}

#[test]
fn calls_to_user_functions_do_not_depend_on_order() {
    // PineWASM collects every function definition before compiling, so a call
    // may precede its definition, from the top level or from another body.
    run_main(
        "top-level call above the definition",
        "plot(f())\nf() => 2.0",
        &[2.0],
    );
    run_main(
        "body calls a function defined below",
        "g() => f() + 1.0\nf() => 2.0\nplot(g())",
        &[3.0],
    );
    run_main(
        "method defined below the call",
        "type Holder\n    float w\nh = Holder.new(2.0)\nplot(h.get())\nmethod get(Holder self) => self.w",
        &[2.0],
    );
}

/// The notes on the first `code` diagnostic `text` compiles to.
fn notes_of(text: &str, code: &str) -> Vec<String> {
    let compiled = compile(text, CompileOptions::default());
    compiled
        .diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some(code))
        .unwrap_or_else(|| panic!("expected {code}: {:?}", compiled.diagnostics))
        .notes
        .clone()
}

#[test]
fn the_declared_below_note_names_only_a_hidden_global() {
    let below = |notes: &[String]| notes.iter().any(|n| n.contains("declared below"));
    assert!(
        !below(&notes_of(&source("f() => zz\nplot(f())"), "E0201")),
        "a name declared nowhere is simply unknown"
    );
    // A field read off a global declared below names the global, not field
    // syntax.
    let notes = notes_of(
        &source("type T\n    float w\nf() => y.w\ny = T.new(1.0)\nplot(f())"),
        "E0201",
    );
    assert!(below(&notes), "{notes:?}");
    assert!(
        !notes.iter().any(|n| n.contains("field access")),
        "{notes:?}"
    );
}

#[test]
fn every_declaration_form_below_the_definition_is_hidden() {
    assert_error_at(
        "var",
        &source("f() => v\nvar float v = 1.0\nplot(f())"),
        "E0201",
        "=> v",
        3,
        1,
    );
    assert_error_at(
        "tuple destructuring",
        &source("t() => [1.0, 2.0]\nf() => b\n[a, b] = t()\nplot(f() + a)"),
        "E0201",
        "=> b",
        3,
        1,
    );
}

#[test]
fn a_default_follows_its_own_definition_inside_another_body() {
    // `g` may read `y`, but `f`'s default is `f`'s, defined above `y`.
    assert_error_at(
        "default under another boundary",
        &source("f(a = y) => a\ny = 3.0\ng() => f() + y\nplot(g())"),
        "E0201",
        "= y)",
        2,
        1,
    );
}

fn compile_library(case: &str, library: &str, expected: Result<&[f64], &str>) {
    let main = source("import me/Order/1 as lib\nplot(lib.get())");
    let libraries = [("me/Order/1".to_string(), library.to_string())];
    match expected {
        Ok(values) => {
            let run = compile_and_run_with_libraries(
                &main,
                &libraries,
                &market(),
                CompileOptions::default(),
                &RunOptions::default(),
            );
            assert_constant_plots(case, &run, values);
        }
        Err(code) => {
            let compiled = compile_with_libraries(&main, &libraries, CompileOptions::default());
            assert!(!compiled.success, "{case}");
            assert!(
                compiled
                    .diagnostics
                    .iter()
                    .any(|d| d.code.as_deref() == Some(code)),
                "{case}: expected {code}: {:?}",
                compiled.diagnostics
            );
        }
    }
}

#[test]
fn library_helpers_and_initializers_follow_their_own_definitions() {
    let header = "//@version=6\nlibrary(\"Order\")\n";
    compile_library(
        "helper above the library global",
        &format!("{header}peek() => late\nexport get() => peek()\nfloat late = 2.0\n"),
        Err("E0201"),
    );
    compile_library(
        "helper below the library global, exported caller above",
        &format!("{header}export get() => peek()\nfloat late = 2.0\npeek() => late\n"),
        Ok(&[2.0]),
    );
    compile_library(
        "global initializer calls a helper defined above the global it reads",
        &format!("{header}helper() => late\nfloat early = helper()\nfloat late = 2.0\nexport get() => early\n"),
        Err("E0201"),
    );
    compile_library(
        "global initializer calls a helper defined below the global it reads",
        &format!("{header}float late = 2.0\nhelper() => late\nfloat early = helper()\nexport get() => early\n"),
        Ok(&[2.0]),
    );
}

#[test]
fn a_library_global_below_with_a_built_in_name_is_rejected_not_replaced() {
    // Inside a library the script-wide shadow-and-use check does not run, so
    // the hidden global must not silently become the built-in `close`.
    let header = "//@version=6\nlibrary(\"Order\")\n";
    compile_library(
        "global below",
        &format!("{header}export get() => close\nfloat close = 3.0\n"),
        Err("E0211"),
    );
    compile_library(
        "global above",
        &format!("{header}float close = 3.0\nexport get() => close\n"),
        Ok(&[3.0]),
    );
    // A later scalar named like a built-in namespace shadows only that bare
    // name, not the namespace's members.
    compile_library(
        "namespace member, scalar named like the namespace below",
        &format!(
            "{header}export get() => barstate.isfirst or bar_index >= 0 ? 1.0 : 0.0\nfloat barstate = 3.0\n"
        ),
        Ok(&[1.0]),
    );
}
