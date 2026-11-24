//! End-to-end library/import/export tests.
//!
//! A library source `export`s a function; a main script imports it and calls it
//! through the alias; the plotted value is asserted concretely, proving the
//! exported function is inlined at the call site exactly like a local function.

use pinewasm_compiler::{
    compile, compile_and_run_with_libraries, compile_with_libraries, CompileOptions, MarketData,
    RunOptions, Value,
};

/// The library source imported by the tests below. It exports `dbl` (doubles
/// its argument) and keeps `half` private (non-exported).
const MATH_LIB: &str = r#"//@version=6
library("MathLib")

export dbl(float x) =>
    x * 2

half(x) =>
    x / 2
"#;

fn libs() -> Vec<(String, String)> {
    vec![("me/MathLib/1".to_string(), MATH_LIB.to_string())]
}

/// Four flat bars so a constant plotted value is unambiguous.
fn flat_data() -> MarketData {
    MarketData {
        open: vec![1.0, 1.0, 1.0, 1.0],
        high: vec![1.0, 1.0, 1.0, 1.0],
        low: vec![1.0, 1.0, 1.0, 1.0],
        close: vec![1.0, 1.0, 1.0, 1.0],
        volume: vec![1.0, 1.0, 1.0, 1.0],
        ..MarketData::default()
    }
}

#[test]
fn exported_function_is_inlined_and_plots_42() {
    let main = r#"//@version=6
indicator("Importer")
import me/MathLib/1 as math
plot(math.dbl(21))
"#;
    let run = compile_and_run_with_libraries(
        main,
        &libs(),
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "compile failed: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    // 21 * 2 = 42 on every bar.
    for (bar, v) in run.plots[0].values.iter().enumerate() {
        assert_eq!(*v, Value::Float(42.0), "bar {bar} plotted wrong value");
    }
}

#[test]
fn default_alias_is_the_library_name() {
    // Omitting `as alias` defaults the alias to the library name.
    let main = r#"//@version=6
indicator("Importer")
import me/MathLib/1
plot(MathLib.dbl(5))
"#;
    let run = compile_and_run_with_libraries(
        main,
        &libs(),
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert_eq!(run.plots[0].values[0], Value::Float(10.0));
}

#[test]
fn imported_function_uses_a_series_argument() {
    // The exported function inlines with a *series* argument (close), so it must
    // recompute per bar — proving the inline is per-call-site, not a constant.
    let main = r#"//@version=6
indicator("Importer")
import me/MathLib/1 as math
plot(math.dbl(close))
"#;
    let data = MarketData {
        open: vec![1.0, 2.0, 3.0, 4.0],
        high: vec![1.0, 2.0, 3.0, 4.0],
        low: vec![1.0, 2.0, 3.0, 4.0],
        close: vec![1.0, 2.0, 3.0, 4.0],
        volume: vec![1.0, 1.0, 1.0, 1.0],
        ..MarketData::default()
    };
    let run = compile_and_run_with_libraries(
        main,
        &libs(),
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    let vals = &run.plots[0].values;
    assert_eq!(vals[0], Value::Float(2.0));
    assert_eq!(vals[1], Value::Float(4.0));
    assert_eq!(vals[2], Value::Float(6.0));
    assert_eq!(vals[3], Value::Float(8.0));
}

#[test]
fn unresolved_import_is_e0243() {
    let main = r#"//@version=6
indicator("Importer")
import you/Missing/1 as m
plot(m.dbl(1))
"#;
    // The library is not in the provided map (empty), so the import is
    // unresolved.
    let compiled = compile_with_libraries(main, &[], CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")),
        "expected E0243, got {:?}",
        compiled.diagnostics
    );
}

#[test]
fn export_in_an_indicator_is_e0243() {
    let src = r#"//@version=6
indicator("NotALibrary")
export dbl(float x) =>
    x * 2
plot(close)
"#;
    let compiled = compile(src, CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")),
        "expected E0243, got {:?}",
        compiled.diagnostics
    );
}

#[test]
fn calling_a_private_function_via_alias_is_e0243() {
    let main = r#"//@version=6
indicator("Importer")
import me/MathLib/1 as math
plot(math.half(10))
"#;
    let compiled = compile_with_libraries(main, &libs(), CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")),
        "expected E0243 (not exported), got {:?}",
        compiled.diagnostics
    );
}

#[test]
fn omitted_required_argument_of_an_exported_function_is_e0224() {
    // `b` has no default but follows `a`, which has one: `lib.g(5.0)` is
    // within the argument count yet leaves `b` without a value. This is a
    // diagnostic, not an inliner panic.
    let lib = r#"//@version=6
library("DefLib")

export g(float a = 1.0, float b) =>
    a + b
"#;
    let libs = vec![("me/DefLib/1".to_string(), lib.to_string())];
    let main = r#"//@version=6
indicator("Importer")
import me/DefLib/1 as lib
plot(lib.g(5.0))
"#;
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);
    let errors: Vec<_> = compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", compiled.diagnostics);
    assert_eq!(errors[0].code.as_deref(), Some("E0224"));
    assert_eq!(errors[0].message, "`g` is missing required argument `b`");
    // Supplying `b` compiles.
    let main_ok = main.replace("lib.g(5.0)", "lib.g(5.0, 2.0)");
    let compiled = compile_with_libraries(&main_ok, &libs, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
}

#[test]
fn a_library_script_compiles_standalone() {
    // The library itself is a valid standalone script (no importer needed).
    let compiled = compile(MATH_LIB, CompileOptions::all());
    assert!(
        compiled.success,
        "library failed to compile standalone: {:?}",
        compiled.diagnostics
    );
}

// ===== Independent-compilation semantics (library linking): a library's exported
// function resolves its free names (sibling calls, private helpers, top-level
// globals, its own imports) against the LIBRARY namespace, so an importer can
// never hijack a library-internal name, and a library-internal name never leaks
// into the importer. =====

/// Compile+run `main` with `libs`, asserting success, and return the first
/// plot's first-bar value.
fn first_plot(main: &str, libs: &[(String, String)]) -> Value {
    let run = compile_and_run_with_libraries(
        main,
        libs,
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "compile failed: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    run.plots[0].values[0].clone()
}

/// A library whose exported `foo` calls a PRIVATE helper `bar`.
const PRIVATE_SIBLING_LIB: &str = r#"//@version=6
library("SibLib")
bar(x) =>
    x + 100
export foo(float x) =>
    bar(x)
"#;

#[test]
fn exported_calls_private_helper_uses_library_helper() {
    // The library helper adds 100; the importer defines a same-named `bar` that
    // multiplies by 1000. The exported function must call the LIBRARY's `bar`.
    let main = r#"//@version=6
indicator("Importer")
import me/SibLib/1 as sib
bar(x) =>
    x * 1000
plot(sib.foo(5))
"#;
    let libs = vec![("me/SibLib/1".to_string(), PRIVATE_SIBLING_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(105.0));
}

#[test]
fn importer_same_named_function_still_works_for_the_importer() {
    // The importer's own `bar` is unaffected by the library's `bar`.
    let main = r#"//@version=6
indicator("Importer")
import me/SibLib/1 as sib
bar(x) =>
    x * 1000
plot(bar(5))
"#;
    let libs = vec![("me/SibLib/1".to_string(), PRIVATE_SIBLING_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(5000.0));
}

#[test]
fn exported_calls_private_helper_without_importer_defining_it() {
    // With no same-named importer function, the private helper still resolves
    // (it must NOT surface as E0202 "unknown function" in the importer).
    let main = r#"//@version=6
indicator("Importer")
import me/SibLib/1 as sib
plot(sib.foo(5))
"#;
    let libs = vec![("me/SibLib/1".to_string(), PRIVATE_SIBLING_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(105.0));
}

/// A library whose exported `outer` calls an exported sibling `inner`.
const EXPORTED_SIBLING_LIB: &str = r#"//@version=6
library("PairLib")
export inner(float x) =>
    x + 7
export outer(float x) =>
    inner(x) * 2
"#;

#[test]
fn exported_calls_exported_sibling() {
    let main = r#"//@version=6
indicator("Importer")
import me/PairLib/1 as p
plot(p.outer(3))
"#;
    let libs = vec![("me/PairLib/1".to_string(), EXPORTED_SIBLING_LIB.to_string())];
    // (3 + 7) * 2 = 20.
    assert_eq!(first_plot(main, &libs), Value::Float(20.0));
}

/// A library whose exported `foo` reads a top-level global `MULT`.
const GLOBAL_LIB: &str = r#"//@version=6
library("GlobLib")
MULT = 3
export foo(float x) =>
    x * MULT
"#;

#[test]
fn exported_uses_library_global_not_importer_global() {
    // The importer defines its own `MULT = 1000`; the exported function must use
    // the LIBRARY's `MULT = 3`.
    let main = r#"//@version=6
indicator("Importer")
import me/GlobLib/1 as g
MULT = 1000
plot(g.foo(5))
"#;
    let libs = vec![("me/GlobLib/1".to_string(), GLOBAL_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(15.0));
}

#[test]
fn importer_same_named_global_still_works_for_the_importer() {
    let main = r#"//@version=6
indicator("Importer")
import me/GlobLib/1 as g
MULT = 1000
plot(MULT)
"#;
    let libs = vec![("me/GlobLib/1".to_string(), GLOBAL_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(1000.0));
}

#[test]
fn exported_uses_library_global_when_importer_lacks_it() {
    // No importer `MULT` at all: the library global must still resolve (not
    // E0201 "unknown variable").
    let main = r#"//@version=6
indicator("Importer")
import me/GlobLib/1 as g
plot(g.foo(5))
"#;
    let libs = vec![("me/GlobLib/1".to_string(), GLOBAL_LIB.to_string())];
    assert_eq!(first_plot(main, &libs), Value::Float(15.0));
}

/// A library that itself imports another library and re-exports through it
/// (transitive linking).
const BASE_LIB: &str = r#"//@version=6
library("BaseLib")
export add10(float x) =>
    x + 10
"#;

const MID_LIB: &str = r#"//@version=6
library("MidLib")
import me/BaseLib/1 as base
export via(float x) =>
    base.add10(x) * 3
"#;

#[test]
fn transitive_library_import_links() {
    let main = r#"//@version=6
indicator("Importer")
import me/MidLib/1 as mid
plot(mid.via(4))
"#;
    let libs = vec![
        ("me/MidLib/1".to_string(), MID_LIB.to_string()),
        ("me/BaseLib/1".to_string(), BASE_LIB.to_string()),
    ];
    // (4 + 10) * 3 = 42.
    assert_eq!(first_plot(main, &libs), Value::Float(42.0));
}

#[test]
fn calling_an_unknown_export_is_e0243_not_e0202() {
    let main = r#"//@version=6
indicator("Importer")
import me/GlobLib/1 as g
plot(g.nope(1))
"#;
    let libs = vec![("me/GlobLib/1".to_string(), GLOBAL_LIB.to_string())];
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")),
        "expected E0243, got {:?}",
        compiled.diagnostics
    );
    assert!(
        !compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0202")),
        "a library-internal name must not surface as E0202: {:?}",
        compiled.diagnostics
    );
}

#[test]
fn import_cycle_terminates_and_links() {
    // A imports B, B imports A. Each exports a function the other does not call,
    // so the graph is acyclic at the call level even though the import graph is
    // a cycle: resolution must terminate (no stack overflow) and link.
    let a = r#"//@version=6
library("ALib")
import me/BLib/1 as b
export fa(float x) =>
    x + 1
"#;
    let b = r#"//@version=6
library("BLib")
import me/ALib/1 as a
export fb(float x) =>
    x + 2
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/ALib/1 as a
plot(a.fa(10))
"#;
    let libs = vec![
        ("me/ALib/1".to_string(), a.to_string()),
        ("me/BLib/1".to_string(), b.to_string()),
    ];
    assert_eq!(first_plot(main, &libs), Value::Float(11.0));
}

#[test]
fn duplicate_import_alias_is_e0243() {
    let main = r#"//@version=6
indicator("Importer")
import me/GlobLib/1 as g
import me/SibLib/1 as g
plot(close)
"#;
    let libs = vec![
        ("me/GlobLib/1".to_string(), GLOBAL_LIB.to_string()),
        ("me/SibLib/1".to_string(), PRIVATE_SIBLING_LIB.to_string()),
    ];
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")),
        "expected E0243 for a duplicate alias, got {:?}",
        compiled.diagnostics
    );
}

// ===== Diagnostic attribution (bug 3): a library's own parse/analysis
// diagnostics carry the LIBRARY's source id and byte offsets, so an editor
// marker lands on the library buffer — never clamped onto the main source.
// The compiler assigns `libraries[i]` the source id `i + 1` (main is 0). =====

#[test]
fn library_syntax_error_is_attributed_to_the_library_source() {
    use pinewasm_span::SourceId;

    // The library has a syntax error (a bare `=` with no right-hand side) at a
    // byte offset well past the end of the short main buffer, so a diagnostic
    // clamped onto main would be out of range. The main script is valid.
    let broken_lib = r#"//@version=6
library("BrokenLib")
export foo(float x) =>
    x +
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/BrokenLib/1 as b
plot(b.foo(1))
"#;
    let libs = vec![("me/BrokenLib/1".to_string(), broken_lib.to_string())];
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);

    // `libraries[0]` is the first (only) library, so its source id is 1.
    let lib_id = SourceId(1);
    // At least one diagnostic must carry a span attributed to the library's own
    // source id — proving library diagnostics are NOT clamped onto main.
    let attributed = compiled
        .diagnostics
        .iter()
        .flat_map(|d| d.labels.iter().map(|l| l.span))
        .any(|span| span.source == lib_id);
    assert!(
        attributed,
        "expected a diagnostic attributed to the library source id {lib_id:?}, got {:?}",
        compiled.diagnostics
    );

    // No diagnostic may point at an offset past the end of the main buffer under
    // `SourceId::MAIN` (that is the out-of-range-marker bug this guards against).
    let main_len = main.len() as u32;
    for d in &compiled.diagnostics {
        for label in &d.labels {
            if label.span.source == SourceId::MAIN {
                assert!(
                    label.span.end <= main_len,
                    "a main-buffer marker is out of range ({} > {main_len}): {d:?}",
                    label.span.end
                );
            }
        }
    }
}

#[test]
fn duplicate_import_path_in_the_libs_vec_is_e0243() {
    // The same import path is supplied twice in the `libraries` slice. Rather
    // than silently keeping the last one (order-sensitive), the compiler rejects
    // the duplicate up front.
    let main = r#"//@version=6
indicator("Importer")
import me/MathLib/1 as math
plot(math.dbl(1))
"#;
    let libs = vec![
        ("me/MathLib/1".to_string(), MATH_LIB.to_string()),
        ("me/MathLib/1".to_string(), MATH_LIB.to_string()),
    ];
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);
    assert!(
        compiled
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0243")
                && d.message.contains("provided more than once")),
        "expected E0243 for a duplicate library path, got {:?}",
        compiled.diagnostics
    );
}

#[test]
fn arguments_evaluate_in_the_caller_namespace() {
    // The argument `helper(2)` must resolve `helper` in the IMPORTER, even
    // though the call target `p.outer` is a library function. The library also
    // has an `inner` it calls internally; the importer's `helper` is unrelated.
    let main = r#"//@version=6
indicator("Importer")
import me/PairLib/1 as p
helper(x) =>
    x + 100
plot(p.outer(helper(2)))
"#;
    let libs = vec![("me/PairLib/1".to_string(), EXPORTED_SIBLING_LIB.to_string())];
    // helper(2) = 102; outer(102) = (102 + 7) * 2 = 218.
    assert_eq!(first_plot(main, &libs), Value::Float(218.0));
}

// -- library linking patterns --------------------------------------------------

/// A library exporting a typed-parameter function whose `simple int` length
/// flows into a `ta.*` engine call. It also carries top-level DEMONSTRATION
/// code (a demonstration plot) that an importer must NOT run.
const TA_LIB: &str = r#"//@version=6
library("TaLib", true)

// Normalized RSI in [0, 1].
export n_rsi(series float src, simple int n) =>
    ta.rsi(src, n) / 100.0

// Demonstration plot. An importer
// must drop this (it references no exported surface).
demo = n_rsi(close, 14)
plot(demo, "demo", color=color.red)
"#;

#[test]
fn exported_typed_param_flows_a_literal_length_into_ta() {
    // The importer calls `n_rsi(close, 14)`; the `14` literal propagates through
    // the `simple int n` parameter into `ta.rsi(src, n)`, which requires a
    // compile-time length, so the literal length must reach ta.rsi.
    let main = r#"//@version=6
indicator("Importer")
import me/TaLib/1 as t
plot(t.n_rsi(close, 14))
"#;
    // 20 monotonically-rising bars: RSI(14) is defined (and 100, all gains) by
    // the final bar, so the normalized value is a real 1.0 — proving the ta
    // engine ran with the length propagated through the `simple int` parameter.
    let ramp: Vec<f64> = (1..=20).map(|i| i as f64).collect();
    let data = MarketData {
        open: ramp.clone(),
        high: ramp.clone(),
        low: ramp.clone(),
        close: ramp.clone(),
        volume: vec![1.0; ramp.len()],
        ..MarketData::default()
    };
    let run = compile_and_run_with_libraries(
        main,
        &[("me/TaLib/1".to_string(), TA_LIB.to_string())],
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "typed-param length propagation should compile: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    // All gains → RSI 100 → normalized 1.0 on the final bar.
    assert_eq!(
        *run.plots[0].values.last().unwrap(),
        Value::Float(1.0),
        "normalized rsi of a monotonic ramp should be 1.0"
    );
}

#[test]
fn importer_only_surfaces_its_own_plots_not_library_demo_code() {
    // TA_LIB's top-level `plot(demo, ...)` is demonstration code. When imported,
    // only the importer's own `plot` is surfaced — the library's demo plot is
    // pruned, so it does not run when imported.
    let main = r#"//@version=6
indicator("Importer")
import me/TaLib/1 as t
plot(t.n_rsi(close, 14))
"#;
    let run = compile_and_run_with_libraries(
        main,
        &[("me/TaLib/1".to_string(), TA_LIB.to_string())],
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert_eq!(
        run.plots.len(),
        1,
        "only the importer's plot should surface, not the library demo plot"
    );
}

#[test]
fn library_demo_globals_do_not_leak_state_into_the_importer() {
    // A library global used only by demo code is dropped, so it neither runs
    // nor collides with an importer global of the same name.
    let lib = r#"//@version=6
library("GLib")
export inc(float x) =>
    x + 1
demo = 999
plot(demo)
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/GLib/1 as g
demo = 5
plot(g.inc(demo))
"#;
    // The importer's own `demo` (5) is used; inc(5) = 6.
    assert_eq!(
        first_plot(main, &[("me/GLib/1".to_string(), lib.to_string())]),
        Value::Float(6.0)
    );
}

#[test]
fn library_using_single_quotes_and_wrapped_lines_links() {
    // Single-quoted strings and wrapped call arguments must parse so the
    // library links.
    let lib = "//@version=6\nlibrary('QLib')\n\
        export pick(simple int n) =>\n    n > 0 ? 'pos' : 'neg'\n";
    let main = "//@version=6\nindicator(\"Importer\")\nimport me/QLib/1 as q\n\
        s = q.pick(1)\nplot(str.length(s))\n";
    // pick(1) = 'pos', length 3.
    assert_eq!(
        first_plot(main, &[("me/QLib/1".to_string(), lib.to_string())]),
        Value::Float(3.0)
    );
}

#[test]
fn library_with_a_line_wrapped_string_links() {
    // A library may wrap one `str.format` string over several indented lines
    // inside the call's parentheses.
    let lib = r#"//@version=6
library("WrapLib")
export label(int n) =>
    str.format(
         "count: {0}
         \nnext: {1}",
         n, n + 1)
"#;
    let main = "//@version=6\nindicator(\"Importer\")\nimport me/WrapLib/1 as w\n\
        plot(str.length(w.label(1)))\n";
    // "count: 1" + " " (the wrapped line break) + "\nnext: 2" = 8 + 1 + 8.
    assert_eq!(
        first_plot(main, &[("me/WrapLib/1".to_string(), lib.to_string())]),
        Value::Float(17.0)
    );
}

#[test]
fn library_global_used_only_as_a_reassignment_target_survives_pruning() {
    // A library global written by an exported function but never *read* by any
    // function (`latch` only assigns `last := v`) must keep its declaration —
    // demo-code pruning walks reassignment targets, not just reads, so the
    // `:=` still has a declared target. The write is illegal across a function
    // boundary, but must report that policy rather than an undeclared target.
    let lib = r#"//@version=6
library("Latch")
var last = 0.0
export latch(float v) =>
    last := v
    v
// Demonstration plot (pruned in an importer).
plot(last)
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/Latch/1 as lt
plot(lt.latch(5.0))
"#;
    let compiled = compile_with_libraries(
        main,
        &[("me/Latch/1".to_string(), lib.to_string())],
        CompileOptions::default(),
    );
    assert!(!compiled.success);
    assert!(
        compiled.diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("E0217")
                && d.message.contains("cannot reassign global variable `last`")
        }),
        "reassign-only library global must remain declared: {:?}",
        compiled.diagnostics
    );
    assert!(!compiled
        .diagnostics
        .iter()
        .any(|d| { d.code.as_deref() == Some("E0217") && d.message.contains("undeclared") }));
}

// ===== Overloads and methods: a name may carry several
// definitions that differ in parameter count or types, and a library's
// methods link into an importer. =====

/// Compile+run `main` with `libs`, asserting success, and return every plot's
/// first-bar value in declaration order.
fn first_bar_values(main: &str, libs: &[(String, String)]) -> Vec<Value> {
    let run = compile_and_run_with_libraries(
        main,
        libs,
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "compile failed: {:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    run.plots.iter().map(|p| p.values[0].clone()).collect()
}

/// Whether compiling `main` with `libs` fails with a diagnostic whose message
/// contains `needle`.
fn fails_with(main: &str, libs: &[(String, String)], needle: &str) -> bool {
    let compiled = compile_with_libraries(main, libs, CompileOptions::default());
    !compiled.success
        && compiled
            .diagnostics
            .iter()
            .any(|d| d.message.contains(needle))
}

#[test]
fn local_overloads_resolve_by_argument_count_and_type() {
    let main = r#"//@version=6
indicator("Overloads")
f(float x) => x + 1
f(string s) => str.length(s)
f(float x, float y) => x * y
plot(f(1.5))
plot(f("abc"))
plot(f(2.0, 3.0))
plot(f(4))
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![
            Value::Float(2.5),
            Value::Float(3.0),
            Value::Float(6.0),
            Value::Float(5.0)
        ]
    );
}

#[test]
fn an_overload_may_call_another_overload_of_its_name() {
    let main = r#"//@version=6
indicator("Delegating overload")
f(float x, float y) => x + y
f(float x) => f(x, 10.0)
plot(f(2.0))
"#;
    assert_eq!(first_bar_values(main, &[]), vec![Value::Float(12.0)]);
}

#[test]
fn a_same_signature_redefinition_is_still_an_error() {
    let main = r#"//@version=6
indicator("Duplicate")
f(float x) => x
f(float y) => y * 2
plot(f(1.0))
"#;
    assert!(fails_with(main, &[], "is already defined"));
}

#[test]
fn a_method_may_be_called_as_a_function() {
    let main = r#"//@version=6
indicator("Method as function")
method twice(array<float> a, float k = 1.0) =>
    a.avg() * 2 * k
plot(twice(array.from(1.0, 3.0)))
plot(twice(array.from(1.0, 3.0), 2.0))
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![Value::Float(4.0), Value::Float(8.0)]
    );
}

/// A library with an overloaded export, a private method its exported methods
/// call, and exported methods on `array<float>` with defaulted parameters.
const RATIO_LIB: &str = r#"//@version=6
library("RatioLib")
float scale = 10.0
helper(float x) =>
    x * scale
export total(series float a, simple int n) =>
    a * n
export total(series float a, series float b, simple int n) =>
    (a + b) * n
method spread(array<float> values) =>
    values.max() - values.min()
export method ratio(array<float> values, series float k = 1.0) =>
    helper(values.avg()) * k / values.spread()
"#;

fn ratio_libs() -> Vec<(String, String)> {
    vec![("me/RatioLib/1".to_string(), RATIO_LIB.to_string())]
}

#[test]
fn library_overloads_resolve_through_the_alias() {
    let main = r#"//@version=6
indicator("Importer")
import me/RatioLib/1 as r
plot(r.total(2.0, 3))
plot(r.total(2.0, 1.0, 3))
"#;
    assert_eq!(
        first_bar_values(main, &ratio_libs()),
        vec![Value::Float(6.0), Value::Float(9.0)]
    );
}

#[test]
fn exported_library_methods_link_in_both_call_forms() {
    // ratio: helper(avg) * k / spread = (2 * 10) * k / 2.
    let main = r#"//@version=6
indicator("Importer")
import me/RatioLib/1 as r
// The importer's own names must not leak into the library's bodies.
scale = 1.0
helper(float x) => x
a = array.from(1.0, 3.0)
plot(a.ratio())
plot(r.ratio(a, 2.0))
plot(array.from(1.0, 3.0).ratio(3.0))
"#;
    assert_eq!(
        first_bar_values(main, &ratio_libs()),
        vec![Value::Float(10.0), Value::Float(20.0), Value::Float(30.0)]
    );
}

#[test]
fn a_private_library_method_is_not_callable_from_an_importer() {
    let main = r#"//@version=6
indicator("Importer")
import me/RatioLib/1 as r
a = array.from(1.0, 3.0)
plot(r.spread(a))
"#;
    assert!(fails_with(
        main,
        &ratio_libs(),
        "is not exported by library"
    ));
}

#[test]
fn a_same_signature_redefinition_in_a_library_is_an_error() {
    use pinewasm_span::SourceId;

    // As in a main script, and reported at the library's definitions even
    // though the importer never calls `f`.
    let lib = r#"//@version=6
library("DupLib")
export f(float x) => x
export f(float y) => y * 2
export g(float x) => x + 1
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/DupLib/1 as d
plot(d.g(1.0))
"#;
    let libs = vec![("me/DupLib/1".to_string(), lib.to_string())];
    let compiled = compile_with_libraries(main, &libs, CompileOptions::default());
    assert!(!compiled.success);
    let redefinition = compiled
        .diagnostics
        .iter()
        .find(|d| d.message == "function `f` is already defined")
        .unwrap_or_else(|| panic!("no redefinition error: {:?}", compiled.diagnostics));
    assert_eq!(redefinition.code.as_deref(), Some("E0224"));
    assert!(redefinition
        .labels
        .iter()
        .all(|label| label.span.source == SourceId(1)));
}

#[test]
fn a_repeated_library_method_signature_is_an_error() {
    let lib = r#"//@version=6
library("DupMethods")
export method m(array<float> a) => a.sum()
export method m(array<float> b) => b.sum() * 2
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/DupMethods/1 as d
plot(close)
"#;
    let libs = vec![("me/DupMethods/1".to_string(), lib.to_string())];
    assert!(fails_with(
        main,
        &libs,
        "method `m` is already defined for `array<float>`"
    ));
}

#[test]
fn a_library_method_overloaded_by_its_parameters_links_each_overload() {
    // Pine allows a method on one receiver type to be overloaded by its other
    // parameters; each call picks the overload its arguments match.
    let lib = r#"//@version=6
library("OverloadedMethods")
export method m(array<float> a) => a.sum()
export method m(array<float> a, float k) => a.sum() * k
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/OverloadedMethods/1 as o
a = array.from(1.0, 2.0)
plot(a.m())
plot(a.m(10.0))
plot(o.m(a, 2.0))
"#;
    let libs = vec![("me/OverloadedMethods/1".to_string(), lib.to_string())];
    assert_eq!(
        first_bar_values(main, &libs),
        vec![Value::Float(3.0), Value::Float(30.0), Value::Float(6.0)]
    );
}

#[test]
fn methods_overloaded_by_argument_type_dispatch() {
    // Overloads of one method name on user-defined types are chosen by argument
    // type: the `add` overloads by the type of the object added, the `remove`
    // overloads by an int index or a string tag, and the `lookAt` overloads by
    // three floats or one vector.
    let main = r#"//@version=6
indicator("Object-method overload shape")
type Vec3
    float x = 0.0
    float y = 0.0
    float z = 0.0
type Mesh
    float size = 1.0
type Label3D
    string txt = ""
type Scene
    float total = 0.0
type Camera
    float aim = 0.0
method add(Scene self, Mesh m) =>
    self.total += m.size
    self
method add(Scene self, Label3D lbl) =>
    self.total += 100.0
    self
method remove(Scene self, int i) =>
    self.total -= i
    self
method remove(Scene self, string t) =>
    self.total -= 1000.0
    self
method lookAt(Camera self, float x, float y, float z) =>
    self.aim := x + y + z
    self
method lookAt(Camera self, Vec3 tgt) =>
    self.aim := tgt.x * 10.0
    self
s = Scene.new()
m = Mesh.new(2.0)
lbl = Label3D.new("hi")
s.add(m)
s.add(lbl)
s.remove(1)
s.remove("tag")
c = Camera.new()
c.lookAt(1.0, 2.0, 3.0)
a1 = c.aim
v = Vec3.new(4.0, 0.0, 0.0)
c.lookAt(v)
plot(s.total)
plot(a1)
plot(c.aim)
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![
            Value::Float(2.0 + 100.0 - 1.0 - 1000.0),
            Value::Float(6.0),
            Value::Float(40.0),
        ]
    );
}

#[test]
fn an_overload_is_chosen_by_the_type_of_a_call_argument() {
    // `Mesh.new(...)`, a user function's result and a ternary of objects have
    // a type only once evaluated; each is evaluated once and picks `add`.
    let main = r#"//@version=6
indicator("Call-typed overload arguments")
type Mesh
    float size = 1.0
type Label3D
    string txt = ""
type Scene
    float total = 0.0
    int calls = 0
method add(Scene self, Mesh m) =>
    self.total += m.size
    self
method add(Scene self, Label3D lbl) =>
    self.total += 100.0
    self
s = Scene.new()
mk() =>
    s.calls += 1
    Mesh.new(3.0)
s1 = s.add(Mesh.new(2.0))
s2 = s.add(mk())
s3 = s.add(close > 0 ? Label3D.new("a") : Label3D.new("b"))
plot(s.total)
plot(s.calls)
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![Value::Float(105.0), Value::Float(1.0)]
    );
}

#[test]
fn comma_joined_declarations_compile_like_separate_lines() {
    // Several typed and untyped declarations joined on one line with `,`,
    // including a typed declaration after a comma and a unary minus.
    let joined = r#"//@version=6
indicator("Comma-joined declarations")
type Point3
    float x = 0.0
    float y = 0.0
    float z = 0.0
project(Point3 p, float angle, float sx, float sy, float lift) =>
    float c = math.cos(angle), float s = math.sin(angle)
    float px = p.x * sx, py = p.y * sy
    float rx = px * c - py * s, ry = px * s + py * c, rz = p.z - lift * s
    float ox = rx + lift, oy = ry - lift, oz = -rz
    Point3.new(ox, oy, oz)
v = project(Point3.new(1.5, -2.0, 0.5), 0.4, 2.0, 3.0, 0.75)
plot(v.x)
plot(v.y)
plot(v.z)
"#;
    let separate = r#"//@version=6
indicator("Comma-joined declarations")
type Point3
    float x = 0.0
    float y = 0.0
    float z = 0.0
project(Point3 p, float angle, float sx, float sy, float lift) =>
    float c = math.cos(angle)
    float s = math.sin(angle)
    float px = p.x * sx
    py = p.y * sy
    float rx = px * c - py * s
    ry = px * s + py * c
    rz = p.z - lift * s
    float ox = rx + lift
    oy = ry - lift
    oz = -rz
    Point3.new(ox, oy, oz)
v = project(Point3.new(1.5, -2.0, 0.5), 0.4, 2.0, 3.0, 0.75)
plot(v.x)
plot(v.y)
plot(v.z)
"#;
    let joined_values = first_bar_values(joined, &[]);
    assert_eq!(joined_values.len(), 3);
    assert_eq!(joined_values, first_bar_values(separate, &[]));
}

#[test]
fn a_main_script_method_overloaded_by_its_parameters_dispatches() {
    let main = r#"//@version=6
indicator("Main overloads")
method scale(array<float> a) => a.sum()
method scale(array<float> a, float k) => a.sum() * k
a = array.from(1.0, 2.0)
plot(a.scale())
plot(a.scale(2.0))
plot(scale(a, 3.0))
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![Value::Float(3.0), Value::Float(6.0), Value::Float(9.0)]
    );
}

#[test]
fn a_repeated_main_script_method_signature_is_still_an_error() {
    let main = r#"//@version=6
indicator("Main dup")
method m(array<float> a) => a.sum()
method m(array<float> b) => b.sum() * 2
plot(close)
"#;
    assert!(fails_with(
        main,
        &[],
        "method `m` is already defined for `array<float>`"
    ));
}

#[test]
fn a_method_called_as_a_function_dispatches_on_the_receiver_type() {
    // Two receiver types share a method name; the receiver's type picks it,
    // in function form as in method form.
    let main = r#"//@version=6
indicator("Receiver dispatch")
method m(array<float> a) => 1.0
method m(array<int> a) => 2.0
ai = array.from(1, 2)
af = array.from(1.0, 2.0)
plot(m(ai))
plot(m(af))
plot(ai.m())
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![Value::Float(2.0), Value::Float(1.0), Value::Float(2.0)]
    );
}

#[test]
fn a_method_called_as_a_function_rejects_another_receiver_type() {
    let main = r#"//@version=6
indicator("Wrong receiver")
method onlyFloat(array<float> a) => 1.0
plot(onlyFloat(array.from(1, 2)))
"#;
    assert!(fails_with(main, &[], "has no method `onlyFloat`"));
}

#[test]
fn an_alias_method_call_dispatches_on_the_receiver_type() {
    let lib = r#"//@version=6
library("ReceiverLib")
export method m(array<float> a) => a.sum() + 1000
export method m(array<int> a) => a.sum() + 2000
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/ReceiverLib/1 as la
ai = array.from(1, 2)
af = array.from(1.0, 2.0)
plot(la.m(ai))
plot(la.m(af))
"#;
    assert_eq!(
        first_bar_values(main, &[("me/ReceiverLib/1".to_string(), lib.to_string())]),
        vec![Value::Float(2003.0), Value::Float(1003.0)]
    );
}

#[test]
fn type_only_overloads_follow_built_in_series_and_arithmetic_types() {
    // The float overload is declared first; an int argument still picks the
    // int overload, whatever form the argument takes.
    let main = r#"//@version=6
indicator("Type overloads")
f(float x) => x + 0.5
f(int x) => x * 10
plot(f(bar_index))
plot(f(bar_index + 1))
plot(f(close))
plot(f(close + 1))
plot(f(int(close)))
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![
            Value::Float(0.0),
            Value::Float(10.0),
            Value::Float(1.5),
            Value::Float(2.5),
            Value::Float(10.0)
        ]
    );
}

#[test]
fn type_only_library_overloads_follow_the_argument_type() {
    let lib = r#"//@version=6
library("TypeLib")
export h(series float x) => x + 0.5
export h(series int x) => x * 10
"#;
    let main = r#"//@version=6
indicator("Importer")
import me/TypeLib/1 as lc
plot(lc.h(bar_index + 2))
plot(lc.h(close))
"#;
    assert_eq!(
        first_bar_values(main, &[("me/TypeLib/1".to_string(), lib.to_string())]),
        vec![Value::Float(20.0), Value::Float(1.5)]
    );
}

#[test]
fn an_overload_choice_that_would_be_a_guess_is_an_error() {
    // `ta.sma`'s type is not known before analysis, and the candidates
    // disagree on it.
    let unknown = r#"//@version=6
indicator("Unknown argument type")
f(float x) => x + 0.5
f(int x) => x * 10
plot(f(ta.sma(close, 2)))
"#;
    assert!(fails_with(
        unknown,
        &[],
        "cannot tell which overload of `f`"
    ));
    // Two overloads take the same call equally well.
    let tie = r#"//@version=6
indicator("Tie")
g(float x, float y = 1.0) => x + y
g(float x) => x
plot(g(2.0))
"#;
    assert!(fails_with(tie, &[], "cannot tell which overload of `g`"));
}

#[test]
fn a_tuple_returning_method_called_as_a_function_destructures() {
    let main = r#"//@version=6
indicator("Tuple method")
method bounds(array<float> a) => [a.min(), a.max()]
[lo, hi] = bounds(array.from(1.0, 3.0))
plot(lo)
plot(hi)
"#;
    assert_eq!(
        first_bar_values(main, &[]),
        vec![Value::Float(1.0), Value::Float(3.0)]
    );
}

#[test]
fn imported_object_types_keep_identity_and_exported_methods() {
    let library = r#"//@version=6
library("Canvas")
export type Band
    int value = 21
export type Canvas
    array<Band> bands
export make() =>
    Canvas.new(array.from(Band.new()))
export method drawingLimit(Canvas self, int count) =>
    self.bands.get(0).value + count
"#;
    let main = r#"//@version=6
indicator("Imported objects")
import acme/Canvas/1 as pc
array<pc.Canvas> canvases = array.new<pc.Canvas>()
pc.Canvas cv = pc.make()
canvases.push(cv)
plot(canvases.get(0).drawingLimit(21))
"#;
    let run = compile_and_run_with_libraries(
        main,
        &[("acme/Canvas/1".into(), library.into())],
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.plots[0].values, vec![Value::Float(42.0); 4]);
}

#[test]
fn imported_private_object_types_are_not_exposed() {
    let library = "//@version=6\nlibrary(\"L\")\ntype Hidden\n    int x\nexport value() => 1\n";
    let main = "//@version=6\nindicator(\"T\")\nimport acme/L/1 as lib\nx = lib.Hidden.new()\nplot(lib.value())\n";
    let result = compile_with_libraries(
        main,
        &[("acme/L/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert!(!result.success);
}

#[test]
fn imported_types_and_enums_are_isolated_across_libraries_and_aliases() {
    let library = r#"//@version=6
library("L")
export enum Mode
    first
    second
export type Point
    Mode mode = Mode.second
    int value = 21
export make() => Point.new()
export method read(Point self, Point other) => self.value + other.value
export method read(Point self, int other) => self.value + other
"#;
    let main = r#"//@version=6
indicator("Namespaces")
import acme/L/1 as a
import acme/L/1 as same
import other/L/1 as b
type Point
    string value
same.Point x = a.make()
b.Point y = b.Point.new(b.Mode.first, 7)
plot(x.read(a.Point.new()) + y.read(1))
"#;
    let libraries = vec![
        ("acme/L/1".into(), library.into()),
        ("other/L/1".into(), library.into()),
    ];
    let run = compile_and_run_with_libraries(
        main,
        &libraries,
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.plots[0].values, vec![Value::Float(50.0); 4]);
    let cross = main.replace("x.read(a.Point.new())", "x.read(y)");
    assert!(!compile_with_libraries(&cross, &libraries, CompileOptions::default()).success);
}

#[test]
fn integer_array_aggregates_remain_integer_through_field_receivers() {
    let main = r#"//@version=6
indicator("Integer arrays")
type Holder
    array<int> values
h = Holder.new(array.from(2, 4, 6))
int smallest = h.values.min()
int largest = h.values.max()
int total = h.values.sum()
plot(smallest + largest + total)
"#;
    let run = compile_and_run_with_libraries(
        main,
        &[],
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.plots[0].values, vec![Value::Float(20.0); 4]);
}

// Library-global initialization is hoisted before caller statements. These
// regressions exercise the existing local retention/cache policy; the export
// returns a literal or reads only Const globals. An unused private reader can
// retain demo state conservatively.
const LIB_CONTEXT_REQUEST: &str = r#"request.security(syminfo.tickerid, "1W", close)"#;

const LIB_CONTEXT_RETAINED: &str = r#"//@version=6
library("ContextInit")
retainedRequest = request.security(syminfo.tickerid, "1W", close)
unusedPrivateReader() => retainedRequest
export get() => 1.0
"#;

const LIB_CONTEXT_PRUNED: &str = r#"//@version=6
library("ContextInit")
retainedRequest = request.security(syminfo.tickerid, "1W", close)
export get() => 1.0
"#;

const LIB_CONTEXT_CONST: &str = r#"//@version=6
library("ContextInit")
const float seed = 1.0
export get() => seed
"#;

fn library_global_context_main(body: &str) -> String {
    format!("//@version=6\nstrategy(\"Hoisted library context\", dynamic_requests=false)\nimport audit/ContextInit/1 as lib\nenabled = input.bool(true)\n{body}\nplot(close)\n")
}

#[test]
fn hoisted_library_initializer_keeps_the_frozen_first_use_and_pruning_controls() {
    use sha2::{Digest, Sha256};
    for (main, library, main_hash, library_hash, rejected) in [
        (
            r#"//@version=6
strategy("Library initializer context", dynamic_requests=false)
import audit/ContextInit/1 as lib
enabled = input.bool(true)
float observedValue = 0.0
if enabled
    observedValue := lib.get()
plot(observedValue)
"#,
            LIB_CONTEXT_RETAINED,
            "20930d5aa388a04cca7a3ef8e9741ca854794a530529b3d84a63427fa0172c9c",
            "389234894595e80e6daba5527c0fef2031b215ab4f246dec5cbb3f38709c4c4c",
            false,
        ),
        (
            r#"//@version=6
strategy("Library initializer context", dynamic_requests=false)
import audit/ContextInit/1 as lib
enabled = input.bool(true)
primedValue = lib.get()
float observedValue = 0.0
if enabled
    observedValue := lib.get()
plot(observedValue)
"#,
            LIB_CONTEXT_RETAINED,
            "28f7cf607e9c71e2cae75f08aba8af35ff2f9dea4d45f5841e0beb57b0eb08e3",
            "389234894595e80e6daba5527c0fef2031b215ab4f246dec5cbb3f38709c4c4c",
            false,
        ),
        (
            r#"//@version=6
strategy("Library initializer context", dynamic_requests=false)
import audit/ContextInit/1 as lib
enabled = input.bool(true)
float observedValue = 0.0
if enabled
    observedValue := lib.get()
plot(observedValue)
"#,
            LIB_CONTEXT_PRUNED,
            "20930d5aa388a04cca7a3ef8e9741ca854794a530529b3d84a63427fa0172c9c",
            "1ed8fb8e20009cfdef3b0740b0b5ffd34ff7aba3710377dc794a64efdfd6a058",
            false,
        ),
        (
            r#"//@version=6
strategy("Library initializer context", dynamic_requests=false)
import audit/ContextInit/1 as lib
enabled = input.bool(true)
float observedValue = 0.0
if enabled
    observedValue := lib.get()
    observedValue := observedValue + request.security(syminfo.tickerid, "1W", close)
plot(observedValue)
"#,
            LIB_CONTEXT_CONST,
            "41602c25f1276c0c265c03185e59cf5e1db51fbd71f08ece7f23901e7712ff9e",
            "31c3730e61b2bac4d3e7a6af2ea5c5f8d4e03fc03965888e7f1cb7e6187c67e6",
            true,
        ),
    ] {
        for (source, expected) in [(main, main_hash), (library, library_hash)] {
            let digest = Sha256::digest(source.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(digest, expected);
        }
        let result = compile_with_libraries(
            main,
            &[("audit/ContextInit/1".into(), library.into())],
            CompileOptions::default(),
        );
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert_eq!(
            result.success, !rejected,
            "{main}\n{:?}",
            result.diagnostics
        );
        assert_eq!(
            errors.len(),
            usize::from(rejected),
            "{:?}",
            result.diagnostics
        );
        if rejected {
            assert_eq!(errors[0].code.as_deref(), Some("E0242"));
            let span = errors[0].primary_span().unwrap();
            assert_eq!(span.source, pinewasm_compiler::SourceId::MAIN);
            assert_eq!((span.start, span.end), (243, 290));
        }
    }
}

#[test]
fn hoisted_library_request_does_not_inherit_caller_scheduling() {
    for body in [
        "if close > open\n    held = lib.get()",
        "held = close > open ? lib.get() : close",
        "held = enabled and lib.get() > 0",
        "for i = 0 to 1\n    held = lib.get()",
        "var held = lib.get()",
        "held = switch\n    enabled => lib.get()\n    => close",
    ] {
        let main = library_global_context_main(body);
        let result = compile_with_libraries(
            &main,
            &[("audit/ContextInit/1".into(), LIB_CONTEXT_RETAINED.into())],
            CompileOptions::default(),
        );
        assert!(result.success, "{main}\n{:?}", result.diagnostics);
    }
}

#[test]
fn library_initializer_restores_caller_requests_in_branches_operands_and_errors() {
    for body in [
        "if close > open\n    primed = lib.get()\n    requested = request.security(syminfo.tickerid, \"1W\", close)",
        "held = close > open ? lib.get() + request.security(syminfo.tickerid, \"1W\", close) : close",
    ] {
        let main = library_global_context_main(body);
        let result = compile_with_libraries(&main, &[("audit/ContextInit/1".into(), LIB_CONTEXT_CONST.into())], CompileOptions::default());
        assert!(!result.success);
        let errors: Vec<_> = result.diagnostics.iter().filter(|diagnostic| diagnostic.is_error()).collect();
        assert_eq!(errors.len(), 1, "{main}\n{:?}", result.diagnostics);
        assert_eq!(errors[0].code.as_deref(), Some("E0242"));
        let span = errors[0].primary_span().unwrap();
        let start = main.find(LIB_CONTEXT_REQUEST).unwrap() as u32;
        assert_eq!(span.source, pinewasm_compiler::SourceId::MAIN);
        assert_eq!((span.start, span.end), (start, start + LIB_CONTEXT_REQUEST.len() as u32));
    }
    let library = r#"//@version=6
library("ContextInit")
retainedRequest = if chart.is_standard
    request.security(syminfo.tickerid, "1W", close)
else
    close
unusedPrivateReader() => retainedRequest
export get() => 1.0
"#;
    let main = library_global_context_main("if enabled\n    primed = lib.get()\n    requested = request.security(syminfo.tickerid, \"1W\", close)");
    let result = compile_with_libraries(
        &main,
        &[("audit/ContextInit/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert!(!result.success);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert_eq!(errors.len(), 2, "{:?}", result.diagnostics);
    for (diagnostic, source_id, source) in [(errors[0], 1, library), (errors[1], 0, main.as_str())]
    {
        assert_eq!(diagnostic.code.as_deref(), Some("E0242"));
        let span = diagnostic.primary_span().unwrap();
        let start = source.find(LIB_CONTEXT_REQUEST).unwrap() as u32;
        assert_eq!(span.source, pinewasm_compiler::SourceId(source_id));
        assert_eq!(
            (span.start, span.end),
            (start, start + LIB_CONTEXT_REQUEST.len() as u32)
        );
    }
}

#[test]
fn hoisted_library_initializers_keep_their_own_request_and_loop_controls() {
    // These are importer dynamic-request controls.
    // Own-if uses opaque Simple control; the operand cases exercise the existing
    // Series-only operand guard.
    for initializer in [
        "if chart.is_standard\n    request.security(syminfo.tickerid, \"1W\", close)\nelse\n    close",
        "(close > open) ? request.security(syminfo.tickerid, \"1W\", close) : close",
        "(close > open) and request.security(syminfo.tickerid, \"1W\", close) > 0",
    ] {
        let library = format!("//@version=6\nlibrary(\"ContextInit\")\nretainedRequest = {initializer}\nunusedPrivateReader() => retainedRequest\nexport get() => 1.0\n");
        let main = library_global_context_main("held = lib.get()");
        let result = compile_with_libraries(&main, &[("audit/ContextInit/1".into(), library.clone())], CompileOptions::default());
        assert!(!result.success, "initializer: {initializer}\nlibrary:\n{library}\nmain:\n{main}\ndiagnostics: {:?}", result.diagnostics);
        let errors: Vec<_> = result.diagnostics.iter().filter(|diagnostic| diagnostic.is_error()).collect();
        assert_eq!(errors.len(), 1, "initializer: {initializer}\nlibrary:\n{library}\ndiagnostics: {:?}", result.diagnostics);
        assert_eq!(errors[0].code.as_deref(), Some("E0242"), "initializer: {initializer}\nlibrary:\n{library}\ndiagnostics: {:?}", result.diagnostics);
        let span = errors[0].primary_span().unwrap();
        let start = library.find(LIB_CONTEXT_REQUEST).unwrap() as u32;
        assert_eq!(span.source, pinewasm_compiler::SourceId(1), "initializer: {initializer}\nlibrary:\n{library}\ndiagnostics: {:?}", result.diagnostics);
        assert_eq!((span.start, span.end), (start, start + LIB_CONTEXT_REQUEST.len() as u32), "initializer: {initializer}\nlibrary:\n{library}\ndiagnostics: {:?}", result.diagnostics);
    }
    let library = r#"//@version=6
library("ContextInit")
initialize() =>
    for i = 0 to 1
        held = ta.sma(close, 2)
    close
retainedRequest = initialize()
unusedPrivateReader() => retainedRequest
export get() => 1.0
"#;
    let main = library_global_context_main("held = lib.get()");
    let result = compile_with_libraries(
        &main,
        &[("audit/ContextInit/1".into(), library.into())],
        CompileOptions::default(),
    );
    assert!(
        !result.success,
        "own library loop initializer\nlibrary:\n{library}\nmain:\n{main}\ndiagnostics: {:?}",
        result.diagnostics
    );
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "own library loop initializer\nlibrary:\n{library}\ndiagnostics: {:?}",
        result.diagnostics
    );
    assert_eq!(errors[0].code.as_deref(), Some("E0222"));
    assert_eq!(
        errors[0].primary_span().unwrap().source,
        pinewasm_compiler::SourceId(1)
    );
}

#[test]
fn library_global_scheduling_does_not_erase_caller_or_own_var_warnings() {
    let library = r#"//@version=6
library("ContextInit")
retainedTa = ta.sma(close, 2)
unusedPrivateReader() => retainedTa
export get() => 1.0
"#;
    for body in [
        "if close > open\n    held = lib.get()",
        "held = close > open ? lib.get() : close",
        "for i = 0 to 1\n    held = lib.get()",
    ] {
        let main = library_global_context_main(body);
        let result = compile_with_libraries(
            &main,
            &[("audit/ContextInit/1".into(), library.into())],
            CompileOptions::default(),
        );
        assert!(result.success, "{main}\n{:?}", result.diagnostics);
        assert!(
            result
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code.as_deref() != Some("W0004")),
            "{:?}",
            result.diagnostics
        );
    }
    for (body, library, warning_source) in [
        (
            "var held = lib.get() + ta.sma(close, 2)",
            library.to_string(),
            0,
        ),
        (
            "held = lib.get()",
            library.replace("retainedTa =", "var retainedTa ="),
            1,
        ),
    ] {
        let main = library_global_context_main(body);
        let result = compile_with_libraries(
            &main,
            &[("audit/ContextInit/1".into(), library)],
            CompileOptions::default(),
        );
        assert!(result.success, "{main}\n{:?}", result.diagnostics);
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0004"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(
            warnings[0].primary_span().unwrap().source,
            pinewasm_compiler::SourceId(warning_source)
        );
    }
    // The real helper body still executes in the restored caller context.
    // Global initialization is separate from the helper's per-call-site TA.
    let actual_body_library = r#"//@version=6
library("ContextInit")
const float seed = 1.0
export get() => ta.sma(close, 2) + seed
"#;
    for body in [
        "if close > open\n    held = lib.get()",
        "held = close > open ? lib.get() : close",
        "var held = lib.get()",
    ] {
        let main = library_global_context_main(body);
        let result = compile_with_libraries(
            &main,
            &[("audit/ContextInit/1".into(), actual_body_library.into())],
            CompileOptions::default(),
        );
        assert!(result.success, "{main}\n{:?}", result.diagnostics);
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code.as_deref() == Some("W0004"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(
            warnings[0].primary_span().unwrap().source,
            pinewasm_compiler::SourceId(1)
        );
    }
}

#[test]
fn library_global_context_keeps_transitive_namespaces_and_shared_init_slots() {
    let main = r#"//@version=6
strategy("Namespace context", dynamic_requests=false)
import audit/Nested/1 as nested
import audit/Base/1 as base
BASE = 99.0
enabled = input.bool(true)
selected = if enabled
    nested.value()
else
    0.0
plot(selected)
plot(nested.value())
plot(base.value())
plot(BASE)
"#;
    let base = r#"//@version=6
library("Base")
BASE = 2.0
export value() => BASE
"#;
    let nested = r#"//@version=6
library("Nested")
import audit/Base/1 as inner
BASE = 40.0
export value() => inner.value() + BASE
"#;
    let run = compile_and_run_with_libraries(
        main,
        &[
            ("audit/Base/1".into(), base.into()),
            ("audit/Nested/1".into(), nested.into()),
        ],
        &flat_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.compile.as_ref().unwrap().success,
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 4);
    assert_eq!(run.plots.len(), 4);
    for (plot, expected) in run.plots.iter().zip([42.0, 42.0, 2.0, 99.0]) {
        assert_eq!(plot.values, vec![Value::Float(expected); 4]);
    }
    let program = run.compile.as_ref().unwrap().program.as_ref().unwrap();
    assert_eq!(
        program
            .series_slots
            .iter()
            .filter(|slot| slot.name == "BASE")
            .count(),
        3,
        "one main and one global per library, shared across repeated/transitive calls"
    );
}

#[test]
fn library_global_tuple_missing_type_is_independent_of_first_use_context() {
    use sha2::{Digest, Sha256};
    // Regression for an invalid na tuple component default.
    let library = r#"//@version=6
library("DeferGlobals")
pair() => [na, 1]
[x, y] = pair()
unusedPrivateReader() => x
export get() => 1
"#;
    let library_hash = Sha256::digest(library.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        library_hash,
        "ec3d6091833e6cf687cf4c9a8e13f8a805426a81142ba29ba2db23ec9cf4f72f"
    );
    for (main, main_hash) in [
        (
            r#"//@version=6
indicator("Defer global tuple context")
import local/DeferGlobals/1 as dep
[first, tupleAuditRight] = if input.bool(true)
    [dep.get(), 7]
else
    [0, 7]
plot(first + tupleAuditRight)
"#,
            "6b0249c0ad4df668b47a1e0e0413c3e908e51efff14bd2da4b22a4c6baf8591a",
        ),
        (
            r#"//@version=6
indicator("Defer global tuple context")
import local/DeferGlobals/1 as dep
warmup = dep.get()
[first, tupleAuditRight] = if input.bool(true)
    [dep.get(), 7]
else
    [0, 7]
plot(first + tupleAuditRight)
"#,
            "744b738d59d68dabc81a6fa4c793a6d3baff7739f05040012c7d372315714572",
        ),
    ] {
        let digest = Sha256::digest(main.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, main_hash);
        let result = compile_with_libraries(
            main,
            &[("local/DeferGlobals/1".into(), library.into())],
            CompileOptions::default(),
        );
        assert!(!result.success, "{main}\n{:?}", result.diagnostics);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert_eq!(errors.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(errors[0].code.as_deref(), Some("E0220"));
        let span = errors[0].primary_span().unwrap();
        assert_eq!(span.source, pinewasm_compiler::SourceId(1));
        assert_eq!((span.start, span.end), (48, 50));

        // An explicit float cast supplies a type; it does not depend on the
        // caller's deferred tuple-component inference permission.
        let typed_library = library.replace("pair() => [na, 1]", "pair() => [float(na), 1]");
        let typed = compile_with_libraries(
            main,
            &[("local/DeferGlobals/1".into(), typed_library)],
            CompileOptions::default(),
        );
        assert!(typed.success, "{main}\n{:?}", typed.diagnostics);
    }
}
