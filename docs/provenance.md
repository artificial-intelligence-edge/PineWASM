# Repository origin and verification

PineWASM compiles and executes the subset of the Pine Script® language listed in its coverage matrix. Pine Script® is a trademark of TradingView, Inc.; PineWASM is not affiliated with or endorsed by TradingView, Inc.
The compiler [test fixtures](../tests/README.md), hand-written [migration-tool test cases](../crates/pinewasm_migrate/tests/scripts/README.md),
and [benchmark workloads](../benchmarks/workloads/) exercise language behavior locally.
The higher-timeframe helper regression uses a [shared library fixture](../crates/pinewasm_lsp/fixtures/language_queries.json)
following Justin Dehorty’s `KernelFunctions` example. The [regression test](../crates/pinewasm_compiler/tests/htf_pure_helper.rs)
credits the author and identifies the original publication as MPL-2.0.

## Verification evidence

The [coverage matrix](pine-v6-coverage.md) links each supported or verified feature
to a test and records assumptions and known limitations. Comparing the bytecode
interpreter and WebAssembly output checks agreement between PineWASM’s own
execution paths. The [AOT guide](aot.md) records checksums for the four runtime
modules built from the adjacent Rust sources.

## Source and artifact history

Artifacts and benchmark measurements record the source
revisions and assumptions they were generated from. See [artifact distribution](artifact-distribution.md)
for the corresponding source and notices supplied with emitted modules.
[Contributing](../CONTRIBUTING.md#originality-and-provenance) covers originality,
attribution, and sign-offs.
