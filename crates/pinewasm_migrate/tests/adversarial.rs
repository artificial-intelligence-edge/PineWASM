//! Deterministic adversarial checks for the v5 → v6 migration.
//!
//! The language server runs the migration on every open `//@version=5`
//! document, so arbitrary text reaches it. These tests are ignored by a bare
//! `cargo test` and are release-gated by `cargo xtask fuzz-smoke`; see
//! `docs/fuzzing.md`.

use pinewasm_migrate::{migrate, MigrationOptions, Profile, Status};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

const DEFAULT_CASES: u32 = 64;
const MAX_CASES: u32 = 100_000;
const MAX_GENERATED_BYTES: usize = 4_096;

/// First-party v5 sources that exercise the hoisting and restructuring
/// paths, so mutations land in the code that splices text.
const SOURCE_SEEDS: &[&str] = &[
    "//@version=5\nstrategy(\"S\")\nc = close > open\nstrategy.entry(\"L\", strategy.long, qty = ta.sma(volume, 3), when = c)\nstrategy.exit(\"X\", \"L\", profit = 10, limit = high)\n",
    "//@version=5\nindicator(\"E\")\nx = close > open ? (volume > 0 and ta.rsi(close, 14) > 50) : false\nif bar_index\n    x := na\nelse if volume > 0 and ta.sma(close, 3) > 0\n    x := true\nplotshape(x)\n",
    "//@version=5\nindicator(\"W\")\ni = 0\nwhile i < 10 and ta.sma(close, 3) > 0\n    i += 1\ny = switch\n    close > open => 1\n    volume > 0 and ta.rsi(close, 14) > 50 => 2\n    => 3\nplot(i + y + 5 / 2, color = color.red, transp = 50)\n",
    "//@version=5\nindicator(\"R\")\nf(x) => x > 0 and ta.change(x) > 0\ns = request.security(syminfo.tickerid, \"D\", close > open and f(close))\nplotshape(s)\nplot(6[1])\nlabel.new(bar_index, high, timeframe.period)\n",
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
        for index in [
            0,
            seed.len() / 3,
            seed.len() / 2,
            (2 * seed.len()) / 3,
            seed.len() - 1,
        ] {
            let mut flipped = seed.to_vec();
            flipped[index] ^= 0x80;
            mutations.push(flipped);

            let mut deleted = seed.to_vec();
            deleted.remove(index);
            mutations.push(deleted);

            let mut inserted = seed.to_vec();
            inserted.splice(index..index, *b"\n (");
            mutations.push(inserted);
        }
    }
    mutations.sort();
    mutations.dedup();
    mutations
}

/// The migration must return for any input, and a migrated output must be
/// source the parser accepts, reproducible from the reported edit, and
/// already v6.
fn exercise(bytes: &[u8]) {
    let source = String::from_utf8_lossy(bytes);
    for profile in [Profile::Exact, Profile::Idiomatic] {
        let options = MigrationOptions {
            profile,
            ..MigrationOptions::default()
        };
        let result = migrate(&source, &options);
        if result.status != Status::Migrated {
            assert_eq!(
                result.output, source,
                "an unmigrated script must be returned unchanged"
            );
            continue;
        }
        let parsed = pinewasm_parse::parse(&result.output);
        assert!(
            !parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == pinewasm_span::Severity::Error),
            "migrated output does not parse:\n{}",
            result.output
        );
        let mut applied = source.to_string();
        for edit in result.edits.iter().rev() {
            applied.replace_range(edit.start as usize..edit.end as usize, &edit.new_text);
        }
        assert_eq!(
            applied, result.output,
            "the reported edit must reproduce the output"
        );
        for finding in &result.findings {
            assert!(
                finding.start <= finding.end && finding.end as usize <= source.len(),
                "finding outside the source: {finding:?}"
            );
        }
    }
}

#[test]
#[ignore = "release-gated by cargo xtask fuzz-smoke"]
fn arbitrary_and_seed_mutated_source_migration_is_total() {
    for seed in SOURCE_SEEDS {
        for mutation in byte_mutations(seed.as_bytes()) {
            exercise(&mutation);
        }
    }
    // Generated text keeps a v5 header so the rewrite passes actually run.
    runner(0x77)
        .run(
            &proptest::collection::vec(any::<u8>(), 0..=MAX_GENERATED_BYTES),
            |bytes| {
                let mut source = b"//@version=5\nindicator(\"G\")\n".to_vec();
                source.extend_from_slice(&bytes);
                exercise(&source);
                Ok(())
            },
        )
        .expect("deterministic arbitrary-source migration campaign failed");
}
