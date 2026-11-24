# PineWASM documentation

PineWASM is an independent compiler and runtime for Pine Script® v6 source.
Pine Script® is a trademark of TradingView, Inc. PineWASM is not affiliated with
or endorsed by TradingView, Inc. These guides describe the current source tree.
Compatibility is scoped to tested behavior, and generated WebAssembly artifacts
have distinct execution tiers.

## Start here

| Task | Read |
| --- | --- |
| Build the project and run a script | [Repository quickstart](../README.md#quickstart) and [platform support](platform-support.md) |
| Embed the compiler in Rust | [Rust API](rust-api.md) and the [checked embedding example](../crates/pinewasm_compiler/examples/embedding.rs) |
| Embed the generated JavaScript package | [JavaScript/WebAssembly API](browser.md) and [package installation](../crates/pinewasm_bindings/README.md) |
| Check language compatibility | [Language support](language-support.md) and [feature coverage](pine-v6-coverage.md) |
| Convert Pine v5 source | [Migration guide](migration.md) |
| Configure an editor | [Language server](language-server.md) |
| Compile a program into WebAssembly | [AOT guide](aot.md) and [CSAW module ABI](spec/README.md) |
| Contribute or report a problem | [Contribution guide](../CONTRIBUTING.md), [fixture policy](../tests/README.md), and [security reporting](../SECURITY.md) |

## Implementation and host contracts

| Guide | Scope |
| --- | --- |
| [Architecture](architecture.md) | Compiler pipeline, crate responsibilities, and dependency direction |
| [Execution explained](execution-explained.md) | Interpretation, script compilation, and JavaScript-engine hosting |
| [Intermediate representations](ir.md) | AST, HIR, MIR, CSIL bytecode, serialization, and validation |
| [Runtime](runtime.md) | Bar execution, values, history, drawings, strategies, and resource limits |
| [Libraries](libraries.md) | Explicit library maps, imports, exports, and linking |
| [AOT runtime safety](aot-runtime-safety.md) | Unsafe inventory, Miri, AddressSanitizer, and validation limits |

## Evidence and maintenance

| Guide | Scope |
| --- | --- |
| [Feature coverage matrix](pine-v6-coverage.md) | Feature rows with their test references and explicit partial-support notes |
| [Local benchmarks](benchmarks.md) | First-party workloads, measured boundaries, and limits on performance claims |
| [Provenance](provenance.md) | First-party inputs, contribution origin, and the external sources the repository depends on |
| [Compiler source ownership](../CONTRIBUTING.md#compiler-source-of-truth) | Contribution rules for compiler changes |
| [Continuous integration](ci.md) | Affected validation, successful baselines, and merge gates |
| [Fuzzing](fuzzing.md) | Bounded adversarial campaigns and crash triage |
| [Artifact distribution](artifact-distribution.md) | Runtime-code licensing and generated notice bundles |
| [Compiler versions](versioning.md) | What a compiler version names and how an embedding host stays current |
| [Releasing](releasing.md) | Build prerequisites and release verification |


[Licensing](licensing.md) covers implementation and specification licenses and alternative terms.
[Trademarks](trademarks.md) covers project naming and attribution.
