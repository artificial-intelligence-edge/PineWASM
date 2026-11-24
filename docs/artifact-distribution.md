# Generated-artifact distribution

This document describes the repository's packaging mechanics and the
generated-artifact licensing model that AI Edge LLC applies. It does not grant a
runtime exception, and it does not grant contributor relicensing rights.

## 1. What an emitted artifact contains

PineWASM emits instructions, tables and data segments representing caller-supplied
Pine source. An artifact can also contain function bodies and read-only data
copied from the checked-in runtime modules:

- `pinewasm_aot_runtime.wasm`
- `pinewasm_aot_runtime_standalone_wasm.wasm`
- `pinewasm_aot_runtime_ta.wasm`
- `pinewasm_aot_runtime_broker.wasm`

The selected runtime can include `pinewasm_aot_runtime`, `pinewasm_num`,
`pinewasm_ta` and `pinewasm_broker`. Those implementation components retain the
repository's AGPL-3.0-only license. The external runtime dependency closure is
derived from the standalone runtime Cargo manifest and lockfile, excluding
development edges. The current closure includes `libm`, `ryu` and `itoa`.
Their license texts remain part of the distribution; alternative first-party
terms do not relicense third-party code.

The [AOT documentation](aot.md) records the four runtime digests and the artifact
interfaces. The notice command reads interface versions from the checked-in
source rather than maintaining a second version constant.

## 2. Caller input and copied implementation

Compiling caller-supplied Pine source with PineWASM does not give AI Edge LLC any
ownership of that source or of the parts of the output that merely represent it.
This covers the input-representation boundary; it is not a claim that every byte
of an emitted module comes from the caller. Copied runtime bodies and read-only
data remain identifiable implementation material under their existing licenses.
Those bytes stay under AGPL-3.0-only and are conveyed as such: the distribution
model makes their corresponding source available rather than carving an exception out
of the license, so no runtime output exception is granted.

Commercial use is permitted under AGPL-3.0-only, subject to that license's
conditions; alternative commercial terms are optional, never a prerequisite.
Alternative terms can cover only portions for which AI Edge LLC holds the
necessary rights. See [the licensing guide](licensing.md).
The separate Apache-2.0 license for `docs/spec/` permits use of that specification;
it does not change the license of implementation bytes copied into artifacts.

## 3. The notice bundle

From a clean source checkout, run:

```sh
cargo xtask artifact-notice --out target/artifact-notice
```

Choose a fresh output directory. The command refuses to overwrite existing bundle
files, including links to source files; the repository root is not a valid output.

The command writes:

| File | Contents |
| --- | --- |
| `LICENSE` | The repository's AGPL-3.0-only license text. |
| `NOTICE` | Project attribution, runtime boundaries and third-party context. |
| `THIRD_PARTY_LICENSES.txt` | Runtime-closure license texts, generated through the same inventory path as the complete repository inventory. |
| `artifact-manifest.json` | PineWASM version, interface versions, four runtime digests, dependency elections and the corresponding source repository/commit. |

The manifest records `runtime_exception: "none"` and the dependency elections as
recorded, matching the policy below; `artifact-notice` refuses to write a
bundle whose statuses disagree with the implemented model. Generating this
bundle does not by itself discharge a distributor's source-provision
obligations. Review the actual distribution and service architecture with
counsel. A source URL and commit identify the checkout; they do not by
themselves guarantee that every recipient has access to that source.

Runtime bytes must match the reviewed checksum table in `aot.md`. The command
refuses an uncommitted source checkout so its recorded commit identifies the
material used to generate the bundle. Keep the bundle alongside the emitted
artifacts it describes and preserve the applicable third-party notices.

## Distribution policy

<!-- BEGIN GENERATED ARTIFACT POLICY -->
This block is generated from PineWASM's artifact notice model.
Notice revision: artifact-notice-v1
Project license: AGPL-3.0-only
Runtime output exception: none
Dependency-license status: recorded

PineWASM distributes artifacts under AGPL-3.0-only: when an artifact is conveyed, its corresponding source must be made available from the PineWASM repository at the commit recorded in the artifact notice bundle or through another AGPL-compliant method; the repository URL and commit identify that source, and no separate runtime output exception is granted.

Dependency license elections:
- Apache-2.0 OR BSL-1.0 -> Apache-2.0
- MIT OR Apache-2.0 -> MIT
- Unlicense OR MIT -> MIT
- MIT OR Apache-2.0 OR LGPL-2.1-or-later -> Apache-2.0
<!-- END GENERATED ARTIFACT POLICY -->

The generated block is the authoritative policy statement. Prose elsewhere in
this document explains packaging and publication mechanics but is not parsed
as an additional policy grant or election.

Run `cargo xtask policy-check` to verify the generated notices and public policy
wording. See [release validation](releasing.md#verify-the-release-policy) for
the maintainer release checks.
The current contribution restrictions are defined in
[CONTRIBUTING.md](../CONTRIBUTING.md#before-the-first-outside-contribution).

Each artifact's notice must identify the actual source used to build it, and
recipients must be able to obtain that source.
