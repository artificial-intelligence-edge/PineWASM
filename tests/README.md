# PineWASM test fixtures

This is the home for Pine Script® source used by PineWASM's test
suite. The one other location is
[`crates/pinewasm_migrate/tests/scripts/`](../crates/pinewasm_migrate/tests/scripts/README.md),
which holds full-length v5 test scripts written for this project and their
expected v6 migrations from PineWASM's migrator, for the migration crate; the
same rules apply there. These files are compiler inputs, not an end-user
example gallery or a benchmark dataset. The separate
[`benchmarks/`](../docs/benchmarks.md) suite contains first-party performance workloads
and mathematical calculations.

## Layout

| Directory | Meaning |
| --- | --- |
| [`compile-pass/`](compile-pass/) | Small programs PineWASM is expected to accept; the pipeline test also executes them on a local synthetic feed. |
| [`compile-fail/`](compile-fail/) | Intentionally malformed or unsupported programs PineWASM is expected to reject with a structured diagnostic. |
| [`regression/`](regression/) | Larger, deterministic programs that pin PineWASM's numeric and supported-language behavior across indicator, strategy, and library declarations. |

The directory names describe PineWASM's expected result. The supported-language
matrix, with the test evidence for each row, is in
[`docs/pine-v6-coverage.md`](../docs/pine-v6-coverage.md).

## Fixture origin and test boundary

PineWASM contributors wrote the test scripts for this project. The repository
distributes the fixtures under its AGPL-3.0-only license.

Repository tests compile these files locally with PineWASM. The regression feed
is generated in Rust, and the numeric fixture derives its inputs from
`bar_index`; no test in this repository makes a network request. The expected
output table in `crates/pinewasm_compiler/tests/fixture_regressions.rs` records
PineWASM's results on that local synthetic feed. It is a regression lock for
this project.

Maintainers selected some expected values in `tests/fixtures/rendering/` and
`crates/pinewasm_compiler/tests/fixtures/` after comparing PineWASM's results
with Pine Script® v6 behavior. These values serve as local regression checks.
One shared library fixture follows a maintainer's own published
example; [repository origin](../docs/provenance.md) records the attribution.

Contributions must preserve this boundary. New Pine fixtures must be original
to the contributor and covered by the commit's Developer Certificate of Origin
sign-off. Do not add third-party code, market-data files, or text copied from
documentation. Reduce a reported behavior to the smallest original reproducer
instead.

## Running the suites

From the repository root:

```sh
cargo test --locked -p pinewasm_compiler --test pipeline
cargo test --locked -p pinewasm_compiler --test fixture_regressions
cargo xtask check-file tests/regression/numeric-runtime.pine
```

For a compile-and-run smoke check across all larger regression fixtures:

```sh
cargo run --locked -p pinewasm_compiler --bin pinewasm-check -- \
  --run tests/regression/*.pine
```

After an intentional semantic change, inspect the focused behavior tests before
updating an expected output. Never refresh an expectation merely to make the
suite green: a changed bit pattern is a PineWASM behavior change that requires
review.
