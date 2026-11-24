# Compiler versions

One version number names one compiler, and this repository is the only place
that assigns it. `crates/pinewasm_compiler/VERSION` holds the version, and a
host reads the same value from `version().compilerVersion`, the package's
`package.json` and `pinewasm --version`.

The version follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html):

- **Major** for a breaking change: Pine that compiled before no longer does or
  behaves differently by design, a CSIL or Pine language version change, or a
  breaking JavaScript, Rust, ABI or output change a host must adapt to.
- **Minor** for a backwards-compatible feature or newly supported Pine surface.
- **Patch** for a fix, performance work or a dependency update with no new
  surface.

A pull request that changes compiler inputs steps the version by exactly one
Semantic Versioning step (`cargo xtask bump-version patch|minor|major`) and
says in its description what changed. CI checks the step with
`cargo xtask check-compiler-version`. No other repository, fork or branch
assigns compiler versions: a build from anywhere else is identified by its
commit, not by a version of its own.

The version says *which* compiler a build is. Whether a host can load it is a
separate question, answered by the contract versions that `version()` also
reports: `ilVersion` (CSIL), `languageVersion` and `renderingContractVersion`.
The package records the commit it was built from in `version().sourceRevision`
when the checkout was clean.

## Keeping an embedding host current

A host that bundles the generated package goes stale when updating it depends
on someone remembering to do it. These practices make the update routine and
make a stale copy visible:

- **Depend on a version, never a branch or a local build.** Record the
  version, the source commit and the SHA-256 of every package file in one
  lock file. A script regenerates the bundled files from that lock and
  verifies the hashes. Nobody edits the bundled files by hand.
- **Bundle one copy.** A second copy of the compiler with its own lock drifts
  from the first. Features that need another build belong behind package
  options, not in a separate bundle.
- **Find the newest version in one place:** `crates/pinewasm_compiler/VERSION`
  on this repository's `main` branch.
- **Let automation open the update.** A scheduled job, or a dependency bot,
  compares the lock with the newest version and opens a pull request that
  updates the lock, regenerates the bundled files and runs the host's tests.
  A host that is behind then shows up as an open pull request instead of
  silently.
- **Check at load time.** Compare `version().compilerVersion` and
  `version().sourceRevision` with the lock, and check the contract versions
  against the ranges the host supports. Refuse a mismatch instead of running
  a different compiler than the one that was tested.
- **Size the review by the step.** Patch and minor steps should merge once
  the host's tests pass. A major step is a breaking change and may need work
  in the host first; the pull request that made the step says what changed.
- **Resolve versions when the lock is updated, not when building or
  running.** A build from the same lock always uses the same compiler.

Publishing release artifacts, such as a tagged package with checksums, is a
separate step described in [Releasing](releasing.md).
Until then, a host builds the package from this repository's commit for the
version it locks.
