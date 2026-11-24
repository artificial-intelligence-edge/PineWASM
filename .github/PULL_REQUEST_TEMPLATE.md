<!--
Thanks for contributing! CONTRIBUTING.md has the full workflow; the short
version is below. Keep the PR focused: one behavior change (plus its tests
and docs) per PR.
-->

## What this changes

A summary of the change and why it's needed. Link the issue it addresses,
if there is one.

## Language surfaces touched

If this PR changes Pine Script® v6 semantics (parser, type checker,
lowering, VM behavior, builtin signatures, diagnostics), say which surfaces
— semantics changes need linked tests in this repository (see CONTRIBUTING.md).
Write "none" for pure docs/tooling changes.

## Checklist

- [ ] **Quality bar.** `cargo xtask check` passes (repository policy and
      workflow-pin checks, rustfmt, all-feature Clippy/rustdoc, default-workspace tests,
      explicit all-feature and minimal checks, broker/TA/LSP tests, and
      coverage metadata).
      Formatting, lint, and documentation checks include
      the excluded AOT runtime; its all-features Wasm Clippy pass also denies
      undocumented unsafe blocks.
      Wasm changes additionally pass `cargo xtask wasm`.
- [ ] **Tests.** Behavior changes come with tests (a `tests/`
      fixture or a unit test in the owning crate), and
      docs/pine-v6-coverage.md is updated when the documented support
      changes. Changes to a `tests/compile-pass` fixture also review the exact native/standalone WebAssembly census
      assertions (`cargo xtask aot-census`) and the public AOT description.
- [ ] **Adversarial boundaries.** Changes to compiler, artifact, runtime, or
      binding trust boundaries update the deterministic targets where needed;
      `cargo xtask fuzz-smoke` passes, and any fuzz failure is reported as
      described in `docs/fuzzing.md`.
- [ ] **DCO.** Every commit carries a `Signed-off-by` line
      (`git commit -s`).
- [ ] **Original writing.** All prose in this PR — API docs, diagnostics
      help, and documentation — is original writing.
- [ ] **Provenance.** No external code is included without stating its
      source and a license that permits inclusion here.
- [ ] **Pine Script® fixtures.** Any added `.pine` file is a minimal,
      first-party test input written for this repository.
