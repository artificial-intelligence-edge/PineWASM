use crate::command::{git, git_bytes, nul_fields, output_allow_failure, repo_root, run, temp_dir};
use crate::TaskResult;
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Paths that may be published. Each entry is an exact path or a narrow pattern.
const PUBLIC_PATTERNS: &[&str] = &[
    r"^crates/pinewasm_compiler/tests/fixtures/README\.md$",
    r"^crates/pinewasm_compiler/tests/fixtures/drawing-constants\.json$",
    r"^\.github/CODEOWNERS$",
    r"^\.github/actionlint\.yaml$",
    r"^\.github/ISSUE_TEMPLATE/config\.yml$",
    r"^\.github/ISSUE_TEMPLATE/(bug_report|divergence_report|feature_request)\.md$",
    r"^\.github/PULL_REQUEST_TEMPLATE\.md$",
    r"^\.github/runtime-safety-toolchain\.toml$",
    r"^\.github/ci/components\.toml$",
    r"^\.github/ci/(native_cache|test_native_cache|test_policy_preflight|test_contributor_policy_dispatch|toolchain_identity|test_toolchain_identity|install_job_gh|test_install_job_gh|emit_owned_record|test_emit_owned_record)\.py$",
    r"^docs/ci\.md$",
    r"^\.github/workflows/(ci|runtime-safety|contributor-policy)\.yml$",
    r"^\.(gitattributes|gitignore)$",
    r"^(CODE_OF_CONDUCT|CONTRIBUTING|SECURITY)\.md$",
    // Repository instruction files. Local tool state is denied separately.
    r"^(AGENTS|CLAUDE)\.md$",
    r"^(Cargo\.lock|Cargo\.toml|LICENSE|NOTICE|README\.md|THIRD_PARTY_LICENSES\.txt|clippy\.toml|deny\.toml|rust-toolchain\.toml)$",
    // Compiler version file read by `cargo xtask bump-version`.
    r"^crates/pinewasm_compiler/VERSION$",
    r"^\.cargo/config\.toml$",
    r"^crates/(pinewasm_aot|pinewasm_broker|pinewasm_compiler|pinewasm_hir|pinewasm_csil|pinewasm_lsp|pinewasm_migrate|pinewasm_mir|pinewasm_num|pinewasm_parse|pinewasm_search|pinewasm_semantics|pinewasm_span|pinewasm_syntax|pinewasm_ta|pinewasm_tz|pinewasm_vm|pinewasm_bindings|pinewasm_xtask)/(Cargo\.toml|README\.md)$",
    r"^crates/pinewasm_bindings/LICENSE$",
    r"^crates/pinewasm_lsp/fixtures/language_queries\.json$",
    r"^crates/pinewasm_aot/build\.rs$",
    r"^crates/(pinewasm_aot|pinewasm_broker|pinewasm_compiler|pinewasm_hir|pinewasm_csil|pinewasm_lsp|pinewasm_migrate|pinewasm_mir|pinewasm_num|pinewasm_parse|pinewasm_search|pinewasm_semantics|pinewasm_span|pinewasm_syntax|pinewasm_ta|pinewasm_tz|pinewasm_vm|pinewasm_bindings|pinewasm_xtask)/(src|tests|examples)/[A-Za-z0-9_./-]+\.rs$",
    r"^crates/(pinewasm_csil|pinewasm_mir|pinewasm_parse|pinewasm_semantics)/src/snapshots/[A-Za-z0-9_.-]+\.snap$",
    r"^crates/pinewasm_aot/runtime/(\.gitignore|Cargo\.lock|Cargo\.toml)$",
    r"^crates/pinewasm_aot/runtime/\.cargo/config\.toml$",
    r"^crates/pinewasm_aot/runtime/src/[A-Za-z0-9_./-]+\.rs$",
    r"^crates/pinewasm_aot/runtime/pinewasm_aot_runtime(_standalone_wasm|_broker|_ta)?\.wasm$",
    r"^crates/pinewasm_csil/src/fixtures/csil_v2_wire_compatibility\.json$",
    r"^docs/assets/ai-edge-logo\.png$",
    r"^docs/assets/pinewasm-logo(-dark)?\.png$",
    r"^docs/execution-explained\.md$",
    r"^docs/README\.md$",
    r"^docs/(aot|aot-runtime-safety|architecture|artifact-distribution|benchmarks|browser|fuzzing|ir|language-server|language-support|libraries|licensing|migration|pine-v6-coverage|platform-support|provenance|releasing|runtime|rust-api|trademarks|versioning)\.md$",
    r"^docs/spec/(LICENSE|README\.md|csaw-module-abi\.md)$",
    r"^tests/README\.md$",
    r"^tests/(compile-fail|compile-pass|regression)/[A-Za-z0-9_.-]+\.pine$",
    r"^crates/pinewasm_migrate/tests/scripts/(README\.md|[a-z0-9_]+\.(v5\.pine|v6\.pine|findings))$",
    r"^benchmarks/manifests/(node-indicators-v1|subject-tiers-v1)\.json$",
    r"^benchmarks/workloads/(history|arithmetic|sma|control|persistent|collections|legacy-mixed)\.pine$",
];

// Paths that are never published. Checked before the allowlist.
const DENY_PATTERNS: &[&str] = &[
    // Dependency-update bot configuration is not used in this repository.
    r"^\.github/dependabot\.ya?ml$",
    // Local tool state, editor settings, and secret-bearing files.
    r"(^|/)\.claude(/|$)",
    r"(^|/)\.mcp\.json$",
    r"(^|/)\.vscode(/|$)",
    r"\.(env|pem|key|p12)$",
    // No JavaScript or TypeScript sources are maintained in this repository.
    r"(?i)\.(js|mjs|cjs|jsx|ts|mts|cts|tsx)$",
];

// Exact paths of data fixtures that the allowlist patterns above do not cover.
// Only these paths bypass the built-in deny rules; content, secret, symlink,
// and history checks still apply to them.
const DATA_FILE_EXCEPTIONS: &[&str] = &[
    "crates/pinewasm_compiler/tests/fixtures/legacy-color-accessors.json",
    "crates/pinewasm_compiler/tests/fixtures/plotchar_character_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_static_role_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_runtime_colorer_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_coupled_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_conditional_alpha_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_boolean_and_string_default_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_cosine_guard_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_missing_guard_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_helper_callsite_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_body_boundary_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_body_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_body_helper_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_body_missing_origin_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_input_default_alpha_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/ohlc_input_property_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_constant_guard_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_history_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/arrow_nested_loop_guard_cases.json",
    "crates/pinewasm_compiler/tests/fixtures/const_color_t_numeric_cases.json",
    "crates/pinewasm_csil/src/fixtures/coalescing_scalar.json",
    "crates/pinewasm_csil/src/fixtures/coalescing_scalar_before.json",
    "tests/fixtures/rendering/frozen-color-alpha.json",
    "tests/fixtures/rendering/named-colors.json",
];

// Case-insensitive content markers that are rejected in every published file.
const MARKER_PATTERNS: &[&str] = &[
    r"REPLACE-ME",
    r"TODO\(owner\)",
    // Absolute home-directory paths must not appear in published files.
    r"/(Users|home)/[A-Za-z0-9._-]+/",
    // Merge conflict markers.
    r"(?m)^<<<<<<< ",
    r"(?m)^=======$",
    r"(?m)^>>>>>>> ",
];

const REQUIRED_FILES: &[&str] = &[
    "LICENSE",
    "crates/pinewasm_bindings/LICENSE",
    "NOTICE",
    "THIRD_PARTY_LICENSES.txt",
    "docs/licensing.md",
    "CONTRIBUTING.md",
    "docs/trademarks.md",
    "docs/artifact-distribution.md",
    "docs/rust-api.md",
    "docs/README.md",
    "docs/spec/LICENSE",
    "tests/README.md",
];

const SELF_PATH: &str = "crates/pinewasm_xtask/src/boundary.rs";

// Directories whose .pine files are checked for disallowed markers.
const PINE_FIXTURE_PREFIXES: &[&str] = &[
    "tests/compile-fail/",
    "tests/compile-pass/",
    "tests/regression/",
    "benchmarks/workloads/",
    "crates/pinewasm_migrate/tests/scripts/",
];

// Case-insensitive substrings rejected in .pine fixtures. Further markers come
// from the optional extra rules file (`pine-marker`).
const PINE_PROVENANCE_MARKERS: &[&str] = &[
    "http://",
    "https://",
    "www.",
    "copyright",
    "all rights reserved",
    "@author",
];

// Optional extra rules. They are read only when this variable names a file;
// otherwise only the rules in this source file apply.
const EXTRA_RULES_ENV: &str = "PINEWASM_BOUNDARY_EXTRA";

// Rules read from the file named by EXTRA_RULES_ENV. One rule per line as
// `<rule> <value>`; blank lines and lines starting with `#` are ignored.
//   deny <regex>        path rule that applies to every path
//   marker <regex>      content rule, searched in file contents
//   pine-marker <text>  case-insensitive text rejected in .pine fixtures
//   allow <regex>       extra path added to the publishable allowlist
#[derive(Default)]
struct ExtraRules {
    deny: Vec<String>,
    marker: Vec<String>,
    pine_marker: Vec<String>,
    allow: Vec<String>,
}

impl ExtraRules {
    fn load(path: &Path) -> TaskResult<Self> {
        let text = fs::read_to_string(path).map_err(|error| {
            format!(
                "cannot read {EXTRA_RULES_ENV} file {}: {error}",
                path.display()
            )
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    fn parse(text: &str, origin: &str) -> TaskResult<Self> {
        let mut rules = Self::default();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let location = format!("{origin}:{}", index + 1);
            let (kind, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let value = value.trim();
            if value.is_empty() {
                return Err(format!(
                    "{location}: expected '<rule> <value>', found {line:?}"
                ));
            }
            let target = match kind {
                "deny" => &mut rules.deny,
                "marker" => &mut rules.marker,
                "pine-marker" => &mut rules.pine_marker,
                "allow" => &mut rules.allow,
                _ => return Err(format!("{location}: unknown rule {kind:?}")),
            };
            target.push(value.to_string());
        }
        Ok(rules)
    }
}

struct Matchers {
    public: Vec<Regex>,
    allowed: Vec<Regex>,
    denied: Vec<Regex>,
    extra_denied: Vec<Regex>,
    markers: Vec<(Regex, String)>,
    pine_markers: Vec<String>,
}

impl Matchers {
    fn new() -> TaskResult<Self> {
        let extra = match env::var_os(EXTRA_RULES_ENV) {
            Some(path) if !path.is_empty() => ExtraRules::load(Path::new(&path))?,
            _ => ExtraRules::default(),
        };
        Self::with_extra(&extra)
    }

    fn built_in() -> TaskResult<Self> {
        Self::with_extra(&ExtraRules::default())
    }

    fn with_extra(extra: &ExtraRules) -> TaskResult<Self> {
        let mut markers = Vec::new();
        for source in MARKER_PATTERNS
            .iter()
            .copied()
            .chain(extra.marker.iter().map(String::as_str))
        {
            markers.push((compile_one(source, true)?, source.to_string()));
        }
        Ok(Self {
            public: compile(PUBLIC_PATTERNS, false)?,
            allowed: compile(&extra.allow, false)?,
            denied: compile(DENY_PATTERNS, true)?,
            extra_denied: compile(&extra.deny, true)?,
            markers,
            pine_markers: extra
                .pine_marker
                .iter()
                .map(|marker| marker.to_ascii_lowercase())
                .collect(),
        })
    }

    fn public(&self, value: &str) -> bool {
        DATA_FILE_EXCEPTIONS.contains(&value)
            || self
                .public
                .iter()
                .chain(&self.allowed)
                .any(|pattern| pattern.is_match(value))
    }

    fn denied(&self, value: &str) -> bool {
        (!DATA_FILE_EXCEPTIONS.contains(&value)
            && self.denied.iter().any(|pattern| pattern.is_match(value)))
            || self
                .extra_denied
                .iter()
                .any(|pattern| pattern.is_match(value))
    }

    fn marker(&self, bytes: &[u8]) -> Option<&str> {
        let text = String::from_utf8_lossy(bytes);
        self.markers
            .iter()
            .find_map(|(regex, source)| regex.is_match(&text).then_some(source.as_str()))
    }

    fn pine_fixture_failures(&self, relative: &str, bytes: &[u8]) -> Vec<String> {
        if !Path::new(relative)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pine"))
        {
            return Vec::new();
        }

        let mut failures = Vec::new();
        if !PINE_FIXTURE_PREFIXES
            .iter()
            .any(|prefix| relative.starts_with(prefix))
        {
            failures.push(format!(
                "Pine source must live in an allowlisted tests directory: {relative}"
            ));
            return failures;
        }

        let Ok(text) = std::str::from_utf8(bytes) else {
            failures.push(format!("Pine fixture is not UTF-8 text: {relative}"));
            return failures;
        };
        let lowercase = text.to_ascii_lowercase();
        let markers = PINE_PROVENANCE_MARKERS
            .iter()
            .copied()
            .chain(self.pine_markers.iter().map(String::as_str));
        for marker in markers {
            if lowercase.contains(marker) {
                failures.push(format!(
                    "Pine fixture contains a disallowed marker {marker:?}: {relative}"
                ));
            }
        }
        failures
    }

    fn benchmark_archive_failure(&self, relative: &str, bytes: &[u8]) -> Option<String> {
        if !relative.starts_with("benchmarks/") || !relative.ends_with(".json.gz") {
            return None;
        }
        match crate::benchmark_run::archive_contents(bytes) {
            Ok(value) => {
                let expanded = value.to_string();
                self.marker(expanded.as_bytes()).map(|pattern| {
                    format!("marker {pattern:?} in expanded benchmark bundle: {relative}")
                })
            }
            Err(error) => Some(format!("invalid benchmark bundle {relative}: {error}")),
        }
    }
}

fn compile<S: AsRef<str>>(patterns: &[S], insensitive: bool) -> TaskResult<Vec<Regex>> {
    patterns
        .iter()
        .map(|pattern| compile_one(pattern.as_ref(), insensitive))
        .collect()
}

fn compile_one(pattern: &str, insensitive: bool) -> TaskResult<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(insensitive)
        .build()
        .map_err(|error| format!("invalid boundary expression {pattern:?}: {error}"))
}

pub(crate) fn check(args: &[String]) -> TaskResult {
    let built_in = Matchers::built_in()?;
    matcher_self_test(&built_in)?;
    extra_rules_self_test()?;
    let matchers = Matchers::new()?;
    match args {
        [] => scan_tree(&repo_root()?, false, &matchers),
        [flag] if flag == "--self-test" => history_self_test(&built_in),
        [flag] if flag == "--staged" => scan_tree(&repo_root()?, true, &matchers),
        [flag, directory] if flag == "--repo" => {
            let root = PathBuf::from(directory)
                .canonicalize()
                .map_err(|error| format!("not a directory: {directory}: {error}"))?;
            git(&root, &["rev-parse", "--git-dir"])?;
            scan_tree(&root, false, &matchers)
        }
        [flag, base, head] if flag == "--git-range" => {
            scan_range(&repo_root()?, base, head, &matchers)
        }
        _ => Err("usage: check-public-boundary [--self-test | --staged | --repo DIR | --git-range BASE_SHA HEAD_SHA]".into()),
    }
}

pub(crate) fn install_hook() -> TaskResult {
    let root = repo_root()?;
    // `core.hooksPath` lives in the repository's shared config. Keep its
    // target in the shared Git directory too; a per-worktree gitdir would
    // leave every checkout pointing at a dead hook after that worktree is
    // removed.
    let common_directory = PathBuf::from(
        git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    );
    let hook_directory = common_directory.join("pinewasm-hooks");
    fs::create_dir_all(&hook_directory)
        .map_err(|error| format!("cannot create {}: {error}", hook_directory.display()))?;
    let destination = hook_directory.join("pre-commit");
    let source =
        env::current_exe().map_err(|error| format!("cannot locate xtask binary: {error}"))?;
    fs::copy(&source, &destination)
        .map_err(|error| format!("cannot install {}: {error}", destination.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("cannot set hook permissions: {error}"))?;
    }
    run(Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["config", "core.hooksPath"])
        .arg(&hook_directory))?;
    println!(
        "Installed Rust pre-commit boundary guard: {}",
        destination.display()
    );
    Ok(())
}

fn matcher_self_test(matchers: &Matchers) -> TaskResult {
    if matchers.public(".github/ci/unlisted_helper.py") {
        return Err("CI helper allowance matched an unlisted path".into());
    }
    for path in DATA_FILE_EXCEPTIONS {
        if !matchers.public(path) || matchers.denied(path) {
            return Err(format!("data exception path was rejected: {path}"));
        }
    }
    if matchers.public("tests/fixtures/rendering/named-colors-copy.json") {
        return Err("data exception matched a neighboring path".into());
    }
    for path in [".github/dependabot.yml", ".github/dependabot.yaml"] {
        if matchers.public(path) || !matchers.denied(path) {
            return Err(format!("deny rule did not reject: {path}"));
        }
    }

    if benchmark_size_failure("benchmarks/manifests/node-indicators-v1.json", 65 * 1024).is_none()
        || benchmark_size_failure("benchmarks/manifests/node-indicators-v1.json", 64 * 1024)
            .is_some()
        || benchmark_size_failure("benchmarks/local.json.gz", 17 * 1024 * 1024).is_none()
        || !matchers
            .pine_fixture_failures(
                "benchmarks/workloads/history.pine",
                b"//@version=6\nindicator(\"History\")\nplot(close)\n",
            )
            .is_empty()
        || matchers
            .pine_fixture_failures(
                "benchmarks/workloads/history.pine",
                b"// https://example.com/copied\n",
            )
            .is_empty()
    {
        return Err("benchmark boundary size/marker self-test failed".into());
    }
    for path in [
        "crates/pinewasm_compiler/src/lib.rs",
        "crates/pinewasm_xtask/src/main.rs",
        "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_broker.wasm",
        "docs/benchmarks.md",
        ".github/CODEOWNERS",
        ".github/runtime-safety-toolchain.toml",
        ".github/workflows/runtime-safety.yml",
        ".github/ci/native_cache.py",
        ".github/ci/test_native_cache.py",
        ".github/ci/test_policy_preflight.py",
        "docs/aot-runtime-safety.md",
        "docs/artifact-distribution.md",
        "docs/assets/ai-edge-logo.png",
        "docs/assets/pinewasm-logo.png",
        "docs/assets/pinewasm-logo-dark.png",
        "docs/execution-explained.md",
        "docs/releasing.md",
        "docs/README.md",
        "docs/licensing.md",
        "CONTRIBUTING.md",
        "docs/fuzzing.md",
        "docs/platform-support.md",
        "docs/rust-api.md",
        "tests/README.md",
        "tests/regression/numeric-runtime.pine",
        "THIRD_PARTY_LICENSES.txt",
        ".gitattributes",
        "tests/regression/language-indicator.pine",
        "crates/pinewasm_migrate/tests/scripts/README.md",
        "crates/pinewasm_migrate/tests/scripts/trend_dashboard.v5.pine",
        "crates/pinewasm_migrate/tests/scripts/trend_dashboard.v6.pine",
        "crates/pinewasm_migrate/tests/scripts/trend_dashboard.findings",
        "benchmarks/manifests/node-indicators-v1.json",
        "benchmarks/manifests/subject-tiers-v1.json",
        "benchmarks/workloads/history.pine",
        "crates/pinewasm_compiler/src/bin/pinewasm_benchmark_subject.rs",
        "crates/pinewasm_xtask/src/benchmark_subject.rs",
        "AGENTS.md",
        "CLAUDE.md",
    ] {
        if !matchers.public(path) || matchers.denied(path) {
            return Err(format!("boundary self-test expected a public path: {path}"));
        }
    }
    for path in [
        ".mcp.json",
        ".claude/settings.json",
        ".vscode/settings.json",
        "config/service.env",
        "certs/server.pem",
        "certs/deploy.key",
        "certs/client.p12",
        "docs/example.js",
        "docs/example.mjs",
        "docs/example.MJS",
        "docs/example.cjs",
        "docs/example.cts",
        "docs/example.mts",
        "docs/example.jsx",
        "docs/example.tsx",
        "crates/pinewasm_vm/src/helper.ts",
    ] {
        if !matchers.denied(path) {
            return Err(format!("boundary self-test expected a denied path: {path}"));
        }
    }
    for path in [
        "notes/scratch.md",
        "docs/unlisted-page.md",
        "crates/pinewasm_scratch/src/lib.rs",
        "scripts/local-helper.sh",
        "scripts/unlisted_helper.py",
        "tests/regression/expected-output.txt",
        "examples/demo.pine",
        "crates/pinewasm_migrate/tests/scripts/notes.txt",
        "crates/pinewasm_migrate/tests/scripts/Upper.v5.pine",
        "crates/pinewasm_compiler/tests/scripts/demo.v5.pine",
        "docs/assets/unlisted-image.png",
        "benchmarks/manifests/unlisted-v2.json",
        "benchmarks/workloads/unlisted.pine",
        ".github/ci/unlisted_helper.py",
        ".github/workflows/unlisted.yml",
    ] {
        if matchers.public(path) {
            return Err(format!(
                "boundary self-test expected a path outside the allowlist: {path}"
            ));
        }
    }
    for path in [
        "crates/pinewasm_compiler/src/browser.rs",
        "crates/pinewasm_compiler/src/cursor.rs",
        "crates/pinewasm_compiler/src/import.rs",
        "crates/pinewasm_compiler/src/reference.rs",
    ] {
        if !matchers.public(path) || matchers.denied(path) {
            return Err(format!(
                "boundary self-test expected a neutral compiler basename to remain public: {path}"
            ));
        }
    }
    // Marker self-test inputs, joined from parts. This file is already exempt
    // from the marker scan through SELF_PATH.
    for marker in [
        ["Replace", "-Me"].concat(),
        ["TODO", "(Owner)"].concat(),
        ["/Users/", "example-developer/build.log"].concat(),
        ["/home/", "example-developer/build.log"].concat(),
        ["<<<", "<<<< HEAD"].concat(),
        ["===", "===="].concat(),
        [">>>", ">>>> branch"].concat(),
    ] {
        if matchers.marker(marker.as_bytes()).is_none() {
            return Err(format!(
                "boundary self-test expected a content marker: {marker}"
            ));
        }
    }
    for public_content in [
        "https://github.com/example/PineWASM",
        "https://www.example.com/docs/",
        "https://static.example.com/assets/",
        "https://example.com/check",
    ] {
        if matchers.marker(public_content.as_bytes()).is_some() {
            return Err(format!(
                "boundary self-test rejected public content: {public_content}"
            ));
        }
    }
    if !matchers
        .pine_fixture_failures(
            "tests/compile-pass/minimal.pine",
            b"//@version=6\nindicator(\"Minimal\")\nplot(close)\n",
        )
        .is_empty()
        || !matchers
            .pine_fixture_failures(
                "crates/pinewasm_migrate/tests/scripts/minimal.v5.pine",
                b"//@version=5\nindicator(\"Minimal\")\nplot(close)\n",
            )
            .is_empty()
    {
        return Err("boundary self-test rejected a clean local Pine fixture".into());
    }
    for (path, contents) in [
        ("examples/demo.pine", "indicator(\"Demo\")\n"),
        ("examples/demo.PINE", "indicator(\"Demo\")\n"),
        (
            "tests/regression/vendor.pine",
            "// https://example.com/copied-script\nindicator(\"Vendor\")\n",
        ),
        (
            "crates/pinewasm_migrate/tests/scripts/vendor.v5.pine",
            "// https://example.com/copied-script\nindicator(\"Vendor\")\n",
        ),
        (
            "crates/pinewasm_compiler/tests/demo.pine",
            "indicator(\"Demo\")\n",
        ),
    ] {
        if matchers
            .pine_fixture_failures(path, contents.as_bytes())
            .is_empty()
        {
            return Err(format!(
                "boundary self-test accepted an unsafe Pine fixture: {path}"
            ));
        }
    }
    Ok(())
}

fn extra_rules_self_test() -> TaskResult {
    let rules = r"# Synthetic rules for the self-test.

deny ^synthetic/[a-z]+\.txt$
marker SECRET-TOKEN-[0-9]+
pine-marker Synthetic-Origin
allow ^synthetic/allowed\.json$
";
    let temporary = temp_dir("boundary-extra-rules-self-test")?;
    let path = temporary.path().join("extra-patterns.txt");
    fs::write(&path, rules).map_err(|error| error.to_string())?;
    let extra = ExtraRules::load(&path)?;
    let matchers = Matchers::with_extra(&extra)?;
    let built_in = Matchers::built_in()?;
    let pine = "tests/compile-pass/synthetic.pine";

    if !matchers.denied("synthetic/notes.txt") || matchers.denied("synthetic/notes.json") {
        return Err("extra deny rule did not apply as a path rule".into());
    }
    if !matchers.public("synthetic/allowed.json") || matchers.public("synthetic/other.json") {
        return Err("extra allow rule did not extend the allowlist exactly".into());
    }
    if matchers.marker(b"token SECRET-TOKEN-42").is_none()
        || matchers.marker(b"token SECRET-TOKEN-x").is_some()
    {
        return Err("extra marker rule did not match its synthetic pattern".into());
    }
    if matchers
        .pine_fixture_failures(pine, b"// synthetic-origin\n")
        .is_empty()
        || !matchers
            .pine_fixture_failures(pine, b"// plain\n")
            .is_empty()
    {
        return Err("extra pine-marker rule did not match case-insensitively".into());
    }
    if built_in.denied("synthetic/notes.txt")
        || built_in.public("synthetic/allowed.json")
        || built_in.marker(b"token SECRET-TOKEN-42").is_some()
        || !built_in
            .pine_fixture_failures(pine, b"// synthetic-origin\n")
            .is_empty()
    {
        return Err("extra rules leaked into the built-in rules".into());
    }
    for (text, description) in [
        ("bogus value\n", "unknown rule"),
        ("deny\n", "missing value"),
    ] {
        if ExtraRules::parse(text, "self-test").is_ok() {
            return Err(format!("extra rules accepted a {description}"));
        }
    }
    if Matchers::with_extra(&ExtraRules::parse("marker (\n", "self-test")?).is_ok() {
        return Err("extra rules accepted an invalid regular expression".into());
    }
    if ExtraRules::load(&temporary.path().join("missing.txt")).is_ok() {
        return Err("extra rules accepted an unreadable file".into());
    }
    Ok(())
}

fn benchmark_size_failure(relative: &str, size: usize) -> Option<String> {
    if !relative.starts_with("benchmarks/") {
        return None;
    }
    let limit = if relative.ends_with(".json.gz") {
        16 * 1024 * 1024
    } else {
        64 * 1024
    };
    (size > limit).then(|| format!("benchmark file exceeds {limit} bytes: {relative}"))
}

fn scan_tree(root: &Path, staged: bool, matchers: &Matchers) -> TaskResult {
    let listing = if staged {
        git_bytes(root, &["ls-files", "-z", "--cached"])?
    } else {
        git_bytes(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?
    };
    let modes = index_modes(root)?;
    let mut files = Vec::new();
    for field in nul_fields(&listing) {
        let relative = std::str::from_utf8(field)
            .map_err(|error| format!("Git path is not UTF-8: {error}"))?
            .to_string();
        let full = root.join(&relative);
        let gitlink = modes.get(&relative).is_some_and(|mode| mode == "160000");
        if !staged && !gitlink && !full.exists() && !full.is_symlink() {
            continue;
        }
        files.push(relative);
    }
    let mut failures = Vec::new();
    for relative in &files {
        let mode = modes.get(relative).map(String::as_str);
        let symlink = if staged {
            mode == Some("120000")
        } else {
            root.join(relative).is_symlink()
        };
        if mode == Some("160000") {
            failures.push(format!("Git submodule entry is not allowed: {relative}"));
        } else if symlink {
            failures.push(format!("symbolic link is not allowed: {relative}"));
        } else if matchers.denied(relative) {
            failures.push(format!(
                "known-excluded or unsafe path is publishable: {relative}"
            ));
        } else if !matchers.public(relative) {
            failures.push(format!(
                "path is not on the publishable allowlist: {relative}"
            ));
        }
    }
    for relative in &files {
        if relative == SELF_PATH
            || modes
                .get(relative)
                .is_some_and(|mode| matches!(mode.as_str(), "120000" | "160000"))
        {
            continue;
        }
        let bytes = if staged {
            git_bytes(root, &["show", &format!(":{relative}")])?
        } else {
            fs::read(root.join(relative))
                .map_err(|error| format!("cannot read {relative}: {error}"))?
        };
        if let Some(error) = benchmark_size_failure(relative, bytes.len()) {
            failures.push(error);
        }
        if let Some(error) = matchers.benchmark_archive_failure(relative, &bytes) {
            failures.push(error);
        }
        if let Some(pattern) = matchers.marker(&bytes) {
            failures.push(format!(
                "marker {pattern:?} found in publishable file: {relative}"
            ));
        }
        failures.extend(matchers.pine_fixture_failures(relative, &bytes));
    }
    for required in REQUIRED_FILES {
        let present = if staged {
            output_allow_failure(Command::new("git").arg("-C").arg(root).args([
                "cat-file",
                "-e",
                &format!(":{required}"),
            ]))?
            .status
            .success()
        } else {
            root.join(required).is_file()
        };
        if !present {
            failures.push(format!("required file is missing: {required}"));
        }
    }
    let readme = if staged {
        git_bytes(root, &["show", ":README.md"])?
    } else {
        fs::read(root.join("README.md")).unwrap_or_default()
    };
    if !String::from_utf8_lossy(&readme).contains("not affiliated with") {
        failures.push("README.md is missing the 'not affiliated with' trademark statement".into());
    }
    verdict(
        failures,
        if staged {
            "Staged public-tree boundary guard: clean."
        } else {
            "Public-tree boundary guard: clean."
        },
    )
}

fn index_modes(root: &Path) -> TaskResult<BTreeMap<String, String>> {
    let listing = git_bytes(root, &["ls-files", "-s", "-z"])?;
    let mut modes = BTreeMap::new();
    for field in nul_fields(&listing) {
        let entry = std::str::from_utf8(field)
            .map_err(|error| format!("index entry is not UTF-8: {error}"))?;
        let (metadata, relative) = entry
            .split_once('\t')
            .ok_or_else(|| format!("malformed index entry: {entry:?}"))?;
        let mode = metadata.split_whitespace().next().unwrap_or_default();
        modes.insert(relative.to_string(), mode.to_string());
    }
    Ok(modes)
}

fn scan_range(root: &Path, base: &str, head: &str, matchers: &Matchers) -> TaskResult {
    scan_range_with_reporting(root, base, head, matchers, true)
}

fn scan_range_quiet(root: &Path, base: &str, head: &str, matchers: &Matchers) -> TaskResult {
    scan_range_with_reporting(root, base, head, matchers, false)
}

fn scan_range_with_reporting(
    root: &Path,
    base: &str,
    head: &str,
    matchers: &Matchers,
    report: bool,
) -> TaskResult {
    require_sha(base)?;
    require_sha(head)?;
    verify_object(root, head, "commit")?;
    let revision = if base.bytes().all(|byte| byte == b'0') {
        head.to_string()
    } else {
        verify_object(root, base, "commit")?;
        format!("{base}..{head}")
    };
    let commits = git(root, &["rev-list", "--reverse", "--topo-order", &revision])?
        .lines()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if commits.is_empty() {
        if report {
            println!(
                "Git-range public-tree boundary guard: no commits introduced; nothing to check."
            );
        }
        return Ok(());
    }
    let mut failures = Vec::new();
    // A PR often has many commits whose trees share most blobs. Keep the
    // path-sensitive inspection verdict by immutable blob id so every commit
    // is still checked without spawning `git cat-file` for unchanged files.
    let mut blob_cache: HashMap<String, HashMap<String, BlobInspection>> = HashMap::new();
    for commit in &commits {
        // Every commit in the range is scanned for denied paths, disallowed
        // content, and symbolic links. Required files are checked on the final
        // commit only: enforcing a requirement that HEAD introduces against
        // older commits would make the guard impossible to evolve.
        failures.extend(scan_commit(
            root,
            commit,
            matchers,
            commit.eq_ignore_ascii_case(head),
            &mut blob_cache,
        )?);
    }
    if failures.is_empty() {
        if report {
            println!(
                "Git-range public-tree boundary guard: {} immutable commit tree(s) clean.",
                commits.len()
            );
        }
        Ok(())
    } else if report {
        verdict(failures, "")
    } else {
        Err(format!(
            "public-tree boundary guard failed with {} error(s)",
            failures.len()
        ))
    }
}

struct BlobInspection {
    pre_marker_failures: Vec<String>,
    fixture_failures: Vec<String>,
    marker: Option<String>,
    readme_has_trademark: bool,
}

fn scan_commit(
    root: &Path,
    commit: &str,
    matchers: &Matchers,
    enforce_final_tree: bool,
    blob_cache: &mut HashMap<String, HashMap<String, BlobInspection>>,
) -> TaskResult<Vec<String>> {
    let listing = git_bytes(root, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut failures = Vec::new();
    let mut paths = BTreeSet::new();
    let mut readme_has_trademark = None;
    for field in nul_fields(&listing) {
        let entry = std::str::from_utf8(field)
            .map_err(|error| format!("tree entry is not UTF-8: {error}"))?;
        let (metadata, relative) = entry
            .split_once('\t')
            .ok_or_else(|| format!("malformed tree entry in {commit}"))?;
        let parts = metadata.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 3 {
            failures.push(format!(
                "malformed tree metadata in commit {commit}: {relative}"
            ));
            continue;
        }
        let (mode, kind, object) = (parts[0], parts[1], parts[2]);
        paths.insert(relative.to_string());
        if matchers.denied(relative) {
            failures.push(format!("denied path in commit {commit}: {relative}"));
        } else if !matchers.public(relative) {
            failures.push(format!(
                "path is not on the publishable allowlist in commit {commit}: {relative}"
            ));
        }
        match (mode, kind) {
            ("100644" | "100755", "blob") => {}
            ("120000", "blob") => {
                failures.push(format!("symbolic link in commit {commit}: {relative}"));
            }
            ("160000", "commit") => failures.push(format!(
                "Git submodule entry in commit {commit}: {relative}"
            )),
            _ => failures.push(format!(
                "unsupported Git tree mode/type {mode}:{kind} in commit {commit}: {relative}"
            )),
        }
        if kind == "blob" {
            if !blob_cache
                .get(object)
                .is_some_and(|paths| paths.contains_key(relative))
            {
                let bytes = git_bytes(root, &["cat-file", "blob", object])?;
                let mut pre_marker_failures = Vec::new();
                if let Some(error) = benchmark_size_failure(relative, bytes.len()) {
                    pre_marker_failures.push(error);
                }
                if let Some(error) = matchers.benchmark_archive_failure(relative, &bytes) {
                    pre_marker_failures.push(error);
                }
                let inspection = BlobInspection {
                    pre_marker_failures,
                    fixture_failures: matchers.pine_fixture_failures(relative, &bytes),
                    marker: (relative != SELF_PATH)
                        .then(|| matchers.marker(&bytes).map(str::to_owned))
                        .flatten(),
                    readme_has_trademark: relative == "README.md"
                        && String::from_utf8_lossy(&bytes).contains("not affiliated with"),
                };
                blob_cache
                    .entry(object.to_string())
                    .or_default()
                    .insert(relative.to_string(), inspection);
            }
            let inspection = &blob_cache[object][relative];
            if relative == "README.md" {
                readme_has_trademark = Some(inspection.readme_has_trademark);
            }
            failures.extend(
                inspection
                    .pre_marker_failures
                    .iter()
                    .map(|failure| format!("{failure} (commit {commit})")),
            );
            if let Some(pattern) = &inspection.marker {
                failures.push(format!("marker {pattern:?} in commit {commit}: {relative}"));
            }
            failures.extend(
                inspection
                    .fixture_failures
                    .iter()
                    .map(|failure| format!("{failure} (commit {commit})")),
            );
        }
    }
    if paths.is_empty() {
        failures.push(format!("commit {commit} has an empty tree"));
    }
    if enforce_final_tree {
        for required in REQUIRED_FILES {
            if !paths.contains(*required) {
                failures.push(format!(
                    "required file {required} is missing in head commit {commit}"
                ));
            }
        }
        if !readme_has_trademark.unwrap_or(false) {
            failures.push(format!(
                "README.md trademark statement is missing in head commit {commit}"
            ));
        }
    }
    Ok(failures)
}

fn history_self_test(matchers: &Matchers) -> TaskResult {
    let temporary = temp_dir("boundary-history-self-test")?;
    let root = temporary.path();
    git_run(root, &["init", "--quiet"])?;
    git_run(root, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    git_run(root, &["config", "user.name", "Boundary Test"])?;
    git_run(
        root,
        &["config", "user.email", "boundary-test@example.invalid"],
    )?;
    git_run(root, &["config", "commit.gpgsign", "false"])?;
    fs::create_dir_all(root.join(".git/no-hooks")).map_err(|error| error.to_string())?;
    git_run(root, &["config", "core.hooksPath", ".git/no-hooks"])?;
    fs::create_dir_all(root.join("crates/pinewasm_bindings")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("docs/spec")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("tests")).map_err(|error| error.to_string())?;
    for (relative, contents) in [
        ("LICENSE", "test license\n"),
        ("docs/licensing.md", "test commercial policy\n"),
        ("CONTRIBUTING.md", "test contributor policy\n"),
        ("docs/artifact-distribution.md", "test artifact policy\n"),
        ("crates/pinewasm_bindings/LICENSE", "test license\n"),
        ("NOTICE", "test notice\n"),
        ("THIRD_PARTY_LICENSES.txt", "test dependencies\n"),
        ("docs/trademarks.md", "test trademarks\n"),
        ("docs/rust-api.md", "# Rust API support and stability\n"),
        ("docs/README.md", "# PineWASM documentation\n"),
        ("docs/spec/LICENSE", "test spec license\n"),
        (
            "tests/README.md",
            "Only first-party local test fixtures belong here.\n",
        ),
        (
            "README.md",
            "This project is not affiliated with any third party.\n",
        ),
    ] {
        fs::write(root.join(relative), contents).map_err(|error| error.to_string())?;
    }
    git_run(root, &["add", "--", "."])?;
    git_run(root, &["commit", "--quiet", "-m", "initial tree"])?;
    let root_commit = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    scan_range(
        root,
        "0000000000000000000000000000000000000000",
        &root_commit,
        matchers,
    )?;

    fs::remove_file(root.join("crates/pinewasm_bindings/LICENSE"))
        .map_err(|error| error.to_string())?;
    git_run(root, &["add", "--", "crates/pinewasm_bindings/LICENSE"])?;
    git_run(root, &["commit", "--quiet", "-m", "remove required file"])?;
    let missing_required = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    // Git accepts hexadecimal object IDs case-insensitively; the head-contract
    // selector must do the same or an uppercase ID could bypass final checks.
    if scan_range_quiet(
        root,
        &root_commit,
        &missing_required.to_ascii_uppercase(),
        matchers,
    )
    .is_ok()
    {
        return Err("history-boundary self-test accepted a head missing a required file".into());
    }
    fs::write(
        root.join("crates/pinewasm_bindings/LICENSE"),
        "test license\n",
    )
    .map_err(|error| error.to_string())?;
    git_run(root, &["add", "--", "crates/pinewasm_bindings/LICENSE"])?;
    git_run(root, &["commit", "--quiet", "-m", "restore required file"])?;
    let restored_required = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    scan_range(root, &root_commit, &restored_required, matchers)?;

    fs::write(root.join("docs/browser.md"), "temporary REPLACE-ME note\n")
        .map_err(|error| error.to_string())?;
    git_run(root, &["add", "docs/browser.md"])?;
    git_run(root, &["commit", "--quiet", "-m", "add transient marker"])?;
    let marker_add = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(
        root,
        &[
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "retain transient marker",
        ],
    )?;
    let marker_retain = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let mut repeated_blob_cache = HashMap::new();
    for commit in [&marker_add, &marker_retain] {
        let failures = scan_commit(root, commit, matchers, false, &mut repeated_blob_cache)?;
        if !failures
            .iter()
            .any(|failure| failure.contains(&format!("in commit {commit}: docs/browser.md")))
        {
            return Err(format!(
                "history-boundary self-test missed the marker in repeated blob commit {commit}"
            ));
        }
    }
    git_run(root, &["rm", "--quiet", "--", "docs/browser.md"])?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "delete transient marker"],
    )?;
    let marker_delete = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if scan_range_quiet(root, &root_commit, &marker_delete, matchers).is_ok() {
        return Err(
            "history-boundary self-test accepted marker content added and deleted in one range"
                .into(),
        );
    }
    scan_range(root, &marker_retain, &marker_delete, matchers)?;

    fs::write(root.join(".mcp.json"), "{}\n").map_err(|error| error.to_string())?;
    git_run(root, &["add", "--", ".mcp.json"])?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "add transient denied path"],
    )?;
    let path_add = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(root, &["rm", "--quiet", "--", ".mcp.json"])?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "delete transient denied path"],
    )?;
    let path_delete = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if scan_range_quiet(root, &marker_delete, &path_delete, matchers).is_ok() {
        return Err(
            "history-boundary self-test accepted a denied path added and deleted in one range"
                .into(),
        );
    }
    scan_range(root, &path_add, &path_delete, matchers)?;

    #[cfg(unix)]
    std::os::unix::fs::symlink("../README.md", root.join("docs/runtime.md"))
        .map_err(|error| error.to_string())?;
    git_run(root, &["add", "--", "docs/runtime.md"])?;
    git_run(root, &["commit", "--quiet", "-m", "add transient symlink"])?;
    let symlink_add = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(root, &["rm", "--quiet", "--", "docs/runtime.md"])?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "delete transient symlink"],
    )?;
    let symlink_delete = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if scan_range_quiet(root, &path_delete, &symlink_delete, matchers).is_ok() {
        return Err(
            "history-boundary self-test accepted a symlink added and deleted in one range".into(),
        );
    }
    scan_range(root, &symlink_add, &symlink_delete, matchers)?;

    let cache_info = format!("160000,{root_commit},docs/runtime.md");
    git_run(root, &["update-index", "--add", "--cacheinfo", &cache_info])?;
    git_run(root, &["commit", "--quiet", "-m", "add transient gitlink"])?;
    let gitlink_add = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(
        root,
        &["update-index", "--force-remove", "--", "docs/runtime.md"],
    )?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "delete transient gitlink"],
    )?;
    let gitlink_delete = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if scan_range_quiet(root, &symlink_delete, &gitlink_delete, matchers).is_ok() {
        return Err(
            "history-boundary self-test accepted a gitlink added and deleted in one range".into(),
        );
    }
    scan_range(root, &gitlink_add, &gitlink_delete, matchers)?;

    println!(
        "History-boundary self-test: final-tree contract and transient marker, path, symlink, and gitlink cases passed."
    );
    Ok(())
}

fn verdict(failures: Vec<String>, success: &str) -> TaskResult {
    if failures.is_empty() {
        if !success.is_empty() {
            println!("{success}");
        }
        Ok(())
    } else {
        for failure in &failures {
            if env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                eprintln!("::error::{failure}");
            } else {
                eprintln!("ERROR: {failure}");
            }
        }
        Err(format!(
            "public-tree boundary guard failed with {} error(s)",
            failures.len()
        ))
    }
}

fn require_sha(value: &str) -> TaskResult {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("BASE_SHA and HEAD_SHA must be exact 40-character Git object ids".into())
    }
}

fn verify_object(root: &Path, sha: &str, expected: &str) -> TaskResult {
    let kind = git(root, &["cat-file", "-t", sha])?;
    if kind.trim() == expected {
        Ok(())
    } else {
        Err(format!("{sha} is not a {expected} object"))
    }
}

fn git_run(root: &Path, args: &[&str]) -> TaskResult {
    run(Command::new("git").arg("-C").arg(root).args(args))
}
