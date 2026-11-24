//! The compiler release version and the rule that every compiler change bumps it.
//!
//! `crates/pinewasm_compiler/VERSION` holds the one product version that hosts
//! show (`version().compilerVersion`, the npm package version, `pinewasm
//! --version`). It follows Semantic Versioning:
//!
//! * major: a breaking change to accepted Pine, the CSIL wire format, the
//!   JavaScript/Rust API or the ABI, or output a host must handle differently;
//! * minor: a backwards-compatible feature or newly supported Pine surface;
//! * patch: a fix, performance work or a dependency update with no new surface.
//!
//! `check-compiler-version` enforces it: a range that changes compiler inputs
//! must move the version exactly one Semantic Versioning step forward.
//! `bump-version` makes the edit. `--first-parent` applies that rule to each first-parent
//! step of a longer history, so each pull request merged onto `main` is held
//! to one step even when a range spans several merges.

use crate::command::{git, git_is_ancestor, output_allow_failure, repo_root};
use crate::TaskResult;
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) const VERSION_FILE: &str = "crates/pinewasm_compiler/VERSION";
const CSIL_SOURCE: &str = "crates/pinewasm_csil/src/lib.rs";

/// The compiler release version this xtask was built with.
pub(crate) const COMPILER_VERSION: &str =
    include_str!("../../pinewasm_compiler/VERSION").trim_ascii();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub(crate) fn parse(text: &str) -> TaskResult<Self> {
        let text = text.trim();
        let parts: Vec<_> = text.split('.').collect();
        let numbers = (parts.len() == 3)
            .then(|| {
                parts
                    .iter()
                    .map(|part| {
                        let canonical = !part.is_empty()
                            && part.bytes().all(|byte| byte.is_ascii_digit())
                            && (part.len() == 1 || !part.starts_with('0'));
                        canonical.then(|| part.parse::<u64>().ok()).flatten()
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .flatten();
        match numbers.as_deref() {
            Some(&[major, minor, patch]) => Ok(Self {
                major,
                minor,
                patch,
            }),
            _ => Err(format!(
                "{VERSION_FILE} must hold MAJOR.MINOR.PATCH (no prefix, pre-release or build suffix), found {text:?}"
            )),
        }
    }

    fn bumped(self, bump: Bump) -> Self {
        match bump {
            Bump::Major => Self {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
            Bump::Minor => Self {
                minor: self.minor + 1,
                patch: 0,
                ..self
            },
            Bump::Patch => Self {
                patch: self.patch + 1,
                ..self
            },
        }
    }

    /// The single Semantic Versioning step from `self` to `next`, if it is one.
    fn step_to(self, next: Self) -> Option<Bump> {
        [Bump::Major, Bump::Minor, Bump::Patch]
            .into_iter()
            .find(|bump| self.bumped(*bump) == next)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bump {
    Major,
    Minor,
    Patch,
}

impl Bump {
    fn parse(text: &str) -> TaskResult<Self> {
        match text {
            "major" => Ok(Self::Major),
            "minor" => Ok(Self::Minor),
            "patch" => Ok(Self::Patch),
            _ => Err(format!("expected major, minor or patch, found {text:?}")),
        }
    }
}

/// Whether a changed path can change what the compiler accepts or emits, or
/// what the shipped package contains. Tests, examples, prose and repository
/// automation other than the package builder do not. The check is by path:
/// inline `#[cfg(test)]` modules live under `src/` and so count as inputs.
pub(crate) fn is_compiler_input(path: &str) -> bool {
    if matches!(
        path,
        "Cargo.toml"
            | "Cargo.lock"
            | "rust-toolchain.toml"
            | ".cargo/config.toml"
            | "crates/pinewasm_xtask/src/package.rs"
            // `package` links the chart runtime that `aot` builds.
            | "crates/pinewasm_xtask/src/aot.rs"
    ) {
        return true;
    }
    let Some(rest) = path.strip_prefix("crates/") else {
        return false;
    };
    if rest.starts_with("pinewasm_xtask/") {
        return false;
    }
    let segments: Vec<_> = rest.split('/').collect();
    let Some((file, directories)) = segments.split_last() else {
        return false;
    };
    if Path::new(file)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        || *file == "LICENSE"
    {
        return false;
    }
    !directories
        .iter()
        .skip(1)
        .any(|directory| matches!(*directory, "tests" | "benches" | "examples" | "fuzz"))
}

/// Reads `CSIL_VERSION` and `LANGUAGE_VERSION`. Either one missing or written
/// in another form is an error, so a changed declaration can never compare equal.
fn csil_versions(source: &str) -> TaskResult<(u32, u32)> {
    let read = |name: &str| {
        source
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix(&format!("pub const {name}: u32 = "))?
                    .strip_suffix(';')?
                    .parse()
                    .ok()
            })
            .ok_or_else(|| {
                format!(
                    "{CSIL_SOURCE} must declare `pub const {name}: u32 = <decimal>;` so the version check can read it"
                )
            })
    };
    Ok((read("CSIL_VERSION")?, read("LANGUAGE_VERSION")?))
}

/// What a range must satisfy, given the files at its merge base and head.
fn check_step(
    base: Option<Version>,
    head: Version,
    changed: &[String],
    base_csil: (u32, u32),
    head_csil: (u32, u32),
) -> TaskResult {
    let inputs: Vec<_> = changed
        .iter()
        .filter(|path| is_compiler_input(path))
        .collect();
    let Some(base) = base else {
        // The range introduces versioning; there is no earlier release to step from.
        return Ok(());
    };
    if base == head {
        if inputs.is_empty() {
            return Ok(());
        }
        let shown: Vec<_> = inputs.iter().take(10).map(|path| path.as_str()).collect();
        let more = inputs.len().saturating_sub(shown.len());
        return Err(format!(
            "compiler inputs changed but the compiler version is still {head}:\n  {}{}\n\
             Run `cargo xtask bump-version patch|minor|major` (fix: patch, new feature or Pine surface: minor, \
             breaking language/CSIL/API/output change: major).",
            shown.join("\n  "),
            if more > 0 { format!("\n  ... and {more} more") } else { String::new() }
        ));
    }
    let Some(bump) = base.step_to(head) else {
        return Err(format!(
            "the compiler version moves {base} -> {head}; it must move exactly one SemVer step forward \
             ({} patch, {} minor or {} major). If another merge already took {head}, merge main and bump again.",
            base.bumped(Bump::Patch),
            base.bumped(Bump::Minor),
            base.bumped(Bump::Major)
        ));
    };
    if base_csil != head_csil && bump != Bump::Major {
        return Err(format!(
            "the CSIL or Pine language version changed ({base_csil:?} -> {head_csil:?}), which breaks existing \
             programs or hosts; that needs a major bump ({base} -> {}), not {head}",
            base.bumped(Bump::Major)
        ));
    }
    Ok(())
}

fn resolve(root: &Path, revision: &str) -> TaskResult<String> {
    let result = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "rev-parse",
        "--quiet",
        "--verify",
        &format!("{revision}^{{commit}}"),
    ]))?;
    if result.status.success() {
        Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
    } else {
        Err(format!(
            "not a commit in this repository: {revision}; fetch full history"
        ))
    }
}

fn file_at(root: &Path, revision: &str, path: &str) -> TaskResult<Option<String>> {
    let result = output_allow_failure(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show", &format!("{revision}:{path}")]),
    )?;
    Ok(result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).into_owned()))
}

fn check_tree(root: &Path) -> TaskResult<Version> {
    let path = root.join(VERSION_FILE);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {VERSION_FILE}: {error}"))?;
    Version::parse(&text)
}

/// Checks one range and returns what it found.
fn check_range(root: &Path, base: &str, head: &str) -> TaskResult<String> {
    let head = resolve(root, head)?;
    let head_version = Version::parse(
        &file_at(root, &head, VERSION_FILE)?
            .ok_or_else(|| format!("{VERSION_FILE} is missing at {head}"))?,
    )?;
    // Only the exact all-zero object id means a branch-creating push. An
    // empty or shortened base must not skip the check.
    if base.len() == 40 && base.bytes().all(|byte| byte == b'0') {
        return Ok(format!(
            "Compiler version {head_version}: branch-creating push, no earlier release to compare."
        ));
    }
    let base = resolve(root, base)?;
    let merge_base = git(root, &["merge-base", &base, &head])?.trim().to_owned();
    let changed: Vec<String> = git(
        root,
        &["diff", "--name-only", "--no-renames", &merge_base, &head],
    )?
    .lines()
    .map(ToOwned::to_owned)
    .collect();
    let base_version = file_at(root, &merge_base, VERSION_FILE)?
        .map(|text| Version::parse(&text))
        .transpose()?;
    let csil = |revision: &str| -> TaskResult<_> {
        let source = file_at(root, revision, CSIL_SOURCE)?
            .ok_or_else(|| format!("{CSIL_SOURCE} is missing at {revision}"))?;
        csil_versions(&source)
    };
    check_step(
        base_version,
        head_version,
        &changed,
        csil(&merge_base)?,
        csil(&head)?,
    )?;
    Ok(match base_version {
        Some(base) if base != head_version => {
            format!("Compiler version {base} -> {head_version}.")
        }
        Some(_) => format!("Compiler version {head_version}: no compiler inputs changed."),
        None => format!("Compiler version {head_version}: introduced by this range."),
    })
}

/// Checks each first-parent step from `base` to `head` as its own range. On
/// `main` one step is one merged pull request: its squash commit, or its merge
/// commit with everything that merge brings in. A range over several merges
/// may move the version several steps; each merge may move it only one.
fn check_first_parent(root: &Path, base: &str, head: &str) -> TaskResult {
    for value in [base, head] {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "--first-parent needs exact 40-character commit ids, found {value:?}"
            ));
        }
    }
    let base = resolve(root, base)?;
    let head = resolve(root, head)?;
    if !git_is_ancestor(root, &base, &head)? {
        return Err(format!("{base} is not an ancestor of {head}"));
    }
    let steps = git(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--reverse",
            "--parents",
            &format!("{base}..{head}"),
        ],
    )?;
    let mut parent = base.clone();
    let mut count = 0usize;
    let mut failures = Vec::new();
    for line in steps.lines() {
        let mut ids = line.split_whitespace();
        let (Some(commit), Some(first_parent)) = (ids.next(), ids.next()) else {
            return Err(format!("unexpected first-parent history entry {line:?}"));
        };
        // Each step must start where the previous one ended. A base reached
        // only through a merge's second parent leaves no step boundary there.
        if first_parent != parent {
            return Err(format!(
                "{base} is not on the first-parent history of {head}: {commit} follows {first_parent}"
            ));
        }
        count += 1;
        let subject = git(root, &["show", "-s", "--format=%s", commit])?;
        let step = format!("{} {}", &commit[..12], subject.trim());
        match check_range(root, first_parent, commit) {
            Ok(message) => println!("{step}: {message}"),
            Err(error) => failures.push(format!("{step}: {error}")),
        }
        commit.clone_into(&mut parent);
    }
    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("ERROR: {failure}");
        }
        return Err(format!(
            "{} of {count} first-parent step(s) from {} to {} break the compiler version rule",
            failures.len(),
            &base[..12],
            &head[..12]
        ));
    }
    if count == 0 {
        println!("Compiler version: {base} is {head}; no first-parent steps to check.");
    } else {
        println!(
            "Compiler version: all {count} first-parent step(s) from {} to {} pass.",
            &base[..12],
            &head[..12]
        );
    }
    Ok(())
}

pub(crate) fn check(args: &[String]) -> TaskResult {
    let root = repo_root()?;
    match args {
        [] => {
            let version = check_tree(&root)?;
            println!("Compiler version {version}.");
            Ok(())
        }
        [flag, base, head] if flag == "--git-range" => {
            println!("{}", check_range(&root, base, head)?);
            Ok(())
        }
        [flag, base, head] if flag == "--first-parent" => check_first_parent(&root, base, head),
        _ => Err(
            "usage: check-compiler-version [--git-range BASE HEAD | --first-parent BASE HEAD]"
                .into(),
        ),
    }
}

pub(crate) fn bump(args: &[String]) -> TaskResult {
    let [kind] = args else {
        return Err("usage: bump-version major|minor|patch".into());
    };
    let bump = Bump::parse(kind)?;
    let root = repo_root()?;
    let path = root.join(VERSION_FILE);
    let current = Version::parse(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {VERSION_FILE}: {error}"))?,
    )?;
    let next = current.bumped(bump);
    fs::write(&path, format!("{next}\n"))
        .map_err(|error| format!("cannot write {VERSION_FILE}: {error}"))?;
    println!("Compiler version {current} -> {next}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_checked_in_version_is_canonical_semver() {
        assert_eq!(v(COMPILER_VERSION).to_string(), COMPILER_VERSION);
    }

    #[test]
    fn parse_rejects_non_canonical_versions() {
        for bad in [
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "01.2.3",
            "1.2.3-rc.1",
            "1..3",
            "",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(v(" 10.0.7\n").to_string(), "10.0.7");
    }

    #[test]
    fn only_single_forward_steps_count() {
        assert_eq!(v("0.2.0").step_to(v("0.2.1")), Some(Bump::Patch));
        assert_eq!(v("0.2.5").step_to(v("0.3.0")), Some(Bump::Minor));
        assert_eq!(v("0.2.5").step_to(v("1.0.0")), Some(Bump::Major));
        for bad in ["0.2.2", "0.3.1", "0.2.0", "0.1.9", "2.0.0", "1.0.1"] {
            assert_eq!(v("0.2.0").step_to(v(bad)), None, "{bad}");
        }
    }

    #[test]
    fn compiler_inputs_exclude_tests_prose_and_automation() {
        for input in [
            "crates/pinewasm_hir/src/lower.rs",
            "crates/pinewasm_compiler/VERSION",
            "crates/pinewasm_aot/runtime/pinewasm_aot_runtime.wasm",
            "crates/pinewasm_aot/build.rs",
            "crates/pinewasm_vm/Cargo.toml",
            "crates/pinewasm_xtask/src/package.rs",
            "crates/pinewasm_xtask/src/aot.rs",
            "Cargo.lock",
            "rust-toolchain.toml",
        ] {
            assert!(is_compiler_input(input), "{input}");
        }
        for other in [
            "crates/pinewasm_hir/tests/lower.rs",
            "crates/pinewasm_compiler/tests/fixtures/a.json",
            "crates/pinewasm_aot/examples/demo.rs",
            "crates/pinewasm_bindings/README.md",
            "crates/pinewasm_bindings/LICENSE",
            "crates/pinewasm_xtask/src/ci/mod.rs",
            "docs/ir.md",
            "tests/compile-pass/a.pine",
            ".github/workflows/ci.yml",
            "README.md",
        ] {
            assert!(!is_compiler_input(other), "{other}");
        }
    }

    #[test]
    fn ranges_that_change_compiler_inputs_must_step_the_version() {
        let compiler = vec!["crates/pinewasm_parse/src/lib.rs".to_owned()];
        let tests_only = vec!["crates/pinewasm_parse/tests/parse.rs".to_owned()];
        let csil = (2, 6);
        assert!(check_step(Some(v("0.2.0")), v("0.2.0"), &compiler, csil, csil).is_err());
        assert!(check_step(Some(v("0.2.0")), v("0.2.0"), &tests_only, csil, csil).is_ok());
        assert!(check_step(Some(v("0.2.0")), v("0.2.1"), &compiler, csil, csil).is_ok());
        assert!(check_step(Some(v("0.2.0")), v("0.2.1"), &tests_only, csil, csil).is_ok());
        assert!(check_step(Some(v("0.2.0")), v("0.2.2"), &compiler, csil, csil).is_err());
        assert!(check_step(Some(v("0.2.1")), v("0.2.0"), &compiler, csil, csil).is_err());
        assert!(check_step(None, v("0.2.0"), &compiler, csil, csil).is_ok());
    }

    #[test]
    fn a_csil_or_language_version_change_needs_a_major_bump() {
        let changed = vec!["crates/pinewasm_csil/src/lib.rs".to_owned()];
        let old = (2, 6);
        let new = (3, 6);
        assert!(check_step(Some(v("0.2.0")), v("0.3.0"), &changed, old, new).is_err());
        assert!(check_step(Some(v("0.2.0")), v("1.0.0"), &changed, old, new).is_ok());
        let source =
            "pub const CSIL_VERSION: u32 = 2;\n/// x\npub const LANGUAGE_VERSION: u32 = 6;\n";
        assert_eq!(csil_versions(source), Ok((2, 6)));
        assert!(csil_versions(
            "pub const CSIL_VERSION: u32 = 2u32;\npub const LANGUAGE_VERSION: u32 = 6;\n"
        )
        .is_err());
        assert!(csil_versions("pub const CSIL_VERSION: u32 = 2;\n").is_err());
    }

    /// A throwaway repository holding the files the range check reads.
    struct History(crate::command::TempDir);

    impl History {
        fn new() -> Self {
            let history = Self(crate::command::temp_dir("version-history").unwrap());
            history.git(&["init", "--quiet"]);
            history.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
            history.git(&["config", "user.name", "Version Test"]);
            history.git(&["config", "user.email", "version-test@example.invalid"]);
            history.git(&["config", "commit.gpgsign", "false"]);
            history.git(&["config", "core.hooksPath", "no-hooks"]);
            history.write(
                CSIL_SOURCE,
                "pub const CSIL_VERSION: u32 = 2;\npub const LANGUAGE_VERSION: u32 = 6;\n",
            );
            history
        }

        fn root(&self) -> &Path {
            self.0.path()
        }

        fn git(&self, args: &[&str]) -> String {
            git(self.root(), args).unwrap().trim().to_owned()
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.root().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// Commits a compiler change released as `version`.
        fn release(&self, version: &str, summary: &str) -> String {
            self.write(VERSION_FILE, &format!("{version}\n"));
            self.write(
                "crates/pinewasm_parse/src/lib.rs",
                &format!("// {summary}\n"),
            );
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "-m", summary]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    #[test]
    fn each_first_parent_step_may_move_the_version_one_step() {
        let history = History::new();
        let root = history.root();
        let base = history.release("0.2.0", "baseline");
        history.release("0.2.1", "a fix");
        let second = history.release("0.2.2", "another fix");
        // One range over both merges moves two steps; each merge moved one.
        assert!(check_range(root, &base, &second).is_err());
        assert_eq!(check_first_parent(root, &base, &second), Ok(()));
        assert_eq!(check_first_parent(root, &second, &second), Ok(()));
        let skipped = history.release("0.2.4", "two bumps in one merge");
        let error = check_first_parent(root, &base, &skipped).unwrap_err();
        assert!(error.starts_with("1 of 3 first-parent step(s)"), "{error}");
    }

    #[test]
    fn a_merge_commit_is_one_step_with_everything_it_brings_in() {
        let history = History::new();
        let root = history.root();
        let base = history.release("0.2.0", "baseline");
        history.git(&["checkout", "--quiet", "-b", "topic"]);
        let side = history.release("0.2.1", "first bump on the branch");
        history.release("0.2.2", "second bump on the branch");
        history.git(&["checkout", "--quiet", "main"]);
        history.git(&["merge", "--quiet", "--no-ff", "-m", "merge topic", "topic"]);
        let merge = history.git(&["rev-parse", "HEAD"]);
        let error = check_first_parent(root, &base, &merge).unwrap_err();
        assert!(error.starts_with("1 of 1 first-parent step(s)"), "{error}");
        // A base reached only through a merge's second parent is no step boundary.
        let error = check_first_parent(root, &side, &merge).unwrap_err();
        assert!(
            error.contains("is not on the first-parent history"),
            "{error}"
        );
        history.git(&["checkout", "--quiet", "-b", "single"]);
        history.release("0.2.3", "one bump on the branch");
        history.git(&["checkout", "--quiet", "main"]);
        history.git(&[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge single",
            "single",
        ]);
        let next = history.git(&["rev-parse", "HEAD"]);
        assert_eq!(check_first_parent(root, &merge, &next), Ok(()));
    }

    #[test]
    fn a_range_base_must_be_an_exact_ancestor() {
        let history = History::new();
        let root = history.root();
        let base = history.release("0.2.0", "baseline");
        let head = history.release("0.2.1", "a fix");
        for bad in ["", "0", &base[..12]] {
            assert!(check_first_parent(root, bad, &head).is_err(), "{bad:?}");
        }
        // An empty or shortened zero base is not the all-zero branch-creation id.
        for bad in ["", "0"] {
            assert!(check_range(root, bad, &head).is_err(), "{bad:?}");
        }
        assert!(check_range(root, &"0".repeat(40), &head).is_ok());
        history.git(&["checkout", "--quiet", "-b", "unmerged", &base]);
        let unmerged = history.release("0.2.1", "never merged");
        let error = check_first_parent(root, &unmerged, &head).unwrap_err();
        assert!(error.contains("is not an ancestor"), "{error}");
    }
}
