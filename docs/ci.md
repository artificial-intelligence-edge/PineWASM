# Continuous integration

This page describes hosted validation and the local change planner.
[CONTRIBUTING.md](../CONTRIBUTING.md) lists development checks;
[Releasing](releasing.md) defines the additional release requirements.

## Hosted runs

CI runs full validation whenever a push updates `main`, including every merged
pull request. There are no scheduled runs. Maintainers can also dispatch it
manually on `main`; select `full` for a complete rerun. Pull-request events do
not start this workflow. A release requires a successful full run on its exact
commit. Explicit full reruns execute the validation commands again instead of
reusing results from earlier runs.

Every job uses a reviewed GitHub-hosted runner image. The workflows check the
repository name and immutable numeric ID, independently
of repository visibility. Forks and copies cannot report their validation as
PineWASM's own. See [platform support](platform-support.md) for the host,
target, and feature matrix.

The final `CI result` check rejects failed, cancelled, missing, or unexpectedly
skipped selected jobs. Rust tests and package builds wait for the required
policy checks; the secret and policy checks also cover commits added since the base.
Package validation tests the freshly generated JavaScript/Wasm adapter under
Node.js before retaining the package. Successful local checks and cached
build outputs do not replace this hosted result.

An active run on `main` finishes even when newer commits arrive. GitHub retains
only the newest pending run. This preserves a validated ancestor without
claiming that an earlier success validates a later commit.

## Commit-range checks

Each main run checks every commit merged since the last eligible run that
passed. The plan job's `ci-range-base` step finds the base: the head of
the newest successful `CI` run on `main` whose head is
an ancestor of the commit under test and whose workflow found its own base this
way. Until the first such run passes, the base is the unique root commit of
the repository; reviewers check the full tree when it is first pushed. The immutable repository ID is pinned alongside the workflow
identity, and a history with more than one root commit is rejected. If the Actions API cannot
list the runs, planning fails rather than trusting an empty range. From that
base to the run's commit, the run checks:

- every commit for a `Signed-off-by` that matches its author
  (`check-dco BASE HEAD`). Pure merges stay exempt, and Dependabot commits keep
  the exemption they had when a push to `main` checked them;
- every commit for secrets with gitleaks
  (`check-secrets --git-range BASE HEAD`);
- every commit's tree against the repository's path allow and deny rules
  (`check-public-boundary --git-range BASE HEAD`);
- each first-parent step on `main` on its own, which is one merged pull
  request: its squash commit, or its merge commit with everything the merge
  brings in (`check-compiler-version --first-parent BASE HEAD`). Each merged
  pull request that changes compiler inputs must move the version exactly one
  SemVer step, even if several merges arrive before a successful run.

A failed run leaves the base where it was, so the next run checks the same
commits again: a commit on `main` that fails a range check fails every later
run too. Resolve failed checks before promotion; do not skip a failing commit
or replace the baseline merely to make validation pass. A dispatch on a commit
that already passed has no new commits and says so.

To repeat a run's range checks locally, take the base from its plan job's
`Range base` line and run the four commands with that base and the run's
commit. Run the same checks on your branch before opening a pull request;
[CONTRIBUTING.md](../CONTRIBUTING.md) lists them.

## Contribution and workflow protection

Outside pull requests remain subject to the separate
[Contributor acceptance policy](../CONTRIBUTING.md#before-the-first-outside-contribution).
A passing acceptance check is not a full CI run. Preserve the same-repository
and identity guards when changing workflows; repository renames or transfers
require coordinated changes to the workflow, result record and range checks, and
policy tests. Runner routes must match the static allowlist, and no repository
variable or input may choose a runner.

## Local planning

To inspect a plan against a committed base, replace `BASE` with its commit ID:

```sh
cargo run --locked -p pinewasm_xtask -- ci-plan --base BASE --output plan.json
CI_PLAN="$(cat plan.json)" cargo run --locked -p pinewasm_xtask -- ci-run --stage default
cargo test --locked -p pinewasm_xtask ci::
```

The candidate must match the checkout. The planner uses Cargo dependency
metadata and `.github/ci/components.toml` to find affected packages, reverse
consumers, narrative documents, and non-Cargo fixture dependencies. Unknown
inputs, changed toolchains or workflows, and missing or untrusted baseline
evidence require broader validation. Full plans disable success reuse.

An eligible baseline is a successful tested tree verified against immutable
Git data and the run's own identity. A same-repository pull-request run
can qualify after a squash merge only if its exact tested tree appears in the
candidate's history. A similar tree, a missing result record, or a failed run cannot
qualify. This planner capability does not enable pull-request workflow triggers.

Isolated integration-test changes select whole test executables, including
default and all-feature variants; no individual test-name filtering is used.
Shared or deleted inputs select the owning suite. All-feature checks include
the dependency closure. Runtime reproduction selects the affected variants;
shared or uncertain runtime changes rebuild all four. Fixture inputs
and their consumers must be registered explicitly in `components.toml`.

Build caches only accelerate compilation. Each checkout owns its extracted
cache; incomplete restores cannot count as successful checks. Selected jobs
currently execute their checks rather than reusing earlier success. Job
summaries record the selected tasks and reasons; the workflow defines the
current timeouts and cache configuration.
