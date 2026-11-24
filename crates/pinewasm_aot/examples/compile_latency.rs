//! Source-to-standalone WebAssembly and prepared-CSIL emission timings, including the producer audit.
//! Run with `cargo run --release -p pinewasm_aot --example compile_latency`.
//! The first sample is cold; the subsequent samples reuse the runtime templates.
//! This measures compilation only, not WebAssembly instantiation or evaluation.
use pinewasm_aot::{
    compile_standalone_wasm_program, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use std::{hint::black_box, time::Instant};

fn main() {
    let source = "//@version=6\nstrategy(\"Synthetic compile workload\")\nlength = input.int(20, \"Length\", minval=1)\nbasis = ta.sma(close, length)\natr = ta.atr(length)\nif close > basis + atr\n    strategy.entry(\"L\", strategy.long)\nif close < basis\n    strategy.close(\"L\")\nplot(basis)\nplot(atr)\n";
    let options = StandaloneWasmOptions {
        module_type: StandaloneWasmModuleType::Strategy,
        signal_plot: None,
    };
    let started = Instant::now();
    let compiled = compile(source, CompileOptions::all());
    assert!(!compiled
        .diagnostics
        .iter()
        .any(pinewasm_span::Diagnostic::is_error));
    let program = compiled.program.expect("synthetic source compiles");
    let source_ns = started.elapsed().as_nanos();
    let mut emission_ns = Vec::new();
    let mut bytes = 0;
    for _ in 0..101 {
        let started = Instant::now();
        let artifact = compile_standalone_wasm_program(black_box(&program), &options)
            .expect("standalone WebAssembly emission");
        emission_ns.push(started.elapsed().as_nanos());
        bytes = black_box(artifact.len());
    }
    println!(
        "{}",
        serde_json::json!({
            "workload": "synthetic-sma-atr-strategy-v1",
            "source_compile_ns": source_ns,
            "first_emission_ns": emission_ns[0],
            "warm_emission_ns": &emission_ns[1..],
            "wasm_bytes": bytes,
            "scope": "CSIL-to-standalone WebAssembly emission including audit; no execution"
        })
    );
}
