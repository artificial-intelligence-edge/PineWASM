//! A user function's body and defaults resolve free names where the function
//! is defined, not at the call site it is inlined into. Name resolution happens
//! before lowering, so these scripts check that the Native WebAssembly route
//! receives the same lexical bindings as the Bytecode Interpreter. The
//! expected values are written out per case in the test source;
//! `function_lexical_scope.rs` in `pinewasm_compiler` covers the interpreter,
//! libraries and diagnostics.
use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, ExecutionMode, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};

#[allow(dead_code)]
mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

const BARS: usize = 4;

/// `expected[i]` is plot `i`'s value on every bar.
fn assert_standalone(case: &str, body: &str, expected: &[f64]) {
    let source = format!("//@version=6\nindicator(\"Function scope\")\n{body}\n");
    let compiled = compile(&source, CompileOptions::default());
    assert!(compiled.success, "{case}: {:?}", compiled.diagnostics);
    let program = compiled.program.unwrap();
    assert_eq!(
        compile_program(&program).unwrap().layout.execution_mode,
        ExecutionMode::Native,
        "{case}"
    );
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .unwrap_or_else(|refusal| panic!("{case}: {refusal:?}"));
    wasmparser::validate(&wasm).expect("valid emitted WebAssembly");
    let mut host = StandaloneWasmHost::new(&wasm);
    host.evaluate(&standalone_wasm_wire(&synthetic_data(BARS), None))
        .unwrap_or_else(|error| panic!("{case}: {error}"));
    for (plot, value) in expected.iter().enumerate() {
        assert_eq!(
            host.plot_lane(plot),
            vec![*value; BARS],
            "{case}: plot {plot}"
        );
    }
}

#[test]
fn standalone_bodies_and_defaults_resolve_at_the_definition() {
    for (case, body, expected) in [
        (
            "body",
            "x = 1.0\nf() => x\ng(x) => f()\nplot(g(2.0), \"g\")",
            &[1.0][..],
        ),
        (
            "omitted default",
            "x = 1.0\nf(a = x) => a\ng(x) => f()\nplot(g(2.0))",
            &[1.0],
        ),
        (
            "default skipped by a named argument",
            "x = 1.0\nf(a = x, b = 0.0) => a + b\ng(x) => f(b = 0.5)\nplot(g(2.0))",
            &[1.5],
        ),
        (
            "caller block local",
            "x = 1.0\nf() => x\nd(a = x) => a\nfloat body = na\nfloat dflt = na\nif bar_index >= 0\n    x = 5.0\n    body := f()\n    dflt := d()\nplot(body)\nplot(dflt)",
            &[1.0, 1.0],
        ),
        (
            "intermediate helper",
            "x = 1.0\nf() => x\nh() => f()\ng(x) => h()\nplot(g(2.0))",
            &[1.0],
        ),
        (
            "callee below the global, caller above",
            "g() => f()\nx = 1.0\nf() => x\nplot(g())",
            &[1.0],
        ),
        (
            "global between two definitions",
            "f() => 2.0\nx = 1.0\ng() => x + f()\nplot(g())",
            &[3.0],
        ),
    ] {
        assert_standalone(case, body, expected);
    }
}

#[test]
fn standalone_controls_keep_their_results() {
    for (case, body, expected) in [
        (
            "renamed caller parameter",
            "x = 1.0\nf() => x\ng(y) => f()\nplot(g(2.0))",
            &[1.0][..],
        ),
        (
            "renamed caller parameter with a default",
            "x = 1.0\nf(a = x) => a\ng(y) => f()\nplot(g(2.0))",
            &[1.0],
        ),
        (
            "explicit argument from the caller",
            "x = 1.0\nf(a = x) => a\ng(x) => f(x)\nplot(g(2.0))",
            &[2.0],
        ),
        (
            "call to a function defined below",
            "g() => f() + 1.0\nf() => 2.0\nplot(g())",
            &[3.0],
        ),
    ] {
        assert_standalone(case, body, expected);
    }
}
