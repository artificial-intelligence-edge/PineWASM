//! Deterministic adversarial checks for the compiler, durable CSIL, and VM.
//!
//! These tests are ignored by a bare `cargo test` and are release-gated by
//! `cargo xtask fuzz-smoke`. Keeping the runner seed and case count explicit
//! makes the pull-request run reproducible; see `docs/fuzzing.md` for the
//! larger fuzzing campaign and regression-promotion policy.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, RunOptions, RuntimeErrorKind,
};
use pinewasm_csil::{validate, CsilProgram};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

const DEFAULT_CASES: u32 = 64;
const MAX_CASES: u32 = 100_000;
const MAX_GENERATED_BYTES: usize = 4_096;
const BUDGET_SOURCE: &str =
    "//@version=6\nindicator(\"Budget seed\")\nx = 0\nwhile true\n    x := x + 1\nplot(x)\n";

const SOURCE_SEEDS: &[&str] = &[
    "//@version=6\nindicator(\"Arithmetic seed\")\nplot((close - open) / 2.0)\n",
    "//@version=6\nindicator(\"State seed\")\nvar count = 0\ncount := count + 1\nplot(count)\n",
    "//@version=6\nindicator(\"TA seed\")\nplot(ta.sma(close, 3))\n",
    BUDGET_SOURCE,
];

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

fn runner(seed_byte: u8) -> TestRunner {
    let config = Config {
        cases: configured_cases(),
        failure_persistence: None,
        max_shrink_iters: 4_096,
        ..Config::default()
    };
    TestRunner::new_with_rng(
        config,
        TestRng::from_seed(RngAlgorithm::ChaCha, &[seed_byte; 32]),
    )
}

fn byte_mutations(seed: &[u8]) -> Vec<Vec<u8>> {
    let mut mutations = vec![Vec::new(), seed.to_vec()];
    for end in [seed.len() / 4, seed.len() / 2, seed.len().saturating_sub(1)] {
        mutations.push(seed[..end].to_vec());
    }
    if !seed.is_empty() {
        for index in [0, seed.len() / 3, seed.len() / 2, seed.len() - 1] {
            let mut flipped = seed.to_vec();
            flipped[index] ^= 0x80;
            mutations.push(flipped);

            let mut deleted = seed.to_vec();
            deleted.remove(index);
            mutations.push(deleted);

            let mut inserted = seed.to_vec();
            inserted.splice(index..index, [0, b'\n', 0xff]);
            mutations.push(inserted);
        }
    }
    mutations.sort();
    mutations.dedup();
    mutations
}

fn exercise_compiler(bytes: &[u8]) {
    let source = String::from_utf8_lossy(bytes);
    let mask = bytes.first().copied().unwrap_or_default();
    let result = compile(
        &source,
        CompileOptions {
            emit_ast: mask & 1 != 0,
            emit_hir: mask & 2 != 0,
            emit_mir: mask & 4 != 0,
            emit_csil: mask & 8 != 0,
            validate_passes: mask & 16 != 0,
            ..CompileOptions::default()
        },
    );
    let mut has_errors = false;
    for diagnostic in &result.diagnostics {
        has_errors |= diagnostic.is_error();
    }
    assert_eq!(
        result.success,
        result.program.is_some() && !has_errors,
        "CompileResult success/program/diagnostic contract drifted"
    );
    if let Some(program) = result.program {
        assert!(
            validate(&program).is_ok(),
            "the compiler returned invalid CSIL"
        );
    }
}

fn exercise_csil_json(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let Ok(program) = CsilProgram::from_json(&text) else {
        return;
    };
    if validate(&program).is_ok() {
        assert_valid_csil_round_trip(&program);
    }
}

fn assert_valid_csil_round_trip(program: &CsilProgram) {
    assert!(validate(program).is_ok(), "CSIL seed must start valid");
    let canonical = program.to_json();
    let round_trip = CsilProgram::from_json(&canonical)
        .expect("serializing then deserializing valid CSIL must succeed");
    assert_eq!(
        &round_trip, program,
        "valid CSIL JSON must round-trip exactly"
    );
    assert!(
        validate(&round_trip).is_ok(),
        "round-tripped valid CSIL must stay valid"
    );
}

fn bounded_options() -> RunOptions {
    RunOptions {
        max_instructions_per_bar: 20_000,
        max_instructions_per_run: 100_000,
        max_string_bytes_per_run: 64 * 1024,
        max_vm_bytes_per_run: 4 * 1024 * 1024,
        max_stack_depth: 512,
        max_bars: Some(16),
        max_collection_elements: 4_096,
        ..RunOptions::default()
    }
}

fn synthetic_data(prices: &[i16]) -> MarketData {
    let mut data = MarketData::default();
    for (index, price) in prices.iter().enumerate() {
        let close = f64::from(*price) / 8.0;
        data.open.push(close - 0.25);
        data.high.push(close + 1.0);
        data.low.push(close - 1.0);
        data.close.push(close);
        data.volume.push(1_000.0 + index as f64);
        data.time.push(1_700_000_000_000 + index as i64 * 60_000);
    }
    data
}

fn structured_source(kind: u8, literal: i16, window: u8) -> String {
    let literal = i32::from(literal);
    let window = u32::from(window.max(1));
    match kind % 6 {
        0 => "indicator(\"Close\")\nplot(close)\n".to_string(),
        1 => format!("indicator(\"Arithmetic\")\nplot(close + {literal}.0)\n"),
        2 => format!("indicator(\"Window\")\nplot(ta.sma(close, {window}))\n"),
        3 => format!(
            "indicator(\"Branch\")\nx = 0.0\nif close > open\n    x := high - low\nelse\n    x := close - {literal}.0\nplot(x)\n"
        ),
        4 => format!(
            "indicator(\"History\")\noffset = math.min(bar_index, {window})\nplot(close[offset])\n"
        ),
        _ => format!(
            "indicator(\"Loop\")\nsum = 0.0\nfor i = 0 to {window}\n    sum := sum + close[i]\nplot(sum + {literal}.0)\n"
        ),
    }
}

#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn arbitrary_and_seed_mutated_source_compilation_is_total() {
    for seed in SOURCE_SEEDS {
        for mutation in byte_mutations(seed.as_bytes()) {
            exercise_compiler(&mutation);
        }
    }

    runner(0x11)
        .run(
            &proptest::collection::vec(any::<u8>(), 0..=MAX_GENERATED_BYTES),
            |bytes| {
                exercise_compiler(&bytes);
                Ok(())
            },
        )
        .expect("deterministic arbitrary-source campaign failed");
}

#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn csil_json_decode_validate_and_valid_round_trip_are_total() {
    for seed in SOURCE_SEEDS.iter().take(3) {
        let result = compile(seed, CompileOptions::default());
        let program = result.program.expect("first-party CSIL seed must compile");
        assert_valid_csil_round_trip(&program);
        let json = program.to_json();
        for mutation in byte_mutations(json.as_bytes()) {
            exercise_csil_json(&mutation);
        }
    }

    let case = (
        proptest::collection::vec(any::<u8>(), 0..=MAX_GENERATED_BYTES),
        0u8..6,
        any::<i16>(),
        1u8..=8,
    );
    runner(0x22)
        .run(&case, |(bytes, kind, literal, window)| {
            exercise_csil_json(&bytes);

            let source = structured_source(kind, literal, window);
            let compiled = compile(&source, CompileOptions::default());
            prop_assert!(compiled.success, "structured source failed: {source}");
            let program = compiled.program.expect("successful compile has a program");
            assert_valid_csil_round_trip(&program);
            Ok(())
        })
        .expect("deterministic CSIL campaign failed");
}

#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn valid_and_mutated_sources_execute_under_explicit_vm_budgets() {
    let fallback_data = synthetic_data(&[800, 804, 799, 812, 808, 820, 816, 824]);

    let budget_program = compile(BUDGET_SOURCE, CompileOptions::default())
        .program
        .expect("the first-party infinite-loop seed must compile");
    let budget_result = run_program(&budget_program, &fallback_data, &bounded_options());
    assert_eq!(
        budget_result.runtime_errors.first().map(|error| error.kind),
        Some(RuntimeErrorKind::InstructionLimitExceeded),
        "the adversarial check must exercise a structured VM budget refusal"
    );

    for seed in SOURCE_SEEDS {
        for mutation in byte_mutations(seed.as_bytes()) {
            let source = String::from_utf8_lossy(&mutation);
            if let Some(program) = compile(&source, CompileOptions::default()).program {
                let result = run_program(&program, &fallback_data, &bounded_options());
                assert!(
                    result.bars_executed <= fallback_data.bars() as u32,
                    "a bounded VM run reported more bars than its feed"
                );
            }
        }
    }

    let case = (
        0u8..6,
        any::<i16>(),
        1u8..=8,
        proptest::collection::vec(any::<i16>(), 1..=16),
    );
    runner(0x33)
        .run(&case, |(kind, literal, window, prices)| {
            let source = structured_source(kind, literal, window);
            let compiled = compile(&source, CompileOptions::default());
            prop_assert!(compiled.success, "structured source failed: {source}");
            let program = compiled.program.expect("successful compile has a program");
            let data = synthetic_data(&prices);
            let result = run_program(&program, &data, &bounded_options());
            prop_assert!(
                result.runtime_errors.is_empty(),
                "bounded structured source failed: {:?}",
                result.runtime_errors
            );
            prop_assert_eq!(result.bars_executed, data.bars() as u32);
            Ok(())
        })
        .expect("deterministic resource-bounded VM campaign failed");
}
