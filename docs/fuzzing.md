# Adversarial testing

PineWASM's pull-request fuzz smoke is a deterministic property-and-mutation
suite. It is intentionally bounded and runs on the repository's pinned stable
Rust toolchain; it is not a claim of open-ended or coverage-guided fuzzing.

Run the same gate as CI with:

~~~sh
cargo xtask fuzz-smoke
~~~

The command first requires all six exact test identities to remain discoverable
as ignored tests, then runs each identity with an exact libtest filter. A
missing, renamed, or no-longer-ignored test therefore fails the gate instead of
passing as a zero-test invocation. It forces 64 cases per property, fixed ChaCha
seeds, one test thread, no persisted Proptest failures, and the dependency
versions in `Cargo.lock`. It removes every inherited Proptest configuration
variable plus Proptest's internal `_PROPTEST_FORKFILE` replay variable, so a
caller cannot silently change the release gate or its replay source. GitHub
gives the complete smoke job a ten-minute deadline.

The exact 32-byte ChaCha seeds repeat one byte: source compilation uses `0x11`,
CSIL uses `0x22`, the VM uses `0x33`, native binding DTOs use `0x44`, AOT
artifact parsing uses `0x55`, VM/AOT differential execution uses `0x66`, and
v5 → v6 migration uses `0x77`.
Each target runs 64 generated cases, for 448 generated cases per smoke invocation,
in addition to the fixed seed mutations described below. The gate is a bounded
regression net, not an exhaustive result or a coverage claim.

## Covered boundaries and checks

| Target | Production entry points | Success condition and bound |
| --- | --- | --- |
| Source compilation | `pinewasm_compiler::compile` | Arbitrary UTF-8 projections and deterministic mutations of small first-party sources must not panic. `success`, error diagnostics, and the optional validated program must agree. Generated inputs are at most 4 KiB. |
| CSIL wire format | `CsilProgram::from_json`, `validate`, and `to_json` | Malformed JSON may be refused. Every decoded program must be safe to validate; every valid program must serialize, deserialize exactly, and validate again. Generated inputs are at most 4 KiB. |
| VM | `pinewasm_compiler::run_program` | Valid structured programs and any compilable source mutations execute with at most 16 bars, 20,000 instructions per bar, 100,000 instructions per run, a 512-cell stack, 4 MiB of VM-owned memory, 64 KiB of runtime string allocation, and 4,096 collection elements. Limit exhaustion is a structured result, not a hang. |
| AOT artifact and metadata parsing | `audit_standalone` and `audit_standalone_wasm` | Arbitrary bytes, mutations of compiler-produced artifacts, and mutations inside a real `module_metadata` section must return a verdict without panicking. Attacker-controlled modules are parsed and validated, never instantiated. Generated raw inputs are at most 4 KiB; the larger valid seeds are built locally from the small first-party source in the test. |
| Binding DTO deserialization | The `Deserialize` implementations used by `serde_wasm_bindgen` for compile options, run options, market data, cached CSIL, library pairs, and standalone WebAssembly options | Native `serde_json` drives the same derived DTO implementations over arbitrary inputs. This does **not** model JavaScript Proxy traps, reflection, BigInt conversion, or the adapter's bounded preflight snapshots; the Wasm/Node package smoke and binding unit tests cover ordinary cases, and browser-specific adversarial generation remains outside this native test. |
| v5 → v6 migration | `pinewasm_migrate::migrate` (run by the language server on every open `//@version=5` document) | Arbitrary text after a v5 header, and deterministic mutations of first-party v5 sources that reach the hoisting and restructuring passes, must not panic under either profile. An unmigrated script comes back unchanged; a migrated one must parse, must be reproduced exactly by the reported edit, and must report findings inside the source. Generated inputs are at most 4 KiB. |
| VM/AOT differential | `compile`, `pinewasm_vm::run`, `compile_program`, and the existing production-ABI `wasmi` loader in `tests/differential.rs` | A structured generator stays inside the native AOT subset. Successful status, completed bars, bit-exact numeric/`na` plot lanes, all emitted plot/visual/fill color lanes, and the complete alert stream must match. Generated programs intentionally exclude hosted-only logs, drawings, and strategy results, whose richer outputs are covered by ordinary differential regression tests. |

The VM property requires every bounded structured case to complete all input
bars without runtime errors. Malformed mutations may be refused, while a
separate first-party infinite-loop seed must hit the instruction-limit error.

Each generated binding case combines raw bytes with a syntactically valid
JSON value, then substitutes that value into known fields of valid DTO seeds
and nested input, market-price, library-pair, and CSIL-function contexts.
Input overrides use the real external tags of all eight `InputValue` variants,
with both whole-payload and known-field substitutions for structured variants.
Controls require valid tagged inputs to survive inside `RunOptions`, malformed
payload fields to be refused, and decimal-string integer extrema to decode
exactly; an unknown tag is not used as evidence of payload coverage.
The value generator has at most three recursive levels, four entries per
collection, and eight characters per string. The harness enforces an 8 KiB
encoded-value limit and a 16 KiB complete-context limit. Non-ignored controls
prove valid seeds deserialize and malformed controls parse as generic JSON
but are refused by the typed deserializers. The property still runs 64
cases; each case exercises multiple derived DTO inputs, so 448 is the overall
property-case count, not a count of deserialization attempts.

The differential property reuses the same loader and full-output comparison as
the existing AOT integration suite. There is no second test-only VM, emitter,
or decoder whose agreement could hide a production bug. Before execution it
requires native lowering with no VM imports, interpreter fallback,
or residual host sites, so comparing the VM with itself cannot pass
as native parity. Non-ignored controls cover every generated source family and
a real host-residual module whose outputs match but which the native gate rejects.

## Seed policy

All checked-in seeds are short constants beside the harness that consumes
them, written for PineWASM. A new seed must be small enough to review directly
and must follow the contribution rules in `CONTRIBUTING.md`.

Generated cases are deterministic prefixes of the checked-in ChaCha seeds.
Changing a seed, generator, case count, input cap, resource budget, comparison
oracle, or ignored-test routing is a reviewed test-policy change.

## Larger campaigns and ownership

The PR author and reviewers own reproduction, code fixes, and regression
validation for the bounded pull-request smoke gate. A designated maintainer
coordinates security classification and escalation as described below. AI Edge
security maintainers own longer-running campaigns, crash retention, and
campaign triage. No continuous fuzzing service is claimed or configured by this
repository today. A maintainer can exercise a longer deterministic prefix in
a local checkout by running the individual ignored test with, for example,
`PINEWASM_ADVERSARIAL_CASES=10000`; the supported range is 1 through 100,000.
The operator must still impose an outer wall-clock and memory limit suitable
for that machine.

Seeds and generated inputs stay first-party and reviewed. If a future
continuous service is adopted, its tool version, owner, scheduling budget,
seed sources, artifact retention, and access controls must be documented in
the same reviewed change. The bounded PR smoke remains separate and must stay
fast enough to run on every contribution.

## Failure, minimization, and disclosure

Proptest shrinks a failing generated value in memory, but this repository
disables automatic persistence so a CI run cannot create an unreviewed file.
For a deterministic seed mutation, reduce the responsible edit while
preserving the failure. Then apply this triage rule:

1. If the failure may cross a trust boundary, escape a resource limit, expose
   memory, or enable a denial of service, reproduce and minimize it only in a
   local checkout. Do not paste the input, stack trace, or generated artifact
   into a public issue, PR, CI artifact, or job summary. Report it through the
   private route in `SECURITY.md` and coordinate disclosure.
2. For an ordinary correctness failure, add a named, non-ignored regression
   test in the owning crate. Keep only the minimized first-party reproducer and
   explain the invariant it pins; do not commit raw external failure output.
3. After a security fix is ready for disclosure, promote a sanitized minimal
   reproducer to the ordinary suite when doing so no longer reveals an
   exploitable condition. Link the advisory or public issue only when its
   disclosure state permits it.

The fixed smoke seed remains useful for reproducibility, but a seed is not a
substitute for a regression test: named tests give future maintainers a stable
description of what broke and why.

### Ownership and failure handling

A designated maintainer coordinates security classification and escalation of
adversarial-smoke failures and assigns a backup when unavailable. The PR author
and reviewers retain responsibility for reproduction, fixes, and regression
validation. Record the security classification, reproduction status, and next
owner. If reproduction is blocked, record the blocker and the next check date.

Treat a new crash as private until triage establishes otherwise. Preserve the
commit, target, deterministic seed and bounded command in a private report using
the [SECURITY.md contact](../SECURITY.md#reporting-a-vulnerability). Do not paste
crash inputs or exploit details into public issues, pull requests or job logs;
coordinate disclosure through that private route after the fix is ready.

After a fix, re-run the regression and the bounded smoke gate. Keep the
original seed and command private when they are needed to explain the failure.

This assigns responsibility for the existing deterministic smoke gate. It does not
claim a continuous coverage-guided fuzzing service, campaign coverage, or a
response guarantee for an unconfigured service.
