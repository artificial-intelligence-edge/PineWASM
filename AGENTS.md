Project policy lives in [CONTRIBUTING.md](CONTRIBUTING.md). This file adds the
rules and commands for contributors.

## Never

- Never loosen the repository-identity or same-repository guards. A pull
  request from a fork does not start the validation checks, and outside pull
  requests are not accepted yet; see
  [Outside contributions](CONTRIBUTING.md#outside-contributions).
- Never add a runner outside the reviewed GitHub-hosted images in
  `.github/ci/test_policy_preflight.py`'s `HOSTED_RUNNERS` table. Self-hosted
  runners and unapproved or configurable images are rejected. A repository
  variable or input must not choose a runner, and no job may depend on
  repository visibility. A route change needs reviewed workflow, allowlist,
  actionlint-label and `docs/platform-support.md` edits together.
- Never add a `Signed-off-by` line on your own initiative. A sign-off is the
  human author's
  [DCO certification](CONTRIBUTING.md#developer-certificate-of-origin). Add
  one only when that person explicitly tells you to, with their exact name and
  email.

## Where changes go

Make compiler, VM, migration, binding and AOT fixes in this repository against
current `main`, with regression tests here.

## CI

Every push to `main`, including a merged pull request, starts full CI on the
reviewed GitHub-hosted runners. There are no scheduled runs; manual dispatch
on `main` remains available. Pull-request events do not start full validation,
so a pull request's commits are first checked after it merges. Each run checks
every commit since the last successful eligible run for sign-offs, secrets and
repository policy, and the compiler version for each first-parent step. A
failing commit stays in later runs' ranges. See [docs/ci.md](docs/ci.md).

## Before you open or update a pull request

Review the change adversarially, then run the local checks for what you
changed:

- every change: with `base=$(git merge-base origin/main HEAD)` and
  `head=$(git rev-parse HEAD)`, run `cargo xtask check-dco "$base" "$head"`,
  `cargo xtask check-compiler-version --git-range "$base" "$head"`,
  `cargo xtask check-secrets --git-range "$base" "$head"` and
  `cargo xtask check-public-boundary --git-range "$base" "$head"`.
  `check-secrets` needs gitleaks on `PATH`:
  `cargo xtask install-gitleaks <version> <prefix>` installs it, with the
  version from `PINEWASM_GITLEAKS_VERSION` in `ci.yml`;
- every Rust change: `cargo xtask check`;
- dependency or `Cargo.lock` changes: `cargo xtask dependency-policy`;
- binding or Wasm changes: `cargo xtask wasm`;
- compiler, artifact, runtime or binding trust-boundary changes:
  `cargo xtask fuzz-smoke`;
- `tests/compile-pass` fixture changes: `cargo xtask aot-census` (the AOT coverage check);
- workflow or CI planner changes: `actionlint`,
  `cargo xtask workflow-pin-check`,
  `python3 -B -m unittest discover -s .github/ci -p 'test_*.py'` and
  `cargo test --locked -p pinewasm_xtask ci::`.
