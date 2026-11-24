# Platform and feature support

This page states the validation lanes configured in this repository's GitHub
Actions workflows. It is a test boundary,
not a promise that unlisted environments cannot work. Releases may
tighten this policy when a platform cannot preserve deterministic compiler or
runtime behavior.

Every lane uses an exact reviewed GitHub-hosted runner image.
The workflow routes and their complete static allowlist are checked together;
a repository variable or input cannot redirect a lane. Every push to `main`,
including a merged pull request, starts full validation. Manual dispatch on
`main` is also available; there is no schedule or pull-request trigger. A copy or fork does not run these
canonical-repository lanes. Local checks use the same source
and toolchains. The configured images are ubuntu-24.04, ubuntu-24.04-arm, and macos-15.

## Host operating systems

| Host | Reviewed route | Architecture | Selected CI verification | Support level |
|---|---|---|---|---|
| Linux | `ubuntu-24.04` | `x86_64` | Full Rust quality gate, default/all-feature workspace tests, minimal-feature checks and broker/TA/LSP/numeric tests, native all-target and production `wasm32-unknown-unknown` binding-graph lint, all-feature rustdoc, dependency and publication policy, AOT reproduction, and policy checks | Tested native and quality-gate host |
| Linux | `ubuntu-24.04-arm` | `aarch64` | Fresh optimized Wasm package build, package smoke test under Node.js 24 LTS, generated-package library-boundary tests, strict TypeScript consumer check, and workflow-artifact retention | Release packaging host |
| macOS | `macos-15` | `aarch64` | Default-feature Rust workspace tests | Tested native host |
| Windows | No configured lane | `x86_64` | A requested lane fails explicitly as unavailable | Not currently tested |

The active Linux and macOS lanes use the fixed hosted images. The Linux ARM64
packaging runner builds and tests the production package; Linux x86_64 runs
the native quality and feature matrix. Package validation on Linux ARM64 does
not claim a full native ARM64 workspace-test lane. macOS is a native compiler,
virtual-machine, and language-server test host.

Local planning can select affected validation from a successful integrated
baseline. Automatic main runs exercise the complete applicable matrix; manual
runs do so when `full` is selected. This table describes the available lanes. See
[CI selection and merge gates](ci.md).

These lanes do not promise every distribution, operating-system version,
architecture, C library, or third-party Rust toolchain.

Windows is currently untested in CI. Setting `ENABLE_WINDOWS_CI` to `true`
fails validation planning with an explicit unavailable-lane result; it
does not select an unconfigured fallback or count an omitted lane as passed. A Windows
route needs a reviewed runner configuration and actual acceptance evidence
before it becomes a claimed tested host. The policy tests reject generic
self-hosted routes, unapproved images, and variable/input runner choice.

The checksum-pinned Binaryen installer used by `cargo xtask setup`, Wasm
packaging, and `cargo xtask release-check` currently supports Linux and macOS,
not native Windows. Windows contributors can run the workspace suite locally
with `cargo test --workspace --locked`; use a Linux or macOS host—or rely on
CI—for the complete repository gate.

Artifact builds must also satisfy the stable-filesystem prerequisites in the
[release guide](releasing.md#2-run-the-complete-release-gate). When a personal
Cargo configuration is above the normal checkout, use a checkout whose entire
ancestor chain is writable only by the builder or an administrator and has no
configured `.cargo` ancestor. A fresh child of macOS's per-user `$TMPDIR` is a
typical option; on Linux, use a builder-owned root provisioned outside the
configured home-directory ancestry. `/tmp` and `/private/tmp` are world-writable
and are suitable only on an isolated single-user host, because another local
user could otherwise add ancestor Cargo configuration after preflight. The
builder performs that preflight check, but it does not lock the ancestors; the
release guide's stable-filesystem and concurrent-writer requirements apply for
the complete build.

## Rust toolchains and Cargo features

The root manifest declares Rust 1.86 as the minimum supported Rust version.
CI checks the committed dependency graph at that exact floor in these
configurations:

- the default workspace feature set;
- all workspace features;
- the library-only, no-default-feature surfaces of `pinewasm_broker`,
  `pinewasm_ta`, and `pinewasm_lsp`; and
- the base, `ta`, `standalone_wasm`, `broker`, and all-feature configurations of the
  excluded AOT runtime for `wasm32-unknown-unknown`.

The pinned development toolchain in `rust-toolchain.toml` supplies formatting,
Clippy, documentation, and ordinary test results. On the Linux x86_64 quality-gate host,
Clippy covers every Cargo target in the native workspace graph plus the
production `pinewasm_bindings` library and its transitive compiler/runtime graph
for `wasm32-unknown-unknown`. Host-only test dependencies and the LSP server
transport make a whole-workspace, all-target Wasm invocation invalid. The
excluded AOT runtime has its own all-feature Wasm lint. The minimum-version job
is a build check, not a second lint or test result.

The isolated minimal checks matter because Cargo unifies features within a
dependency graph. A workspace-wide `--no-default-features` invocation alone
could still enable a feature through another workspace member and would not
prove that each intended minimal library compiles by itself.

Today the unified workspace graph already enables every optional feature, so
the explicit all-feature workspace test repeats the default suite. We retain
both commands deliberately: if a future optional feature creates a distinct
graph, its tests remain covered without relying on this premise. CI also checks
the minimal library graph of every feature-bearing workspace package and runs
the broker and transport-free LSP unit suites plus the TA and numeric packages'
contract suites in those graphs. The explicit all-feature Clippy, rustdoc, and
minimum-version checks provide additional forward guards.

## WebAssembly target

PineWASM generates and packages `wasm32-unknown-unknown` modules. CI builds a
fresh optimized npm tarball, installs that tarball into a disposable Node.js 24
LTS consumer, initializes the packaged Wasm module, and executes a small
compiled program. The Node major is pinned so the package gate does not drift
with the runner image; it identifies the tested packaging host, not a minimum
runtime promise for browser consumers. The checked-in AOT runtime variants are
rebuilt for the same target and compared byte for byte.

No browser-version matrix, WebAssembly System Interface target, shared-memory
configuration, mobile application environment, or embedded JavaScript engine
is currently a release gate. Consumers should qualify those hosts against
their own deployment requirements. A browser or platform becomes a claimed
tested host only after a repeatable CI lane is added here.

## Reporting a platform problem

Include the operating system and architecture, `rustc -Vv`, the enabled Cargo
features, the target triple, and the smallest reproducer. Security-sensitive
reports belong in the private channel described by [`SECURITY.md`](../SECURITY.md).
