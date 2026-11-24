use std::{env, fmt::Write, fs, path::PathBuf};
use wasm_encoder::{reencode::Reencode, CodeSection, Module, RawSection};
use wasmparser::{Parser, Payload};

// One definition of the hardened host feature set, shared with the audit.
include!("src/hardened.rs");

/// Names the chart runtime (`runtime/src/chart.rs`) this build links, built
/// from the same source revision by `cargo xtask aot-runtime-chart`. That
/// runtime holds the VM, so it changes with almost every compiler
/// change and is not checked in: a build without this variable has no chart
/// runtime, and `compile_chart_wasm_program` says so.
const CHART_RUNTIME_ENV: &str = "PINEWASM_AOT_CHART_RUNTIME";

fn main() {
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    for suffix in ["", "_ta", "_standalone_wasm", "_broker"] {
        let name = format!("pinewasm_aot_runtime{suffix}.wasm");
        let input = format!("runtime/{name}");
        println!("cargo:rerun-if-changed={input}");
        let bytes = fs::read(input).expect("checked-in runtime");
        canonicalize(&output, &name, &bytes, true);
    }
    println!("cargo:rustc-check-cfg=cfg(pinewasm_chart_runtime)");
    println!("cargo:rerun-if-env-changed={CHART_RUNTIME_ENV}");
    if let Some(path) = env::var_os(CHART_RUNTIME_ENV).filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!("{CHART_RUNTIME_ENV} names {}: {error}", path.display())
        });
        // The splice keeps every body of this runtime (it calls through a
        // table, which the direct-call graph does not describe), so no call
        // graph is written for it.
        canonicalize(&output, "pinewasm_aot_runtime_chart.wasm", &bytes, false);
        println!("cargo:rustc-cfg=pinewasm_chart_runtime");
    }
}

/// Write `name`'s canonical module to `output`, validated under the hardened
/// host feature set, and with `call_graph` its direct-call graph beside it.
fn canonicalize(output: &std::path::Path, name: &str, bytes: &[u8], call_graph: bool) {
    {
        let mut module = Module::new();
        let mut code = CodeSection::new();
        let mut remaining = 0;
        let mut calls = String::from("&[");
        for payload in Parser::new(0).parse_all(bytes) {
            match payload.expect("valid frozen runtime") {
                Payload::CodeSectionStart { count, .. } => {
                    remaining = count;
                    if count == 0 {
                        module.section(&code);
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    let mut callees = Vec::new();
                    for operator in body.get_operators_reader().expect("runtime operators") {
                        match operator.expect("valid runtime operator") {
                            wasmparser::Operator::Call { function_index } => {
                                callees.push(function_index);
                            }
                            // The producer audit treats a byte-identical copy
                            // of a canonical body as already validated.
                            // `ref.func` is the one operator whose validity
                            // depends on the export and element declarations
                            // an artifact does not carry over, so a body
                            // using it cannot be copied on that basis.
                            wasmparser::Operator::RefFunc { .. } => panic!(
                                "{name} function {} uses ref.func, which the verified-copy audit cannot carry into an artifact",
                                code.len()
                            ),
                            _ => {}
                        }
                    }
                    callees.sort_unstable();
                    callees.dedup();
                    write!(calls, "&{callees:?},").expect("write call graph");
                    wasm_encoder::reencode::RoundtripReencoder
                        .parse_function_body(&mut code, body)
                        .expect("runtime body reencoding");
                    remaining -= 1;
                    if remaining == 0 {
                        module.section(&code);
                    }
                }
                payload => {
                    if let Some((id, range)) = payload.as_section() {
                        // Preserve every non-code section verbatim, including
                        // data addresses and metadata. No index changes.
                        module.section(&RawSection {
                            id,
                            data: &bytes[usize::try_from(range.start).expect("section start")
                                ..usize::try_from(range.end).expect("section end")],
                        });
                    }
                }
            }
        }
        calls.push(']');
        if call_graph {
            fs::write(output.join(format!("{name}.calls.rs")), calls)
                .expect("write runtime call graph");
        }
        let canonical = module.finish();
        // The verified-copy audit rests on this: the canonical bodies the
        // splice copies must validate under exactly the feature set the audit
        // applies to the finished artifact. Checking here means a regenerated
        // blob that reached for SIMD or threads fails the compiler build,
        // rather than the first emission.
        if let Err(error) = wasmparser::Validator::new_with_features(hardened_host_features())
            .validate_all(&canonical)
        {
            panic!("{name} does not validate under the hardened host feature set: {error}");
        }
        fs::write(output.join(name), canonical).expect("write canonical runtime");
    }
}
