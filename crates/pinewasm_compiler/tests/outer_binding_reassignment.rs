//! A user helper may not rebind a global binding, and a caller's bindings are
//! not visible to it at all (`function_lexical_scope.rs`). Reference contents
//! and local shadows are separate; a helper may not rebind its own parameters
//! either.
//! Three cases pin the global-write and shadow boundary.

use pinewasm_compiler::{
    compile, compile_and_run, compile_and_run_with_libraries, compile_with_libraries,
    CompileOptions, CompileResult, MarketData, RunOptions, RunResult, Value,
};
use sha2::{Digest, Sha256};

// SHA-256 of the source text below: 6d4e2b11abb9c462165b7f8b6dc0494088e36ba02b559c97a84fa33173af3eb4
const REVIEW_WITNESS: &str = r#"//@version=6
indicator("x")
bool use = true
off() =>
    use := false
    0
off()
var k = 0
if not use            // summary folds this to `if false` and skips it
    k := k + 1        // carried recurrence the summary never sees
if timeframe.isdaily  // a visible, stable write makes `k` appear in the summary as Simple
    k := 5
plot(close, offset = k)
"#;

// SHA-256 of the source text below: 37ca52d25dfbb3d67e069d969f00056f218ff23e447757315848a6ceb49cce1e
const MINIMAL_GLOBAL_WRITE: &str = r#"//@version=6
indicator("Helper global write discriminator")
bool use = true
off() =>
    use := false
    0
off()
plot(use ? 1 : 0)
"#;

// SHA-256 of the source text below: 98f24fbedec8c2a1ed62a515b042604a696ff39b309aa2820b3285a9794ab6ab
const LEGAL_SHADOW: &str = r#"//@version=6
indicator("Helper local shadow discriminator")
bool use = true
off() =>
    bool use = true
    use := false
    use ? 1 : 0
plot(off())
plot(use ? 1 : 0)
"#;

fn market() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        timeframe: "60".into(),
        ..MarketData::default()
    }
}

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Function binding boundary\")\n{body}\n")
}

fn capture_errors(result: &CompileResult) -> Vec<&pinewasm_compiler::Diagnostic> {
    result
        .diagnostics
        .iter()
        .filter(|d| {
            d.code.as_deref() == Some("E0217")
                && d.message.contains("cannot reassign global variable")
        })
        .collect()
}

fn assert_rejected_at(result: &CompileResult, text: &str, target: &str) {
    assert!(!result.success, "unexpectedly accepted {text}");
    let start = text.find(target).expect("write target") as u32;
    let name = target.split_whitespace().next().unwrap();
    assert!(
        capture_errors(result).iter().any(|d| {
            d.message.contains(&format!("`{name}`"))
                && d.labels
                    .iter()
                    .any(|l| l.span.start == start && l.span.end == start + name.len() as u32)
        }),
        "expected captured-write diagnostic at {start}: {:?}",
        result.diagnostics
    );
}

fn assert_run(result: &RunResult, expected: &[&[f64]]) {
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 4);
    assert_eq!(result.plots.len(), expected.len());
    for (plot, values) in result.plots.iter().zip(expected) {
        assert_eq!(plot.values.len(), values.len());
        for (value, expected) in plot.values.iter().zip(*values) {
            let actual = match value {
                Value::Float(value) => *value,
                Value::Int(value) => *value as f64,
                value => panic!("unexpected nonnumeric value {value:?}"),
            };
            assert_eq!(actual, *expected);
        }
    }
}

#[test]
fn exact_global_write_sources_are_rejected_at_the_illegal_binding() {
    for (text, expected_hash) in [
        (
            REVIEW_WITNESS,
            "6d4e2b11abb9c462165b7f8b6dc0494088e36ba02b559c97a84fa33173af3eb4",
        ),
        (
            MINIMAL_GLOBAL_WRITE,
            "37ca52d25dfbb3d67e069d969f00056f218ff23e447757315848a6ceb49cce1e",
        ),
    ] {
        let digest = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(digest, expected_hash);
        let compiled = compile(text, CompileOptions::default());
        assert_rejected_at(&compiled, text, "use :=");
        assert!(!compiled
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undeclared variable `use`")));
    }
}

#[test]
fn exact_local_shadow_keeps_the_global_value() {
    let run = compile_and_run(
        LEGAL_SHADOW,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[0.0; 4], &[1.0; 4]]);
}

#[test]
fn plain_reference_rebinding_is_also_an_outer_binding_write() {
    let text = source("var values = array.new_int(1, 0)\nreplace() =>\n    values := array.new_int(1, 1)\n    0\nreplace()\nplot(1)");
    assert_rejected_at(
        &compile(&text, CompileOptions::default()),
        &text,
        "values :=",
    );
}

#[test]
fn nested_helpers_cannot_see_a_caller_local_to_rebind() {
    // `inner` resolves names where it is defined, and `outer`'s local does not
    // exist there: the write names an undeclared variable, not a caller's.
    let text = source("inner() =>\n    local := 9.0\n    0\nouter() =>\n    float local = 2.0\n    inner()\n    local\nplot(outer())");
    let result = compile(&text, CompileOptions::default());
    assert!(!result.success);
    let start = text.find("local :=").expect("write target") as u32;
    assert!(
        result.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0217")
                && d.message.contains("undeclared variable `local`")
                && d.labels
                    .iter()
                    .any(|l| l.span.start == start && l.span.end == start + 5)
        }),
        "{:?}",
        result.diagnostics
    );
    assert!(
        capture_errors(&result).is_empty(),
        "{:?}",
        result.diagnostics
    );
}

#[test]
fn nested_body_blocks_keep_function_local_writes() {
    let text = source("float value = 99.0\nlocal(float n) =>\n    float m = n + 1\n    float value = m\n    if true\n        value := value + 1\n    value\nplot(local(2.0))\nplot(value)");
    let run = compile_and_run(
        &text,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[4.0; 4], &[99.0; 4]]);
}

#[test]
fn function_and_method_reference_content_mutation_remains_legal() {
    let text = source("type State\n    bool enabled\nvar state = State.new(true)\nvar values = array.new_int(1, 0)\nmethod disable(State self) =>\n    self.enabled := false\n    0\nmutate() =>\n    state.enabled := false\n    array.set(values, 0, array.get(values, 0) + 1)\n    0\nstate.disable()\nmutate()\nplot(state.enabled ? 1 : 0)\nplot(array.get(values, 0))");
    let run = compile_and_run(
        &text,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[0.0; 4], &[1.0, 2.0, 3.0, 4.0]]);
}

#[test]
fn methods_obey_the_same_plain_binding_boundary() {
    let text = source("type State\n    bool enabled\nstate = State.new(true)\nint count = 0\nmethod touch(State self) =>\n    count := count + 1\n    0\nstate.touch()\nplot(count)");
    assert_rejected_at(
        &compile(&text, CompileOptions::default()),
        &text,
        "count :=",
    );
}

#[test]
fn readonly_guard_aliases_survive_calls_that_only_mutate_reference_contents() {
    let text = source("var bool guard = true\nvar values = array.new_int(1, 0)\ntouch() =>\n    array.set(values, 0, array.get(values, 0) + 1)\n    0\ntouch()\nvar int k = 0\nif not guard\n    k := k + 1\nk := 2\nplot(close, offset=k)\nplot(guard ? 1 : 0)\nplot(array.get(values, 0))");
    let run = compile_and_run(
        &text,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(
        &run,
        &[&[2.0, 1.0, 2.0, 1.0], &[1.0; 4], &[1.0, 2.0, 3.0, 4.0]],
    );
}

#[test]
fn library_function_scope_base_is_measured_after_the_namespace_swap() {
    let library = "//@version=6\nlibrary(\"Local\")\nexport inc(float x) =>\n    float value = x\n    value := value + 1\n    value\n";
    let main = source("import me/Local/1 as lib\nouter(float x) =>\n    float value = 100.0\n    if true\n        value := lib.inc(x)\n    value\nplot(outer(2.0))");
    let run = compile_and_run_with_libraries(
        &main,
        &[("me/Local/1".into(), library.into())],
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[3.0; 4]]);
}

#[test]
fn library_global_initialization_suspends_the_callers_function_boundary() {
    // Covers library initialization behavior only, not every export or
    // global-read restriction.
    let library = "//@version=6\nlibrary(\"Init\")\nfloat seed = 1.0\nseed := 2.0\nexport inc(float x) =>\n    float local = x\n    local := local + seed\n    local\n";
    let main = source("import me/Init/1 as lib\nouter(float x) =>\n    float value = x\n    value := lib.inc(value)\n    value\nplot(outer(3.0))");
    let run = compile_and_run_with_libraries(
        &main,
        &[("me/Init/1".into(), library.into())],
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[5.0; 4]]);
}

#[test]
fn nested_library_calls_restore_each_owning_scope_boundary() {
    let inner = "//@version=6\nlibrary(\"Inner\")\nexport inc(float x) =>\n    float value = x\n    value := value + 1\n    value\n";
    let outer = "//@version=6\nlibrary(\"Outer\")\nimport me/Inner/1 as inner\nexport through(float x) =>\n    float value = x\n    value := inner.inc(value)\n    value := value + 1\n    value\n";
    let main = source("import me/Outer/1 as lib\nwrap(float x) =>\n    float value = x\n    value := lib.through(value)\n    value := value + 1\n    value\nplot(wrap(1.0))");
    let libraries = [
        ("me/Outer/1".into(), outer.into()),
        ("me/Inner/1".into(), inner.into()),
    ];
    let run = compile_and_run_with_libraries(
        &main,
        &libraries,
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_run(&run, &[&[4.0; 4]]);
}

#[test]
fn a_library_global_write_is_rejected_when_first_called_by_a_helper() {
    let library = "//@version=6\nlibrary(\"Bad\")\nvar last = 0.0\nexport latch(float x) =>\n    last := x\n    x\n";
    let main = source("import me/Bad/1 as lib\nouter() => lib.latch(5.0)\nplot(outer())");
    let result = compile_with_libraries(
        &main,
        &[("me/Bad/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert_rejected_at(&result, library, "last :=");
    assert!(!result
        .diagnostics
        .iter()
        .any(|d| d.message.contains("undeclared variable `last`")));
}

#[test]
fn a_failed_body_does_not_leak_a_function_boundary_to_later_statements() {
    let text = source("bad() => missing\nbad()\nint outside = 1\noutside := 2\ngood() =>\n    int local = 1\n    local := 2\n    local\nplot(good())\nplot(outside)");
    let compiled = compile(&text, CompileOptions::default());
    assert!(!compiled.success);
    assert!(compiled
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0201")));
    assert!(
        capture_errors(&compiled).is_empty(),
        "{:?}",
        compiled.diagnostics
    );
}
