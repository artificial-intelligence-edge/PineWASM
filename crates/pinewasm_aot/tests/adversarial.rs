//! Deterministic malformed-artifact and metadata parser checks.
//!
//! The target calls the producer's public byte-audit APIs, so it exercises the
//! same parser and policy path used for exported artifacts. It never
//! instantiates attacker-controlled WebAssembly.

use pinewasm_aot::{
    audit_standalone, audit_standalone_wasm, compile_program, compile_standalone_wasm_program,
    schema, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use std::ops::Range;

const DEFAULT_CASES: u32 = 64;
const MAX_CASES: u32 = 100_000;

fn configured_cases() -> u32 {
    let Some(raw) = std::env::var_os("PINEWASM_ADVERSARIAL_CASES") else {
        return DEFAULT_CASES;
    };
    let raw = raw
        .to_str()
        .expect("PINEWASM_ADVERSARIAL_CASES must be UTF-8");
    let cases: u32 = raw
        .parse()
        .expect("PINEWASM_ADVERSARIAL_CASES must be an integer");
    assert!(
        (1..=MAX_CASES).contains(&cases),
        "PINEWASM_ADVERSARIAL_CASES must be in 1..={MAX_CASES}"
    );
    cases
}

fn runner() -> TestRunner {
    let config = Config {
        cases: configured_cases(),
        failure_persistence: None,
        max_shrink_iters: 4_096,
        ..Config::default()
    };
    TestRunner::new_with_rng(
        config,
        TestRng::from_seed(RngAlgorithm::ChaCha, &[0x55; 32]),
    )
}

fn exercise_artifact(bytes: &[u8]) {
    let standalone = audit_standalone(bytes);
    let standalone_wasm = audit_standalone_wasm(bytes);
    if standalone_wasm.is_ok() {
        assert!(
            standalone.is_ok(),
            "the stricter standalone WebAssembly audit must imply the standalone byte contract"
        );
    }
}

fn byte_mutations(seed: &[u8]) -> Vec<Vec<u8>> {
    let mut mutations = vec![Vec::new(), seed.to_vec(), b"\0asm".to_vec()];
    for end in [
        1,
        4,
        seed.len() / 4,
        seed.len() / 2,
        seed.len().saturating_sub(1),
    ] {
        mutations.push(seed[..end.min(seed.len())].to_vec());
    }
    if !seed.is_empty() {
        for index in [
            0,
            1.min(seed.len() - 1),
            seed.len() / 3,
            seed.len() / 2,
            seed.len() - 1,
        ] {
            let mut flipped = seed.to_vec();
            flipped[index] ^= 0xff;
            mutations.push(flipped);
        }
    }
    let mut appended = seed.to_vec();
    appended.extend_from_slice(&[0, 0xff, b'{', b'}']);
    mutations.push(appended);
    mutations.sort();
    mutations.dedup();
    mutations
}

fn custom_data_range(bytes: &[u8], name: &str) -> Option<Range<usize>> {
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let Ok(wasmparser::Payload::CustomSection(section)) = payload else {
            continue;
        };
        if section.name() == name {
            let range = section.data_range();
            return Some(range.start as usize..range.end as usize);
        }
    }
    None
}

fn first_party_artifacts() -> (Vec<u8>, Vec<u8>) {
    let source = "//@version=6\nindicator(\"Artifact seed\")\nplot(ta.sma(close, 3))\n";
    let program = compile(source, CompileOptions::default())
        .program
        .expect("first-party artifact seed must compile");
    let native = compile_program(&program)
        .expect("first-party seed must be in the native AOT subset")
        .wasm;
    let standalone_wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("first-party seed must produce a standalone WebAssembly artifact");
    (native, standalone_wasm)
}

#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn arbitrary_aot_artifact_and_metadata_parsing_is_total() {
    let (native, standalone_wasm) = first_party_artifacts();
    assert!(audit_standalone(&native).is_ok());
    assert!(audit_standalone(&standalone_wasm).is_ok());
    assert!(audit_standalone_wasm(&standalone_wasm).is_ok());

    for artifact in [&native, &standalone_wasm] {
        for mutation in byte_mutations(artifact) {
            exercise_artifact(&mutation);
        }
    }

    let metadata = custom_data_range(&standalone_wasm, schema::MODULE_METADATA_SECTION)
        .expect("standalone WebAssembly seed must carry module_metadata");
    let metadata_len = metadata.end - metadata.start;
    assert!(metadata_len > 0, "module_metadata must not be empty");
    for offset in [
        0,
        metadata_len / 4,
        metadata_len / 2,
        metadata_len.saturating_sub(1),
    ] {
        let mut mutation = standalone_wasm.clone();
        mutation[metadata.start + offset] ^= 0x80;
        exercise_artifact(&mutation);
    }

    runner()
        .run(
            &proptest::collection::vec(any::<u8>(), 0..=4_096),
            |bytes| {
                exercise_artifact(&bytes);
                Ok(())
            },
        )
        .expect("deterministic AOT artifact campaign failed");
}
