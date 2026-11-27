# Releasing PineWASM

This guide is for maintainers preparing source and JavaScript/WebAssembly
releases. Follow it from a clean, trusted checkout of the intended release
commit. It covers validation and artifact inspection; publication is a
separate, authorized action.

## Verify the release policy

Run `cargo xtask policy-check` to verify the licensing terms and generated
artifact notices. `release-check` runs the same check before its other gates.

## 1. Freeze the release identity

Fetch first and verify the branch, upstream, and clean state:

~~~sh
git fetch origin --prune
git status --short --branch
git rev-parse --abbrev-ref HEAD
git rev-parse HEAD
git rev-parse '@{upstream}'
~~~

Do not release from a detached, dirty, unpushed, or ambiguous revision. Record
the exact source commit and release ref in the release record.

Release only from this repository. It is the only place that assigns
compiler versions, so the version in `crates/pinewasm_compiler/VERSION` at the
release commit names exactly one compiler ([Compiler versions](versioning.md)).

## 2. Run the complete release gate

Before bootstrapping, require trusted Cargo/rustup executables, toolchains,
environment, and Cargo configuration, including ancestors of the checkout.
An in-process gate cannot validate a process that was never launched. The
artifact preflights below are not a sandbox against arbitrary Cargo runners,
compiler wrappers, or injected compiler configuration during bootstrap.

Use Cargo's built-in `run` command for release evidence. The shorter
`cargo xtask` developer convenience is a configuration-defined alias and can
be replaced before the release tool starts.

~~~sh
cargo run --locked -p pinewasm_xtask -- release-check
~~~

`release-check` enforces a clean tree before and after local generation, but it
does not fetch remotes or prove branch/upstream identity. Complete the explicit
identity checks in §1 immediately before and after this local gate.

The gate checks the workspace, dependencies, notices, runtime reproducibility,
and the generated Wasm package. See [CI](ci.md) for the validation matrix.

### Trusted build environment

Artifact builds require Unix directory-handle support (the Linux and
macOS platforms). Each Cargo invocation uses a fresh home, and build tools
start from the retained workspace handle so replacing its original pathname
before startup cannot redirect them into an external cache. This isolates
stale local caches; it is not a sandbox for untrusted toolchains, dependencies,
or another process with write access to the build files.
Build on a trusted filesystem that remains stable from allocation through
completion: no other process may modify the source, tooling, caches, or ancestor
Cargo configuration during that interval. Configuration checks are preflight
validation; they do not lock or snapshot those directories.
The builder rejects any `.cargo` entry under `target/`, `target/xtask/`, or its
artifact namespace because Cargo would otherwise discover that ignored
ancestor configuration. Remove or relocate such configuration before building;
the builder leaves it untouched. Binaryen is invoked directly and its directory
is not added to the compiler's executable search path.
At the repository root, the reviewed `.cargo/config.toml` is allowed, but the
legacy `.cargo/config` filename is always rejected, including ignored files,
because Cargo gives it precedence over the reviewed configuration.
Cargo also searches above the checkout. External ancestor `.cargo` directories
may contain installed tools and caches, but must not contain `config` or
`config.toml`; symlinked configuration or Cargo directories are rejected too.
The only exception is an ancestor `config.toml` that is a regular file
byte-identical to the checkout's own `.cargo/config.toml`, and only while that
file contains nothing but an `[alias]` table of string values. This covers a
nested checkout of this repository. Cargo keeps
one value for a duplicated string key but concatenates duplicated arrays, so
any other duplicated configuration is still rejected.
Use a checkout outside a configured ancestor, or temporarily relocate that
configuration, rather than letting local settings alter release bytes.

### Hosted validation

A release also needs a successful full CI run for its exact `main` commit.
Every merge to `main` starts full validation. If that run needs to be repeated,
dispatch `CI` on `main` with `full` enabled. Local success or a successful run on an earlier commit is
insufficient.

CI adds introduced-commit checks, the host and minimum-Rust-version matrices,
bounded Miri and AddressSanitizer checks, and a fresh package build retained
as a workflow artifact. See [CI](ci.md), [platform support](platform-support.md),
and [AOT runtime safety](aot-runtime-safety.md) for their exact scope. The
package is release material, not tracked source; the four runtime modules
under `crates/pinewasm_aot/runtime/` remain tracked and must reproduce exactly.

Coverage checks validate links, declarations, counts, and checksums.
Reviewers must still decide whether each linked test proves its stated claim.

## 3. Review the source and dependency boundaries

The release path allowlist in the `pinewasm_xtask` boundary module lists the
files that may be published. New crates, documents, workflows, and scripts
require an explicit review. Ordinary Rust
files inside an already reviewed crate remain extensible.

Run the matcher self-test independently when changing the boundary:

~~~sh
cargo xtask check-public-boundary --self-test
cargo xtask public-boundary
cargo xtask dependency-policy
cargo xtask generate-licenses --check
~~~

The hosted introduced-history scans check reachable commits for added
credentials and for files outside the release allowlist, including files deleted
in a later commit. Review the exact base and head object IDs recorded by CI when
evaluating that result.

Regenerate the dependency inventory after an accepted dependency change:

~~~sh
cargo xtask generate-licenses
git diff -- NOTICE THIRD_PARTY_LICENSES.txt Cargo.lock \
  crates/pinewasm_aot/runtime/Cargo.lock
~~~

Review the generated diff. A generated inventory does not decide whether a new
license is compatible with the intended distribution.

## 4. Inspect release artifacts

The CI job summary names the package artifact by run, attempt, and source
commit. Download it from that run or use
`gh run download <run-id> --name <artifact-name>` with an account that can read
the repository. It contains `wasm-pkg.tar.gz`, `SHA256SUMS`, and `provenance.txt`.
Verify `SHA256SUMS` with `sha256sum --check SHA256SUMS` (or
`shasum -a 256 -c SHA256SUMS` on macOS) before extraction, and verify the
provenance names the intended `main` commit. A workflow artifact expires;
retain the verified package with the release before that happens.

For a local build, run:

~~~sh
cargo xtask wasm
cargo xtask wasm-test
test -s target/wasm-pkg/pinewasm_bg.wasm
test -s target/wasm-pkg/README.md
test -s target/wasm-pkg/LICENSE
test -s target/wasm-pkg/NOTICE
test -s target/wasm-pkg/THIRD_PARTY_LICENSES.txt
cmp LICENSE target/wasm-pkg/LICENSE
shasum -a 256 target/wasm-pkg/pinewasm_bg.wasm
~~~

Write the generated-artifact notice bundle and confirm it matches the
release commit:

~~~sh
cargo xtask artifact-notice --out target/artifact-notice
ls target/artifact-notice
cmp LICENSE target/artifact-notice/LICENSE
cmp NOTICE target/artifact-notice/NOTICE
grep '"commit"' target/artifact-notice/artifact-manifest.json
~~~

The bundle is the material a distributor conveys next to emitted artifacts
([artifact-distribution.md](artifact-distribution.md#3-the-notice-bundle)).
Its manifest must name the release commit, and the command refuses to run
when a tracked runtime blob disagrees with the checksum table in `aot.md`.

The hosted workflow waits for every preceding gate, repeats this from a clean
checkout on the packaging runner, and retains a run/attempt/commit-named
workflow artifact. It never restores the final
package from a cross-run cache. Before distribution, inspect the package
manifest or run `npm pack --dry-run --json ./target/wasm-pkg` and confirm it is named
`pinewasm`, `publishConfig.access` is `public`, and the README and all
three legal files are included.

## Verify a published release

Compare the release ref with the commit in the package provenance and
artifact notice bundle. Verify the published checksums and confirm that
recipients can access the corresponding source and notices.
