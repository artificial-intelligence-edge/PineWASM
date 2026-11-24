# Contributing to PineWASM

Thanks for your interest in PineWASM — a compiler and VM for
Pine Script® v6 source. We welcome bug reports, including reports of behavior
that a minimal reproducer shows to be incorrect. See the
[current contribution policy](#outside-contributions).

## Compiler source of truth

`artificial-intelligence-edge/PineWASM` is the source of truth for the
compiler, VM, builtin semantics, migration, bindings, and AOT backend. New work
targets this repository's `main` branch.

### Embedding applications

Applications that embed PineWASM consume the compiler; they do not maintain
parallel implementations of its language semantics. Fix compiler behavior here,
validate the affected surfaces, then pin an exact validated PineWASM revision.

### Revision scope

Recorded results and artifacts state the revision and assumptions they cover.
They do not extend to new source, and re-checking a changed input needs its own
scoped validation.


## Getting set up

```sh
cargo xtask setup     # one-time bootstrap: wasm32 target, local tools, first Wasm build
cargo xtask check     # run the compiler/runtime quality bar
```

The setup and release toolchain supports Linux and macOS. GitHub Actions
exercises the complete packaging path on a GitHub-hosted Linux ARM64 runner.
Windows is not a packaging host, and its CI workspace-test job is opt-in and
currently off. Use a Linux or macOS host for `cargo xtask setup`, Wasm
packaging, and the complete release sweep. See
[`docs/platform-support.md`](docs/platform-support.md) for the exact matrix.

Run `cargo xtask help` for the complete command list. The Rust-native
`pinewasm_xtask` binary is the source of truth for reproducible local builds;
`.cargo/config.toml` only supplies the short Cargo alias. Each workflow routes
child Cargo processes through the version in `rust-toolchain.toml`, even if
another system Cargo binary appears earlier on `PATH`.
The Wasm build compiles its pinned `wasm-bindgen-cli` inside a fresh,
handle-anchored workspace for each artifact. Binaryen and Gitleaks are
checksum-pinned under the checkout's ignored `.tools/` directory, so
simultaneous checkouts cannot replace one another's release tools.

Artifact builds reject `config` or `config.toml` in ancestor `.cargo`
directories, including home-level settings. Keep personal configuration intact
and use a trusted checkout outside configured ancestors; the filesystem,
including ancestor configuration, must remain stable throughout the build.
A checkout nested inside another checkout is allowed: an ancestor `config.toml`
passes when it is a regular file with exactly the bytes of the nested checkout's
own alias-only `.cargo/config.toml`.
See [the release build prerequisites](docs/releasing.md#2-run-the-complete-release-gate).

## The quality bar

Use WebAssembly compilation, Compiled WebAssembly, Standalone WebAssembly, and
Bytecode Interpreter consistently in prose and API documentation. See the
[naming convention](docs/browser.md#standalone-webassembly-naming-and-migration).
Use that convention in source and filenames; `cargo xtask terminology-check`
verifies it, and it also runs as part of `cargo xtask check`.

Every change must clear the quality bar before it lands. For Rust changes:

```sh
cargo xtask check     # repository-policy and pinned-workflow checks
                      # rustfmt: workspace + excluded AOT runtime
                      # Clippy: native Cargo targets, production Wasm graph, and AOT runtime
                      # rustdoc: all workspace features + excluded AOT runtime
                      # facade contract census and executable rustdoc examples
                      # isolated minimal checks and coverage metadata
                      # deterministic adversarial smoke
                      # default/all-feature workspace tests, and
                      # isolated broker/TA/LSP/numeric minimal-feature tests
```

The excluded AOT runtime carries its own lint policy because it is a separate
`no_std` Wasm build. Its Clippy pass denies warnings and undocumented unsafe
blocks; each raw linear-memory operation must include a local safety argument.

Use `cargo xtask wasm` when a change touches the JavaScript/Wasm binding or its
transitive backend. The generated package lives under `target/wasm-pkg` and is
not committed.

`cargo xtask ci` runs the complete local backend sweep. Hosted CI runs on
`main` after every push, including a merged pull request, and by manual
dispatch. There is no scheduled run or pull-request trigger. Run the checks
required for your change before opening a pull request.
See [CI](docs/ci.md) for commit-range checks and
[platform support](docs/platform-support.md) for the tested hosts and targets.

Forks do not start this repository's validation checks, and outside
contributions are not accepted yet; see [Outside contributions](#outside-contributions).
The workflows do not depend on repository visibility and cannot request
self-hosted runners.

`cargo xtask docs-check` renders the full workspace, verifies that facade
re-exports retain their defining-crate rustdoc, checks the explicit
member-documentation debt census, and runs the compiler, binding-adapter, and
AOT-artifact examples. Those examples also compile at the declared minimum
Rust version in GitHub Actions.

The supported-facade census is intentionally fail-closed around attributes and
macros whose expansion could change callable API. A new tool attribute such as
`#[rustfmt::skip]`, a new derive/helper attribute, or a macro in an unlisted
position can therefore require an explicit census-policy update even when it is
harmless. Include a compiler-level positive control with any such update; do
not relax the policy by name alone.

## Language-semantics changes get extra verification

Changes to Pine Script® v6 *semantics* (the parser, type checker, lowering, VM
behavior, builtin signatures, or diagnostics) need tests in this repository.
Repository CI verifies eligible changes against the fixtures bundled in this
repository, and each coverage claim must link to a test in this repository.
Please state in the PR description which language surfaces a change touches
so the right verification runs.

Adding, removing, or changing a `tests/compile-pass` fixture can also move the
exact native and standalone WebAssembly assertions. Run `cargo xtask aot-census`, update the
pinned assertions when the change is intentional, and reassess the related
public AOT description.

Changes at an untrusted-input boundary should also update the deterministic
property or mutation target described in [docs/fuzzing.md](docs/fuzzing.md).
Only purpose-written first-party seeds belong in the repository. Potentially
security-sensitive crashes are minimized and handled privately under
[SECURITY.md](SECURITY.md), not pasted into a pull request or public issue.

## Developer Certificate of Origin

All contributions require a [Developer Certificate of
Origin](https://developercertificate.org/) sign-off. Every commit must carry
a `Signed-off-by` line with your name and an email address you control. A
GitHub-provided `noreply` address is acceptable when it unambiguously identifies
the signing account:

```
Signed-off-by: Jane Developer <jane@example.com>
```

`git commit -s` adds the line for you. By signing off you certify that you
wrote the change or otherwise have the right to submit it under the
project's license. Unsigned commits cannot be merged.

Sign-off records provenance; it does not transfer your copyright or grant a
separate proprietary-relicensing right. Unless you and AI Edge LLC enter a
separate written agreement, an accepted contribution is available under the
repository's AGPL-3.0-only terms and is outside any commercial license AI
Edge LLC may offer for portions for which AI Edge LLC holds the necessary
rights. Maintainers must preserve that boundary when assembling a commercially
licensed distribution.

The project's licensing position is stated identically in every policy
document and checked mechanically (`cargo xtask policy-check`):
Commercial use is permitted under AGPL-3.0-only, subject to that license's
conditions; alternative commercial terms are optional, never a prerequisite.
Compiling caller-supplied Pine source with PineWASM does not give AI Edge LLC
any ownership of that source or of the parts of the output that merely
represent it. [docs/artifact-distribution.md](docs/artifact-distribution.md)
explains what an emitted artifact contains and describes the
distribution model.

## Outside contributions

We accept issue reports from everyone. Code changes are currently accepted from
the project maintainer only, and outside pull requests are not accepted yet.

## Originality and provenance

Three rules protect the project's clean provenance:

- **No external code without license and provenance.** Do not paste code
  from other projects, forums, or documentation into a PR unless you state
  where it came from and its license permits inclusion here (and the license
  and attribution travel with it). When in doubt, open an issue first.
- **Product documentation must be original writing.** Builtin, diagnostic and API
  prose is written from PineWASM's own source and tests, in the authors' own words.
- **Pine fixtures are first-party test inputs.** A `.pine` file under
  [`tests/`](tests/README.md) is written for PineWASM and exercises one behavior
  under test. Do not contribute scripts, data, or output that you do not have
  the right to share. If a report includes
  code you do not have the right to share, write a new minimal reproducer
  instead of copying that code.

## Pull requests

- Keep PRs focused: one behavior change (plus its tests and docs) per PR.
- Semantics changes need tests — a fixture in the appropriate
  `tests/` category or a unit
  test in the owning crate — and an update to
  [docs/pine-v6-coverage.md](docs/pine-v6-coverage.md) when coverage moves.
- Match the surrounding code style and comment voice; this codebase invests
  in comments that explain *why*, and reviews hold new code to that.
- Write commit messages in the imperative with a scope prefix, for example
  `fix(vm): clamp history offset at series start`.
- Fill in the pull request template, including the DCO and originality
  checkboxes.

## Reporting incorrect behavior

If PineWASM behaves incorrectly, please report it. Use the *divergence report*
issue template: it asks for a minimal script and the actual PineWASM output.
Please do not include any proprietary source code you lack the rights to share.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Be
kind; disagree about code, not people.
