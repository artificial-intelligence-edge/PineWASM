use crate::command::{
    output, output_allow_failure, pinned_cargo, pinned_toolchain, reject_untrusted_cargo_sources,
    reject_untrusted_cargo_sources_at, remove_cargo_build_overrides, repo_root, run, temp_dir,
    TempDir,
};
use crate::{
    aot, artifact_notice, boundary, coverage, installers, licenses, package, policy,
    runtime_safety, secrets, workflow, TaskResult,
};
use quote::ToTokens;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::Token;
use syn::{Attribute, Expr, Fields, ImplItem, Item, Meta, UseTree, Visibility};

const MINIMAL_FEATURE_PACKAGES: [&str; 7] = [
    "pinewasm_mir",
    "pinewasm_broker",
    "pinewasm_ta",
    "pinewasm_tz",
    "pinewasm_lsp",
    "pinewasm_num",
    "pinewasm_bindings",
];
const MINIMAL_FEATURE_TESTS: [(&str, &[&str]); 7] = [
    ("pinewasm_mir", &["--lib"]),
    ("pinewasm_broker", &["--lib"]),
    ("pinewasm_ta", &[]),
    // Without `bundled`: the reader and fixed-offset and session parsers
    // emitted runtimes link.
    ("pinewasm_tz", &[]),
    ("pinewasm_lsp", &["--lib"]),
    ("pinewasm_num", &[]),
    ("pinewasm_bindings", &["--lib"]),
];
/// Cargo features that only compile extraction roots (`aeneas_roots*`
/// modules read by Charon/Aeneas) and enable nothing else. A package whose
/// features are all of this kind has no reduced build configuration, so it
/// needs no minimal-feature suite; the all-feature Clippy and test passes
/// still compile and lint its roots.
const EXTRACTION_ONLY_FEATURES: [&str; 1] = ["aeneas"];

const BINARYEN_VERSION: &str = "131";
const CARGO_DENY_VERSION: &str = "0.20.2";
const CRATES_IO_REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const GITLEAKS_VERSION: &str = "8.30.1";
const ADVERSARIAL_SMOKE_CASES: &str = "64";
const COMPILER_ADVERSARIAL_TARGET: &[&str] = &["--test", "adversarial"];
const AOT_ADVERSARIAL_TARGET: &[&str] = &["--test", "adversarial"];
const BINDINGS_ADVERSARIAL_TARGET: &[&str] = &["--lib"];
const AOT_DIFFERENTIAL_TARGET: &[&str] = &["--test", "differential"];
const MIGRATE_ADVERSARIAL_TARGET: &[&str] = &["--test", "adversarial"];

#[derive(Clone, Copy)]
struct AdversarialTest {
    package: &'static str,
    target_args: &'static [&'static str],
    name: &'static str,
}

const ADVERSARIAL_TESTS: [AdversarialTest; 7] = [
    AdversarialTest {
        package: "pinewasm_compiler",
        target_args: COMPILER_ADVERSARIAL_TARGET,
        name: "arbitrary_and_seed_mutated_source_compilation_is_total",
    },
    AdversarialTest {
        package: "pinewasm_compiler",
        target_args: COMPILER_ADVERSARIAL_TARGET,
        name: "csil_json_decode_validate_and_valid_round_trip_are_total",
    },
    AdversarialTest {
        package: "pinewasm_compiler",
        target_args: COMPILER_ADVERSARIAL_TARGET,
        name: "valid_and_mutated_sources_execute_under_explicit_vm_budgets",
    },
    AdversarialTest {
        package: "pinewasm_aot",
        target_args: AOT_ADVERSARIAL_TARGET,
        name: "arbitrary_aot_artifact_and_metadata_parsing_is_total",
    },
    AdversarialTest {
        package: "pinewasm_bindings",
        target_args: BINDINGS_ADVERSARIAL_TARGET,
        name: "tests::adversarial_binding_dto_deserialization_is_total",
    },
    AdversarialTest {
        package: "pinewasm_aot",
        target_args: AOT_DIFFERENTIAL_TARGET,
        name: "adversarial_supported_subset_vm_aot_parity",
    },
    AdversarialTest {
        package: "pinewasm_migrate",
        target_args: MIGRATE_ADVERSARIAL_TARGET,
        name: "arbitrary_and_seed_mutated_source_migration_is_total",
    },
];
/// Shared by CI selection so ignored campaign targets cannot disappear from a test-only plan.
pub(crate) fn owns_adversarial_target(package: &str, target: Option<&str>) -> bool {
    ADVERSARIAL_TESTS.iter().any(|test| {
        test.package == package
            && target.is_none_or(|target| test.target_args == ["--test", target])
    })
}

// Keep this list synchronized with the configuration and internal replay
// environment recognized by the Proptest version in Cargo.lock. The unit test
// below makes every removal and the forced PineWASM case count observable on
// the constructed command.
const PROPTEST_ENVIRONMENT: [&str; 14] = [
    // Proptest uses this private variable when replaying a forked child. An
    // inherited value would otherwise redirect or corrupt this process's run.
    "_PROPTEST_FORKFILE",
    "PROPTEST_CASES",
    "PROPTEST_DISABLE_FAILURE_PERSISTENCE",
    "PROPTEST_FORK",
    "PROPTEST_MAX_DEFAULT_SIZE_RANGE",
    "PROPTEST_MAX_FLAT_MAP_REGENS",
    "PROPTEST_MAX_GLOBAL_REJECTS",
    "PROPTEST_MAX_LOCAL_REJECTS",
    "PROPTEST_MAX_SHRINK_ITERS",
    "PROPTEST_MAX_SHRINK_TIME",
    "PROPTEST_RNG_ALGORITHM",
    "PROPTEST_RNG_SEED",
    "PROPTEST_TIMEOUT",
    "PROPTEST_VERBOSE",
];

pub(crate) fn setup(args: &[String]) -> TaskResult {
    no_args("setup", args)?;
    let root = repo_root()?;
    let toolchain = pinned_toolchain(&root)?;
    run(Command::new("rustup").current_dir(&root).args([
        "target",
        "add",
        "--toolchain",
        &toolchain,
        "wasm32-unknown-unknown",
    ]))?;
    wasm(&[])
}

pub(crate) fn clean(args: &[String]) -> TaskResult {
    no_args("clean", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.arg("clean"))
}

pub(crate) fn hooks(args: &[String]) -> TaskResult {
    no_args("hooks", args)?;
    boundary::install_hook()
}

pub(crate) fn build(args: &[String]) -> TaskResult {
    no_args("build", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args(["build", "--workspace", "--locked"]))
}

pub(crate) fn wasm(args: &[String]) -> TaskResult {
    let migrate = match args {
        [] => false,
        [flag] if flag == "--migrate" => true,
        _ => return Err("usage: wasm [--migrate]".into()),
    };
    let root = repo_root()?;
    let prefix = tool_prefix(&root, "binaryen", BINARYEN_VERSION);
    installers::install_binaryen(&[
        BINARYEN_VERSION.to_string(),
        utf8_path(&prefix)?.to_string(),
    ])?;
    package::build_wasm_package_with_path(&prefix.join("bin"), migrate)
}

/// Test the actual generated adapter, whose JavaScript reflection behavior
/// cannot be exercised by native DTO tests. Node is already pinned in the
/// package CI job; local `ci` and release validation require it as well.
pub(crate) fn wasm_test(args: &[String]) -> TaskResult {
    no_args("wasm-test", args)?;
    let root = repo_root()?;
    // Each runs Node.js against `target/wasm-pkg`: the library boundary, the
    // JavaScript types of the version surface, and a linked chart module,
    // which only a package whose compiler was built with the chart runtime
    // can emit, and the shared native/Wasm language query facade.
    for (target, test_name) in [
        (
            "library_boundary",
            "generated_library_boundary_snapshots_only_bounded_data_properties",
        ),
        (
            "version_surface",
            "generated_package_reports_one_rendering_contract_version",
        ),
        (
            "linked_chart_package",
            "generated_package_runs_a_linked_chart_module",
        ),
        (
            "language_query_package",
            "generated_package_language_queries_match_native_and_boundaries",
        ),
        (
            "table_identity_package",
            "generated_package_preserves_retired_table_identity",
        ),
    ] {
        let command = || -> TaskResult<Command> {
            let mut cargo = pinned_cargo(&root)?;
            cargo.args([
                "test",
                "--locked",
                "-p",
                "pinewasm_bindings",
                "--test",
                target,
                "--",
                "--ignored",
                "--exact",
                test_name,
            ]);
            Ok(cargo)
        };
        let listing = output(command()?.arg("--list"))?;
        require_exact_ignored_test(&listing.stdout, test_name)?;
        run(&mut command()?)?;
    }
    Ok(())
}

pub(crate) fn wasm_watch(args: &[String]) -> TaskResult {
    match args {
        [flag] if flag == "--once" => return wasm(&[]),
        [] => {}
        _ => return Err("usage: wasm-watch [--once]".into()),
    }
    let root = repo_root()?;
    let mut previous = source_snapshot(&root)?;
    println!("Watching Rust and Cargo source files; press Ctrl-C to stop.");
    loop {
        thread::sleep(Duration::from_millis(500));
        let current = source_snapshot(&root)?;
        if current != previous {
            println!("Source change detected; rebuilding the Wasm package.");
            if let Err(error) = wasm(&[]) {
                eprintln!("Wasm rebuild failed: {error}");
            }
            previous = current;
        }
    }
}

pub(crate) fn aot_runtime_all(args: &[String]) -> TaskResult {
    no_args("aot-runtime-all", args)?;
    aot::rebuild_all()
}

pub(crate) fn aot_census(args: &[String]) -> TaskResult {
    no_args("aot-census", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args([
        "test",
        "--locked",
        "-p",
        "pinewasm_aot",
        "--test",
        "census",
        "--test",
        "standalone_wasm_census",
        "--",
        "--nocapture",
    ]))
}

pub(crate) fn fuzz_smoke(args: &[String]) -> TaskResult {
    no_args("fuzz-smoke", args)?;
    require_unique_adversarial_tests(&ADVERSARIAL_TESTS)?;
    let root = repo_root()?;
    for test in ADVERSARIAL_TESTS {
        let mut discovery = pinned_cargo(&root)?;
        configure_adversarial_test_command(&mut discovery, test);
        discovery.args(["--", "--list", "--ignored", "--exact", test.name]);
        let discovered = output(&mut discovery)?;
        require_exact_ignored_test(&discovered.stdout, test.name)?;

        let mut command = pinned_cargo(&root)?;
        configure_adversarial_test_command(&mut command, test);
        command.args(["--", "--ignored", "--exact", test.name, "--test-threads=1"]);
        run(&mut command)?;
    }
    Ok(())
}

fn configure_adversarial_test_command(command: &mut Command, test: AdversarialTest) {
    configure_adversarial_command(command);
    command
        .args(["test", "--locked", "-p", test.package])
        .args(test.target_args);
}

fn require_unique_adversarial_tests(tests: &[AdversarialTest]) -> TaskResult {
    let mut identities = BTreeSet::new();
    for test in tests {
        let identity = (test.package, test.target_args, test.name);
        if !identities.insert(identity) {
            return Err(format!(
                "duplicate adversarial test identity for {}: `{}`",
                test.package, test.name
            ));
        }
    }
    Ok(())
}

fn require_exact_ignored_test(stdout: &[u8], expected: &str) -> TaskResult {
    let listing = std::str::from_utf8(stdout)
        .map_err(|error| format!("adversarial test listing is not UTF-8: {error}"))?;
    let discovered: Vec<_> = listing
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .collect();
    if discovered == [expected] {
        Ok(())
    } else {
        Err(format!(
            "expected exactly one ignored adversarial test `{expected}`, found [{}]",
            discovered.join(", ")
        ))
    }
}

fn configure_adversarial_command(command: &mut Command) {
    for variable in PROPTEST_ENVIRONMENT {
        command.env_remove(variable);
    }
    command.env("PINEWASM_ADVERSARIAL_CASES", ADVERSARIAL_SMOKE_CASES);
}

pub(crate) fn test(args: &[String]) -> TaskResult {
    no_args("test", args)?;
    let root = repo_root()?;
    let mut tests = pinned_cargo(&root)?;
    run(tests.args(["test", "--workspace", "--locked"]))?;

    let mut embedding = pinned_cargo(&root)?;
    run(embedding.args([
        "run",
        "--locked",
        "-p",
        "pinewasm_compiler",
        "--example",
        "embedding",
    ]))?;
    feature_test(&[])
}

/// Run the all-feature workspace suite and every supported feature-minimal
/// package suite from the same fail-closed inventory used by feature checking.
pub(crate) fn feature_test(args: &[String]) -> TaskResult {
    no_args("feature-test", args)?;
    let root = repo_root()?;
    require_minimal_feature_inventory(&root)?;
    let mut command = pinned_cargo(&root)?;
    run(command.args(["test", "--workspace", "--all-features", "--locked"]))?;
    for (package, target_args) in MINIMAL_FEATURE_TESTS {
        let mut command = pinned_cargo(&root)?;
        command.args(["test", "--package", package]);
        command.args(target_args);
        run(command.args(["--no-default-features", "--locked"]))?;
    }
    Ok(())
}

pub(crate) fn check(args: &[String]) -> TaskResult {
    no_args("check", args)?;
    crate::terminology::check(&[])?;
    public_boundary(&[])?;
    runtime_safety::check(&[])?;
    workflow_pin_check(&[])?;
    format_check(&[])?;
    lint(&[])?;
    docs_check(&[])?;
    feature_check(&[])?;
    coverage_check(&[])?;
    crate::benchmark_subject::check(&[])?;
    fuzz_smoke(&[])?;
    test(&[])?;
    crate::aot::chart_test(&[])
}

pub(crate) fn public_boundary(args: &[String]) -> TaskResult {
    no_args("public-boundary", args)?;
    boundary::check(&[])
}

pub(crate) fn workflow_pin_check(args: &[String]) -> TaskResult {
    no_args("workflow-pin-check", args)?;
    workflow::check()
}

pub(crate) fn leak_scan(args: &[String]) -> TaskResult {
    no_args("leak-scan", args)?;
    let root = repo_root()?;
    let prefix = tool_prefix(&root, "gitleaks", GITLEAKS_VERSION);
    installers::install_gitleaks(&[
        GITLEAKS_VERSION.to_string(),
        utf8_path(&prefix)?.to_string(),
    ])?;
    let binary = prefix.join("bin/gitleaks");
    secrets::check_with_program(&["--self-test".to_string()], &binary)?;
    secrets::check_with_program(&[], &binary)
}

pub(crate) fn format(args: &[String]) -> TaskResult {
    no_args("fmt", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args(["fmt", "--all"]))?;
    let mut runtime = pinned_cargo(&root)?;
    run(runtime.args([
        "fmt",
        "--manifest-path",
        "crates/pinewasm_aot/runtime/Cargo.toml",
        "--all",
    ]))
}

pub(crate) fn format_check(args: &[String]) -> TaskResult {
    no_args("fmt-check", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args(["fmt", "--all", "--check"]))?;
    let mut runtime = pinned_cargo(&root)?;
    run(runtime.args([
        "fmt",
        "--manifest-path",
        "crates/pinewasm_aot/runtime/Cargo.toml",
        "--all",
        "--check",
    ]))
}

pub(crate) fn lint(args: &[String]) -> TaskResult {
    no_args("lint", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]))?;
    let mut bindings_wasm = pinned_cargo(&root)?;
    run(bindings_wasm.args([
        "clippy",
        "--package",
        "pinewasm_bindings",
        "--lib",
        "--target",
        "wasm32-unknown-unknown",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]))?;
    let mut runtime = pinned_cargo(&root)?;
    run(runtime.args([
        "clippy",
        "--manifest-path",
        "crates/pinewasm_aot/runtime/Cargo.toml",
        "--lib",
        "--all-features",
        "--target",
        "wasm32-unknown-unknown",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]))
}

pub(crate) fn docs_check(args: &[String]) -> TaskResult {
    no_args("docs-check", args)?;
    let root = repo_root()?;
    let workspace_manifest = root.join("Cargo.toml");
    let runtime_manifest = root.join("crates/pinewasm_aot/runtime/Cargo.toml");
    let cargo = IsolatedCargo::new(&root)?;
    check_rust_api_inventory(&root, &cargo)?;
    let mut command = cargo.command(&root)?;
    run(command
        .env("RUSTDOCFLAGS", "-D warnings")
        .arg("doc")
        .arg("--manifest-path")
        .arg(&workspace_manifest)
        .args(["--workspace", "--all-features", "--no-deps", "--locked"]))?;
    check_supported_reexport_rustdoc(&root, &cargo)?;
    // `cargo doc` checks rendering and intra-doc links but does not execute
    // examples. Keep the supported facade and non-Rust contract wiring
    // examples explicit here rather than relying on the later workspace test
    // to happen to include doctests.
    let mut doctests = cargo.command(&root)?;
    run(doctests
        .arg("test")
        .arg("--manifest-path")
        .arg(&workspace_manifest)
        .args([
            "--locked",
            "--doc",
            "-p",
            "pinewasm_compiler",
            "-p",
            "pinewasm_bindings",
            "-p",
            "pinewasm_aot",
        ]))?;
    let mut runtime = cargo.command(&root)?;
    run(runtime
        .env("RUSTDOCFLAGS", "-D warnings")
        .arg("doc")
        .arg("--manifest-path")
        .arg(&runtime_manifest)
        .args([
            "--lib",
            "--all-features",
            "--target",
            "wasm32-unknown-unknown",
            "--no-deps",
            "--locked",
        ]))
}

/// Ephemeral Cargo state for one complete Rust API documentation audit.
///
/// Both the registry cache and target directory begin empty, so Cargo cannot
/// reuse mutable extracted registry sources or compiled proc-macro artifacts
/// from an earlier build. Cargo also runs from the filesystem root and
/// therefore cannot discover repository/ancestor configuration between the
/// preflight and process startup. The repository configuration is still
/// rejected conservatively, while the actual Cargo discovery root and fresh
/// home are rechecked immediately before every launch.
struct IsolatedCargo {
    _temporary: TempDir,
    cargo_home: PathBuf,
    target_directory: PathBuf,
    working_directory: PathBuf,
}

impl IsolatedCargo {
    fn new(root: &Path) -> TaskResult<Self> {
        reject_untrusted_cargo_sources(root)?;
        let temporary = temp_dir("rust-api-isolated")?;
        let canonical_root = root
            .canonicalize()
            .map_err(|error| format!("cannot canonicalize repository: {error}"))?;
        let canonical_temporary = temporary
            .path()
            .canonicalize()
            .map_err(|error| format!("cannot canonicalize isolated workspace: {error}"))?;
        if canonical_temporary.starts_with(&canonical_root) {
            return Err(format!(
                "Rust API isolated workspace {} must be outside the repository {}",
                canonical_temporary.display(),
                canonical_root.display()
            ));
        }
        let cargo_home = canonical_temporary.join("cargo-home");
        let target_directory = canonical_temporary.join("target");
        let working_directory = canonical_root
            .ancestors()
            .last()
            .ok_or_else(|| "cannot resolve the filesystem root for Cargo".to_string())?
            .to_path_buf();
        for directory in [&cargo_home, &target_directory] {
            fs::create_dir(directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        }
        reject_untrusted_cargo_sources_at(&working_directory, &cargo_home)?;
        Ok(Self {
            _temporary: temporary,
            cargo_home,
            target_directory,
            working_directory,
        })
    }

    fn command(&self, root: &Path) -> TaskResult<Command> {
        // Recheck the repository policy for a useful fail-closed diagnostic,
        // even though the isolated child does not discover its configuration.
        reject_untrusted_cargo_sources(root)?;
        reject_untrusted_cargo_sources_at(&self.working_directory, &self.cargo_home)?;
        let mut command = pinned_cargo(root)?;
        remove_cargo_build_overrides(&mut command);
        command
            .current_dir(&self.working_directory)
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTDOCFLAGS")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC_BOOTSTRAP")
            .env_remove("RUSTDOCFLAGS")
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TARGET_DIR", &self.target_directory);
        Ok(command)
    }
}

/// Keep the API classification exhaustive as the workspace
/// grows. This intentionally checks table membership, not prose or support
/// policy: deciding a new crate's classification remains a separate decision.
fn check_rust_api_inventory(root: &Path, cargo: &IsolatedCargo) -> TaskResult {
    let manifest = root.join("Cargo.toml");
    let mut metadata = cargo.command(root)?;
    let metadata = output(
        metadata
            .arg("metadata")
            .arg("--manifest-path")
            .arg(&manifest)
            .args(["--format-version", "1", "--no-deps", "--locked"]),
    )?;
    let workspace_crates = workspace_package_names_from_metadata(&metadata.stdout)?;
    let inventory = fs::read_to_string(root.join("docs/rust-api.md"))
        .map_err(|error| format!("cannot read docs/rust-api.md: {error}"))?;
    check_rust_api_inventory_text(&workspace_crates, &inventory)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupportedReexport {
    origin_crate: String,
    origin_modules: Vec<String>,
    origin_name: String,
    public_name: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FacadeRootPathHazards {
    bare_bindings: BTreeSet<String>,
    absolute_extern_aliases: BTreeSet<String>,
    has_glob: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceLibrary {
    package_id: String,
    source: PathBuf,
    trusted_macro_providers: BTreeSet<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CrateSyntaxContext {
    alias_names: BTreeSet<String>,
    trusted_macro_providers: BTreeSet<String>,
    trusted_bare_derives: BTreeSet<String>,
    shadowed_macro_names: BTreeSet<String>,
    shadowed_expression_macro_names: BTreeSet<String>,
    shadowed_provider_roots: BTreeSet<String>,
    shadowed_absolute_provider_roots: BTreeSet<String>,
    shadowed_helper_names: BTreeSet<String>,
    has_external_macro_glob: bool,
    has_nested_macro_glob: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DeriveHelpers {
    serde: bool,
    thiserror: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MemberDocDebt {
    variants: usize,
    fields: usize,
    methods: usize,
    associated_constants: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SourceMemberInventory {
    variants: usize,
    named_enum_fields: usize,
    tuple_enum_fields: usize,
    struct_fields: usize,
    methods: usize,
    associated_constants: usize,
    undocumented_named_enum_fields: usize,
    undocumented_tuple_enum_fields: usize,
    undocumented_struct_fields: usize,
    undocumented_variants: BTreeSet<String>,
    member_identities: BTreeSet<String>,
    debt: MemberDocDebt,
}

impl SourceMemberInventory {
    fn merge(&mut self, other: &Self) {
        self.variants += other.variants;
        self.named_enum_fields += other.named_enum_fields;
        self.tuple_enum_fields += other.tuple_enum_fields;
        self.struct_fields += other.struct_fields;
        self.methods += other.methods;
        self.associated_constants += other.associated_constants;
        self.undocumented_named_enum_fields += other.undocumented_named_enum_fields;
        self.undocumented_tuple_enum_fields += other.undocumented_tuple_enum_fields;
        self.undocumented_struct_fields += other.undocumented_struct_fields;
        self.undocumented_variants
            .extend(other.undocumented_variants.iter().cloned());
        self.member_identities
            .extend(other.member_identities.iter().cloned());
        self.debt.variants += other.debt.variants;
        self.debt.fields += other.debt.fields;
        self.debt.methods += other.debt.methods;
        self.debt.associated_constants += other.debt.associated_constants;
    }
}

/// Check the rendered facade rather than assuming that a successful rustdoc
/// process means re-exports kept their defining-crate documentation. A `///`
/// comment on a grouped `pub use` silently replaces every original item
/// description while satisfying `missing_docs`.
fn check_supported_reexport_rustdoc(root: &Path, cargo: &IsolatedCargo) -> TaskResult {
    let compiler_source = fs::read_to_string(root.join("crates/pinewasm_compiler/src/lib.rs"))
        .map_err(|error| format!("cannot read compiler facade source: {error}"))?;
    let reexports = supported_reexports_from_source(&compiler_source)?;
    let root_items = supported_facade_root_items_from_source(&compiler_source)?;
    let inventory = fs::read_to_string(root.join("docs/rust-api.md"))
        .map_err(|error| format!("cannot read docs/rust-api.md: {error}"))?;
    check_identity_manifest(
        &inventory,
        "rust-api-supported-root-items",
        &root_items,
        "supported compiler-root item",
    )?;

    let manifest = root.join("Cargo.toml");
    let mut metadata = cargo.command(root)?;
    let metadata = output(
        metadata
            .arg("metadata")
            .arg("--manifest-path")
            .arg(&manifest)
            .args(["--format-version", "1", "--all-features", "--locked"]),
    )?;
    let doc_root = target_directory_from_metadata(&metadata.stdout)?.join("doc");
    let libraries = workspace_libraries_from_metadata(&metadata.stdout)?;
    let metadata_value: serde_json::Value = serde_json::from_slice(&metadata.stdout)
        .map_err(|error| format!("cannot parse cargo metadata: {error}"))?;
    let resolved_nodes = metadata_value
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its resolved dependency nodes".to_string())?;
    let facade_root = doc_root.join("pinewasm_compiler");
    let compiler_library = libraries
        .get("pinewasm_compiler")
        .ok_or_else(|| "cargo metadata omitted the compiler facade library".to_string())?;
    let compiler_external_inherent = crate_inherent_member_inventories_with_providers(
        &compiler_library.source,
        &compiler_library.source,
        &compiler_library.trusted_macro_providers,
    )?;

    let mut debt = MemberDocDebt::default();
    let mut undocumented_variants = BTreeSet::new();
    let mut supported_members = BTreeSet::new();
    for reexport in &reexports {
        check_supported_reexport_dependency(
            resolved_nodes,
            compiler_library,
            &libraries,
            reexport,
        )?;
        let source_inventory = source_member_inventory(root, &libraries, reexport)?;
        undocumented_variants.extend(
            source_inventory
                .undocumented_variants
                .iter()
                .map(|variant| format!("{}::{variant}", reexport.public_name)),
        );
        supported_members.extend(
            source_inventory
                .member_identities
                .iter()
                .map(|member| qualify_member_identity(&reexport.public_name, member)),
        );
        let facade_page = rustdoc_page_for_name(&facade_root, &reexport.public_name)?;
        let facade_html = fs::read_to_string(&facade_page)
            .map_err(|error| format!("cannot read {}: {error}", facade_page.display()))?;

        let origin_page = rustdoc_page_for_reexport(&doc_root, reexport)?;
        let origin_html = fs::read_to_string(&origin_page)
            .map_err(|error| format!("cannot read {}: {error}", origin_page.display()))?;
        let facade_description = rustdoc_meta_description(&facade_html)
            .filter(|description| !description.trim().is_empty())
            .ok_or_else(|| format!("{} has no rendered description", facade_page.display()))?;
        let origin_description = rustdoc_meta_description(&origin_html)
            .filter(|description| !description.trim().is_empty())
            .ok_or_else(|| format!("{} has no rendered description", origin_page.display()))?;
        check_originating_description(
            &reexport.public_name,
            facade_description,
            &BTreeSet::from([origin_description.to_string()]),
        )?;
        let facade_top_doc =
            normalize_rustdoc_docblock(rustdoc_top_doc(&facade_html).ok_or_else(|| {
                format!("{} has no rendered item docblock", facade_page.display())
            })?);
        let origin_top_doc =
            normalize_rustdoc_docblock(rustdoc_top_doc(&origin_html).ok_or_else(|| {
                format!("{} has no rendered item docblock", origin_page.display())
            })?);
        check_originating_contract(
            &reexport.public_name,
            "item docblock",
            &facade_top_doc,
            &BTreeSet::from([origin_top_doc]),
        )?;

        let page_debt = member_doc_debt(&facade_html, source_inventory)
            .map_err(|error| format!("pinewasm_compiler::{}: {error}", reexport.public_name))?;
        debt.variants += page_debt.variants;
        debt.fields += page_debt.fields;
        debt.methods += page_debt.methods;
        debt.associated_constants += page_debt.associated_constants;
    }

    // Direct facade DTOs are just as public as the re-exported types. Keep
    // their fields and inherent implementations in the same source-to-rustdoc
    // reconciliation so a hidden method cannot bypass the root-item manifest.
    for (public_name, source_inventory) in
        direct_facade_member_inventories_from_source(&compiler_source, &compiler_external_inherent)?
    {
        undocumented_variants.extend(
            source_inventory
                .undocumented_variants
                .iter()
                .map(|variant| format!("{public_name}::{variant}")),
        );
        supported_members.extend(
            source_inventory
                .member_identities
                .iter()
                .map(|member| qualify_member_identity(&public_name, member)),
        );
        let facade_page = rustdoc_page_for_name(&facade_root, &public_name)?;
        let facade_html = fs::read_to_string(&facade_page)
            .map_err(|error| format!("cannot read {}: {error}", facade_page.display()))?;
        let page_debt = member_doc_debt(&facade_html, source_inventory)
            .map_err(|error| format!("pinewasm_compiler::{public_name}: {error}"))?;
        debt.variants += page_debt.variants;
        debt.fields += page_debt.fields;
        debt.methods += page_debt.methods;
        debt.associated_constants += page_debt.associated_constants;
    }

    check_member_doc_debt_marker(&inventory, debt)?;
    check_identity_manifest(
        &inventory,
        "rust-api-supported-members",
        &supported_members,
        "supported facade member",
    )?;
    check_identity_manifest(
        &inventory,
        "rust-api-undocumented-variants",
        &undocumented_variants,
        "undocumented facade variant",
    )
}

fn qualify_member_identity(public_name: &str, member: &str) -> String {
    let (kind, name) = member
        .split_once(' ')
        .expect("source member identities always carry a kind");
    format!("{kind} {public_name}::{name}")
}

fn target_directory_from_metadata(metadata: &[u8]) -> TaskResult<PathBuf> {
    let metadata: serde_json::Value = serde_json::from_slice(metadata)
        .map_err(|error| format!("cannot parse cargo metadata: {error}"))?;
    let target = metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "cargo metadata omitted its target_directory".to_string())?;
    Ok(PathBuf::from(target))
}

fn workspace_libraries_from_metadata(
    metadata: &[u8],
) -> TaskResult<BTreeMap<String, WorkspaceLibrary>> {
    let metadata: serde_json::Value = serde_json::from_slice(metadata)
        .map_err(|error| format!("cannot parse cargo metadata: {error}"))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its packages array".to_string())?;
    let members = metadata
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its workspace_members array".to_string())?
        .iter()
        .map(|member| {
            member.as_str().map(str::to_string).ok_or_else(|| {
                "cargo metadata contains a non-string workspace member id".to_string()
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;

    let mut sources = BTreeMap::new();
    for package in packages {
        let id = package
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "cargo metadata contains a package without a string id".to_string())?;
        if !members.contains(id) {
            continue;
        }
        let targets = package
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("cargo metadata package `{id}` has no targets array"))?;
        for target in targets {
            let is_library = target
                .get("kind")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("lib")));
            if !is_library {
                continue;
            }
            let name = target
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("library target in `{id}` has no name"))?;
            let source = target
                .get("src_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("library target `{name}` has no src_path"))?;
            let trusted_macro_providers =
                trusted_macro_providers_from_metadata(&metadata, package, id)?;
            if sources
                .insert(
                    name.to_string(),
                    WorkspaceLibrary {
                        package_id: id.to_string(),
                        source: PathBuf::from(source),
                        trusted_macro_providers,
                    },
                )
                .is_some()
            {
                return Err(format!("cargo metadata repeats library target `{name}`"));
            }
        }
    }
    Ok(sources)
}

/// Prove that the crate name used by a supported facade re-export resolves to
/// the same workspace package whose source and Rustdoc are inventoried below.
/// Cargo dependency renames deliberately decouple those names; comparing only
/// library target names would let rustc link an alternate package while the
/// release gate inspected the unrelated first-party crate.
fn check_supported_reexport_dependency(
    nodes: &[serde_json::Value],
    compiler_library: &WorkspaceLibrary,
    libraries: &BTreeMap<String, WorkspaceLibrary>,
    reexport: &SupportedReexport,
) -> TaskResult {
    let origin = libraries.get(&reexport.origin_crate).ok_or_else(|| {
        format!(
            "supported re-export `{}` names unknown workspace library `{}`",
            reexport.public_name, reexport.origin_crate
        )
    })?;
    let resolved =
        resolved_normal_dependency_id(nodes, &compiler_library.package_id, &reexport.origin_crate)?;
    if resolved != origin.package_id {
        return Err(format!(
            "compiler dependency `{}` for supported re-export `{}` resolves package `{resolved}`, but the allowlisted workspace library is `{}`",
            reexport.origin_crate, reexport.public_name, origin.package_id
        ));
    }
    Ok(())
}

fn declared_macro_providers_from_package(
    package: &serde_json::Value,
    package_id: &str,
) -> TaskResult<BTreeSet<String>> {
    let dependencies = package
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            format!("cargo metadata package `{package_id}` has no dependencies array")
        })?;
    let mut providers = BTreeSet::new();
    for dependency in dependencies {
        let Some(name) = dependency.get("name").and_then(serde_json::Value::as_str) else {
            return Err(format!(
                "cargo metadata package `{package_id}` has a dependency without a name"
            ));
        };
        if !matches!(name, "serde" | "thiserror") {
            continue;
        }
        let unrenamed = dependency
            .get("rename")
            .is_none_or(serde_json::Value::is_null);
        let source = dependency.get("source").and_then(serde_json::Value::as_str);
        let normal_dependency = dependency
            .get("kind")
            .is_none_or(serde_json::Value::is_null);
        let has_declared_optionality = dependency
            .get("optional")
            .and_then(serde_json::Value::as_bool)
            .is_some();
        let serde_derive_enabled = name != "serde"
            || dependency
                .get("features")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|features| {
                    features
                        .iter()
                        .any(|feature| feature.as_str() == Some("derive"))
                });
        if unrenamed
            && source == Some(CRATES_IO_REGISTRY_SOURCE)
            && normal_dependency
            && has_declared_optionality
            && serde_derive_enabled
        {
            providers.insert(name.to_string());
        }
    }
    Ok(providers)
}

fn resolved_normal_dependency_id<'a>(
    nodes: &'a [serde_json::Value],
    package_id: &str,
    dependency_name: &str,
) -> TaskResult<&'a str> {
    let package_nodes = nodes
        .iter()
        .filter(|node| node.get("id").and_then(serde_json::Value::as_str) == Some(package_id))
        .collect::<Vec<_>>();
    let [package_node] = package_nodes.as_slice() else {
        return Err(format!(
            "cargo metadata must resolve package `{package_id}` exactly once"
        ));
    };
    let dependencies = package_node
        .get("deps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("resolved package `{package_id}` has no dependency array"))?;
    let resolved = dependencies
        .iter()
        .filter(|dependency| {
            dependency.get("name").and_then(serde_json::Value::as_str) == Some(dependency_name)
                && dependency
                    .get("dep_kinds")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|kinds| {
                        kinds
                            .iter()
                            .any(|kind| kind.get("kind").is_none_or(serde_json::Value::is_null))
                    })
        })
        .collect::<Vec<_>>();
    let [resolved] = resolved.as_slice() else {
        return Err(format!(
            "cargo metadata must resolve normal dependency `{dependency_name}` for `{package_id}` exactly once"
        ));
    };
    resolved
        .get("pkg")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!("resolved dependency `{dependency_name}` for `{package_id}` has no package id")
        })
}

fn metadata_package_by_id<'a>(
    packages: &'a [serde_json::Value],
    package_id: &str,
) -> TaskResult<&'a serde_json::Value> {
    let matches = packages
        .iter()
        .filter(|candidate| {
            candidate.get("id").and_then(serde_json::Value::as_str) == Some(package_id)
        })
        .collect::<Vec<_>>();
    let [package] = matches.as_slice() else {
        return Err(format!(
            "cargo metadata must describe resolved dependency `{package_id}` exactly once"
        ));
    };
    Ok(package)
}

fn is_exact_crates_io_package(package: &serde_json::Value, expected_name: &str) -> bool {
    package.get("name").and_then(serde_json::Value::as_str) == Some(expected_name)
        && package.get("source").and_then(serde_json::Value::as_str)
            == Some(CRATES_IO_REGISTRY_SOURCE)
}

fn is_proc_macro_package(package: &serde_json::Value) -> bool {
    package
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|targets| {
            targets
                .iter()
                .filter(|target| {
                    target
                        .get("kind")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|kinds| {
                            kinds.iter().any(|kind| kind.as_str() == Some("proc-macro"))
                        })
                })
                .count()
                == 1
        })
}

fn has_exact_resolved_proc_macro(
    nodes: &[serde_json::Value],
    packages: &[serde_json::Value],
    provider_id: &str,
    dependency_name: &str,
    package_name: &str,
) -> TaskResult<bool> {
    let implementation_id = resolved_normal_dependency_id(nodes, provider_id, dependency_name)?;
    let implementation = metadata_package_by_id(packages, implementation_id)?;
    Ok(is_exact_crates_io_package(implementation, package_name)
        && is_proc_macro_package(implementation))
}

fn trusted_macro_providers_from_metadata(
    metadata: &serde_json::Value,
    package: &serde_json::Value,
    package_id: &str,
) -> TaskResult<BTreeSet<String>> {
    let declared = declared_macro_providers_from_package(package, package_id)?;
    if declared.is_empty() {
        return Ok(BTreeSet::new());
    }
    let nodes = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its resolved dependency nodes".to_string())?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its packages array".to_string())?;

    let mut trusted = BTreeSet::new();
    for provider in declared {
        let resolved_id = resolved_normal_dependency_id(nodes, package_id, &provider)?;
        let provider_package = metadata_package_by_id(packages, resolved_id)?;
        let (implementation_dependency, implementation_package) = match provider.as_str() {
            "serde" => ("serde_derive", "serde_derive"),
            "thiserror" => ("thiserror_impl", "thiserror-impl"),
            _ => unreachable!("declared provider allowlist is exhaustive"),
        };
        if is_exact_crates_io_package(provider_package, &provider)
            && has_exact_resolved_proc_macro(
                nodes,
                packages,
                resolved_id,
                implementation_dependency,
                implementation_package,
            )?
        {
            trusted.insert(provider);
        }
    }
    Ok(trusted)
}

fn rustdoc_page_for_reexport(doc_root: &Path, reexport: &SupportedReexport) -> TaskResult<PathBuf> {
    let mut origin_root = doc_root.join(&reexport.origin_crate);
    for module in &reexport.origin_modules {
        origin_root.push(module);
    }
    rustdoc_page_for_name(&origin_root, &reexport.origin_name)
}

fn source_member_inventory(
    root: &Path,
    libraries: &BTreeMap<String, WorkspaceLibrary>,
    reexport: &SupportedReexport,
) -> TaskResult<SourceMemberInventory> {
    let library = libraries.get(&reexport.origin_crate).ok_or_else(|| {
        format!(
            "supported re-export `{}` names unknown workspace library `{}`",
            reexport.public_name, reexport.origin_crate
        )
    })?;
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize repository root {}: {error}",
            root.display()
        )
    })?;
    let library_source = library.source.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize supported re-export source {}: {error}",
            library.source.display()
        )
    })?;
    library_source.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "supported re-export source escapes the repository: {}",
            library_source.display()
        )
    })?;
    let mut source_path = library_source.clone();
    let mut module_dir = source_path
        .parent()
        .ok_or_else(|| format!("library source has no parent: {}", source_path.display()))?
        .to_path_buf();

    for module_name in &reexport.origin_modules {
        let file = parse_rust_source(&source_path)?;
        let declarations = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(module) if ident_unraw_string(&module.ident) == module_name.as_str() => {
                    Some(module)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [declaration] = declarations.as_slice() else {
            return Err(format!(
                "origin module `{module_name}` must be declared exactly once in {}",
                source_path.display()
            ));
        };
        if !is_public(&declaration.vis) {
            return Err(format!(
                "origin module `{module_name}` is not public in {}",
                source_path.display()
            ));
        }
        if declaration.content.is_some()
            || declaration
                .attrs
                .iter()
                .any(|attribute| rust_path_is_ident(attribute.path(), "path"))
        {
            return Err(format!(
                "inline or path-overridden origin module `{module_name}` needs an explicit inventory parser update"
            ));
        }
        let flat = module_dir.join(format!("{module_name}.rs"));
        let nested = module_dir.join(module_name).join("mod.rs");
        source_path = match (flat.is_file(), nested.is_file()) {
            (true, false) => {
                module_dir = module_dir.join(module_name);
                flat
            }
            (false, true) => {
                module_dir = nested
                    .parent()
                    .expect("nested module path has a parent")
                    .to_path_buf();
                nested
            }
            _ => {
                return Err(format!(
                    "origin module `{module_name}` must resolve to exactly one source file"
                ));
            }
        };
    }

    let file = parse_rust_source(&source_path)?;
    let definitions = file
        .items
        .iter()
        .filter(|item| {
            item_definition_ident(item)
                .is_some_and(|ident| ident_unraw_string(ident) == reexport.origin_name.as_str())
        })
        .collect::<Vec<_>>();
    let [definition] = definitions.as_slice() else {
        return Err(format!(
            "supported re-export `{}` must name exactly one item defined directly in {} (intermediate re-exports are not accepted)",
            reexport.public_name,
            source_path.display()
        ));
    };
    if item_attributes(definition).is_some_and(doc_hidden) {
        return Err(format!(
            "supported re-export `{}` resolves to a doc-hidden definition",
            reexport.public_name
        ));
    }
    let mut inventory = inventory_source_members(definition, &file.items, &reexport.origin_name)?;
    let external_inherent = crate_inherent_member_inventories_with_providers(
        &library_source,
        &source_path,
        &library.trusted_macro_providers,
    )?;
    if let Some(additional) = external_inherent.get(&reexport.origin_name) {
        inventory.merge(additional);
    }
    Ok(inventory)
}

/// Inventory inherent implementations and reject hidden/autoderef trait-surface escapes in
/// every module of a library crate except the defining source file, whose
/// top-level items are already handled by [`inventory_source_members`]. Private
/// modules matter here: an inherent public method, a trait method hidden from
/// Rustdoc, or a `Deref` target method remains callable through a re-exported
/// type even when its `impl` lives behind a private module path.
#[cfg(test)]
fn crate_inherent_member_inventories(
    library_source: &Path,
    defining_source: &Path,
) -> TaskResult<BTreeMap<String, SourceMemberInventory>> {
    crate_inherent_member_inventories_with_providers(
        library_source,
        defining_source,
        &BTreeSet::new(),
    )
}

fn crate_inherent_member_inventories_with_providers(
    library_source: &Path,
    defining_source: &Path,
    trusted_macro_providers: &BTreeSet<String>,
) -> TaskResult<BTreeMap<String, SourceMemberInventory>> {
    let module_dir = library_source
        .parent()
        .ok_or_else(|| format!("library source has no parent: {}", library_source.display()))?;
    let syntax = crate_syntax_context(library_source, module_dir, trusted_macro_providers)?;
    let mut inventories = BTreeMap::new();
    let mut visited = BTreeSet::new();
    scan_crate_module_file(
        library_source,
        module_dir,
        defining_source,
        &syntax,
        &mut visited,
        &mut inventories,
    )?;
    Ok(inventories)
}

/// Collect every module-scope type alias, renamed import, and macro-name
/// origin hazard in the crate.
///
/// Rust aliases are transparent: an inherent implementation written against
/// one changes the underlying nominal type. Resolving every lexical import,
/// glob, re-export, and `cfg` branch would require rustc name resolution rather
/// than a syntax walk. Rejecting any impl self-name that collides with one of
/// these aliases is deliberately conservative and keeps the public API census
/// fail-closed across module boundaries.
fn crate_syntax_context(
    library_source: &Path,
    module_dir: &Path,
    trusted_macro_providers: &BTreeSet<String>,
) -> TaskResult<CrateSyntaxContext> {
    let mut context = CrateSyntaxContext {
        trusted_macro_providers: trusted_macro_providers.clone(),
        ..CrateSyntaxContext::default()
    };
    let mut visited = BTreeSet::new();
    collect_crate_syntax_context_file(
        library_source,
        module_dir,
        true,
        &mut visited,
        &mut context,
    )?;
    Ok(context)
}

fn collect_crate_syntax_context_file(
    source_path: &Path,
    module_dir: &Path,
    is_crate_root: bool,
    visited: &mut BTreeSet<PathBuf>,
    context: &mut CrateSyntaxContext,
) -> TaskResult {
    if !visited.insert(
        source_path
            .canonicalize()
            .map_err(|error| format!("cannot canonicalize crate module source: {error}"))?,
    ) {
        return Err(format!(
            "crate module source is reachable more than once: {}",
            source_path.display()
        ));
    }
    let file = parse_rust_source(source_path)?;
    collect_crate_syntax_context_items(
        &file.items,
        source_path,
        module_dir,
        is_crate_root,
        true,
        visited,
        context,
    )
}

fn collect_crate_syntax_context_items(
    items: &[Item],
    source_path: &Path,
    module_dir: &Path,
    is_crate_root: bool,
    file_scope: bool,
    visited: &mut BTreeSet<PathBuf>,
    context: &mut CrateSyntaxContext,
) -> TaskResult {
    context.alias_names.extend(module_alias_names(items));
    collect_module_macro_origins(items, context, is_crate_root);
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        if cfg_test_only(&module.attrs) {
            continue;
        }
        let explicit_path = checked_module_path(module, source_path, file_scope)?;
        let module_name = ident_unraw_string(&module.ident);
        let child_module_dir = module_dir.join(&module_name);
        if let Some((_, child_items)) = &module.content {
            collect_crate_syntax_context_items(
                child_items,
                source_path,
                &child_module_dir,
                false,
                false,
                visited,
                context,
            )?;
            continue;
        }

        let (child_source, child_module_dir) =
            resolve_crate_module(module, source_path, module_dir, explicit_path)?;
        collect_crate_syntax_context_file(
            &child_source,
            &child_module_dir,
            false,
            visited,
            context,
        )?;
    }
    Ok(())
}

/// Admit a single unconditional literal path on a file-scope, out-of-line
/// module. Inline and conditional overrides remain unsupported rather than
/// approximating rustc's context-dependent path rules.
fn checked_module_path(
    module: &syn::ItemMod,
    source_path: &Path,
    file_scope: bool,
) -> TaskResult<Option<PathBuf>> {
    fn conditional_path(meta: &Meta) -> bool {
        let Meta::List(list) = meta else {
            return false;
        };
        if !rust_path_is_ident(&list.path, "cfg_attr") {
            return false;
        }
        list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .map_or(true, |nested| {
                nested.iter().skip(1).any(|attribute| {
                    rust_path_is_ident(attribute.path(), "path") || conditional_path(attribute)
                })
            })
    }
    if module
        .attrs
        .iter()
        .any(|attribute| conditional_path(&attribute.meta))
    {
        return Err(format!(
            "conditional module path in {} needs explicit inventory support",
            source_path.display()
        ));
    }
    let attributes = module
        .attrs
        .iter()
        .filter(|attribute| rust_path_is_ident(attribute.path(), "path"))
        .collect::<Vec<_>>();
    if attributes.is_empty() {
        return Ok(None);
    }
    let [attribute] = attributes.as_slice() else {
        return Err("module must have exactly one literal path attribute".to_string());
    };
    if !file_scope || module.content.is_some() {
        return Err("inline module path overrides need explicit inventory support".to_string());
    }
    let Meta::NameValue(value) = &attribute.meta else {
        return Err("module path must be a string literal".to_string());
    };
    let Expr::Lit(value) = &value.value else {
        return Err("module path must be a string literal".to_string());
    };
    let syn::Lit::Str(value) = &value.lit else {
        return Err("module path must be a string literal".to_string());
    };
    let value = value.value();
    if value.contains(['\\', ':']) || value.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(
            "module path must contain only portable relative normal components".to_string(),
        );
    }
    let base = source_path
        .parent()
        .ok_or_else(|| "module source has no parent".to_string())?;
    let mut resolved = base.to_path_buf();
    for component in Path::new(&value).components() {
        let std::path::Component::Normal(component) = component else {
            return Err("module path must be relative".to_string());
        };
        resolved.push(component);
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
            format!(
                "cannot resolve explicit module path {}: {error}",
                resolved.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "explicit module path must not traverse symlinks: {}",
                resolved.display()
            ));
        }
    }
    if !resolved.is_file() {
        return Err(format!(
            "explicit module path is not a file: {}",
            resolved.display()
        ));
    }
    Ok(Some(resolved.canonicalize().map_err(|error| {
        format!("cannot canonicalize explicit module path: {error}")
    })?))
}

fn resolve_crate_module(
    module: &syn::ItemMod,
    source_path: &Path,
    module_dir: &Path,
    explicit_path: Option<PathBuf>,
) -> TaskResult<(PathBuf, PathBuf)> {
    if let Some(path) = explicit_path {
        // rustc treats an explicitly included file as mod.rs: its children
        // are siblings, irrespective of the file stem or logical module name.
        let directory = path
            .parent()
            .expect("resolved module file has a parent")
            .to_path_buf();
        return Ok((path, directory));
    }
    let name = ident_unraw_string(&module.ident);
    let directory = module_dir.join(&name);
    let flat = module_dir.join(format!("{name}.rs"));
    let nested = directory.join("mod.rs");
    let source = match (flat.is_file(), nested.is_file()) {
        (true, false) => flat,
        (false, true) => nested,
        _ => {
            return Err(format!(
                "crate module `{}` in {} must resolve to exactly one source file",
                module.ident,
                source_path.display()
            ))
        }
    };
    Ok((source, directory))
}

fn collect_module_macro_origins(
    items: &[Item],
    context: &mut CrateSyntaxContext,
    is_crate_root: bool,
) {
    // Record whether any module has an external glob. A module with its own
    // external glob is ambiguous; so is a local glob elsewhere in such a
    // crate, because it can relay that external binding. Modules with neither
    // remain usable without pretending to perform full name resolution.
    context.has_external_macro_glob |= module_has_unresolved_macro_glob(items);
    for item in items {
        match item {
            Item::Mod(module)
                if matches!(
                    ident_unraw_string(&module.ident).as_str(),
                    "serde" | "thiserror"
                ) =>
            {
                context
                    .shadowed_provider_roots
                    .insert(ident_unraw_string(&module.ident));
            }
            Item::ExternCrate(extern_crate) => {
                let source = ident_unraw_string(&extern_crate.ident);
                let local = extern_crate
                    .rename
                    .as_ref()
                    .map_or_else(|| source.clone(), |(_, rename)| ident_unraw_string(rename));
                if is_crate_root
                    && source != local
                    && matches!(local.as_str(), "serde" | "thiserror")
                {
                    context
                        .shadowed_absolute_provider_roots
                        .insert(local.clone());
                }
                record_macro_import(&[source], &local, context);
            }
            Item::Use(item_use) => {
                let mut bindings = Vec::new();
                flatten_use_tree(&item_use.tree, &mut Vec::new(), &mut bindings);
                for (source, local) in bindings
                    .into_iter()
                    .filter_map(|(source, local)| local.map(|local| (source, local)))
                {
                    record_macro_import(&source, &local, context);
                }
            }
            Item::Macro(item) => record_local_macro_item(item, context),
            _ => collect_nested_macro_origins(item, context),
        }
    }
}

fn record_local_macro_item(item: &syn::ItemMacro, context: &mut CrateSyntaxContext) {
    let Some(name) = item
        .ident
        .as_ref()
        .or_else(|| item.mac.path.segments.last().map(|segment| &segment.ident))
    else {
        return;
    };
    let name = ident_unraw_string(name);
    if is_allowlisted_expression_macro_name(&name) {
        context.shadowed_expression_macro_names.insert(name);
    }
}

fn collect_nested_macro_origins(item: &Item, context: &mut CrateSyntaxContext) {
    struct NestedImportVisitor<'scope> {
        context: &'scope mut CrateSyntaxContext,
    }

    impl<'ast> Visit<'ast> for NestedImportVisitor<'_> {
        fn visit_item_mod(&mut self, _module: &'ast syn::ItemMod) {
            // Module-scope imports need the local-root classification above;
            // the crate walk visits every non-test module separately.
        }

        fn visit_item_extern_crate(&mut self, extern_crate: &'ast syn::ItemExternCrate) {
            let source = ident_unraw_string(&extern_crate.ident);
            let local = extern_crate
                .rename
                .as_ref()
                .map_or_else(|| source.clone(), |(_, rename)| ident_unraw_string(rename));
            record_macro_import(&[source], &local, self.context);
        }

        fn visit_item_use(&mut self, item_use: &'ast syn::ItemUse) {
            let mut bindings = Vec::new();
            flatten_use_tree(&item_use.tree, &mut Vec::new(), &mut bindings);
            for (source, local) in bindings {
                if let Some(local) = local {
                    record_macro_import(&source, &local, self.context);
                } else {
                    // A body-local glob can resolve through enclosing imports;
                    // this bounded prepass deliberately does not reconstruct
                    // that lexical environment.
                    self.context.has_nested_macro_glob = true;
                }
            }
        }

        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            record_local_macro_item(item, self.context);
            visit::visit_item_macro(self, item);
        }
    }

    NestedImportVisitor { context }.visit_item(item);
}

fn record_macro_import(source: &[String], local: &str, context: &mut CrateSyntaxContext) {
    let source = source
        .iter()
        .map(|segment| unraw_ident_text(segment).to_string())
        .collect::<Vec<_>>();
    let local = unraw_ident_text(local);
    let is_exact_trusted_provider_root =
        source == [local.to_string()] && context.trusted_macro_providers.contains(local);
    if is_allowlisted_derive_name(local) {
        if is_trusted_external_derive_import(&source, local, &context.trusted_macro_providers) {
            context.trusted_bare_derives.insert(local.to_string());
        } else {
            context.shadowed_macro_names.insert(local.to_string());
        }
    }
    if matches!(local, "serde" | "error") && !(local == "serde" && is_exact_trusted_provider_root) {
        context.shadowed_helper_names.insert(local.to_string());
    }
    if matches!(local, "serde" | "thiserror") && !is_exact_trusted_provider_root {
        context.shadowed_provider_roots.insert(local.to_string());
    }
    if is_allowlisted_expression_macro_name(local) {
        context
            .shadowed_expression_macro_names
            .insert(local.to_string());
    }
}

fn flatten_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    bindings: &mut Vec<(Vec<String>, Option<String>)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_unraw_string(&path.ident));
            flatten_use_tree(&path.tree, prefix, bindings);
            prefix.pop();
        }
        UseTree::Name(name) => {
            if name.ident == "self" {
                bindings.push((prefix.clone(), prefix.last().cloned()));
            } else {
                let mut source = prefix.clone();
                let name = ident_unraw_string(&name.ident);
                source.push(name.clone());
                bindings.push((source, Some(name)));
            }
        }
        UseTree::Rename(rename) => {
            let mut source = prefix.clone();
            if rename.ident != "self" {
                source.push(ident_unraw_string(&rename.ident));
            }
            bindings.push((source, Some(ident_unraw_string(&rename.rename))));
        }
        UseTree::Glob(_) => bindings.push((prefix.clone(), None)),
        UseTree::Group(group) => {
            for nested in &group.items {
                flatten_use_tree(nested, prefix, bindings);
            }
        }
    }
}

fn module_has_unresolved_macro_glob(items: &[Item]) -> bool {
    module_macro_glob_kinds(items).0
}

fn module_has_local_macro_glob(items: &[Item]) -> bool {
    module_macro_glob_kinds(items).1
}

fn module_macro_glob_kinds(items: &[Item]) -> (bool, bool) {
    let mut local_roots =
        BTreeSet::from(["crate".to_string(), "self".to_string(), "super".to_string()]);
    // The first segment of an import path resolves in Rust's type namespace.
    // Value-only and macro-only definitions therefore cannot prove that an
    // identically named extern-prelude crate is shadowed. Explicit imports and
    // `extern crate` aliases also stay unresolved here: classifying their
    // origin requires name resolution, so the bounded gate fails closed.
    local_roots.extend(
        items
            .iter()
            .filter(|item| !item_may_be_configured_out(item))
            .filter_map(item_type_namespace_ident)
            .map(ident_unraw_string),
    );
    let mut external = false;
    let mut local = false;
    for item in items {
        let Item::Use(item_use) = item else {
            continue;
        };
        let mut bindings = Vec::new();
        flatten_use_tree(&item_use.tree, &mut Vec::new(), &mut bindings);
        for (source, binding) in bindings {
            if binding.is_some() {
                continue;
            }
            // A leading `::` starts from the extern prelude in editions where
            // these public crates are built. A same-named local module cannot
            // make that import local, so preserve the ambiguity rather than
            // classifying by the first path segment alone.
            if item_use.leading_colon.is_some() {
                external = true;
            } else if source
                .first()
                .is_some_and(|root| local_roots.contains(root))
            {
                local = true;
            } else {
                external = true;
            }
        }
    }
    (external, local)
}

fn item_type_namespace_ident(item: &Item) -> Option<&syn::Ident> {
    match item {
        Item::Enum(item) => Some(&item.ident),
        Item::Mod(item) => Some(&item.ident),
        Item::Struct(item) => Some(&item.ident),
        Item::Trait(item) => Some(&item.ident),
        Item::TraitAlias(item) => Some(&item.ident),
        Item::Type(item) => Some(&item.ident),
        Item::Union(item) => Some(&item.ident),
        _ => None,
    }
}

fn item_may_be_configured_out(item: &Item) -> bool {
    item_attributes(item).is_some_and(|attributes| {
        attributes.iter().any(|attribute| {
            rust_path_is_ident(attribute.path(), "cfg") || meta_may_apply_cfg(&attribute.meta)
        })
    })
}

fn meta_may_apply_cfg(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if !rust_path_is_ident(&list.path, "cfg_attr") {
        return false;
    }
    let Ok(nested) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        // Malformed cfg_attr will fail rustc. Treat it as conditional here as
        // well so a syntax-only fixture cannot make an external glob trusted.
        return true;
    };
    nested.iter().skip(1).any(|attribute| {
        rust_path_is_ident(attribute.path(), "cfg") || meta_may_apply_cfg(attribute)
    })
}

fn module_macro_glob_is_ambiguous(items: &[Item], syntax: &CrateSyntaxContext) -> bool {
    syntax.has_nested_macro_glob
        || module_has_unresolved_macro_glob(items)
        || (syntax.has_external_macro_glob && module_has_local_macro_glob(items))
}

fn is_allowlisted_derive_name(name: &str) -> bool {
    matches!(
        name,
        "Clone"
            | "Copy"
            | "Debug"
            | "Default"
            | "Eq"
            | "Hash"
            | "Ord"
            | "PartialEq"
            | "PartialOrd"
            | "Deserialize"
            | "Serialize"
            | "Error"
    )
}

fn is_trusted_external_derive_import(
    source: &[String],
    local: &str,
    trusted_macro_providers: &BTreeSet<String>,
) -> bool {
    matches!(
        (source, local),
        ([provider, derive], "Serialize" | "Deserialize")
            if provider == "serde"
                && derive == local
                && trusted_macro_providers.contains("serde")
    ) || matches!(
        (source, local),
        ([provider, derive], "Error")
            if provider == "thiserror"
                && derive == local
                && trusted_macro_providers.contains("thiserror")
    )
}

fn module_alias_names(items: &[Item]) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    for item in items {
        match item {
            Item::Type(alias) => {
                aliases.insert(ident_unraw_string(&alias.ident));
            }
            Item::Use(item_use) => collect_renamed_imports(&item_use.tree, &mut aliases),
            _ => {}
        }
    }
    aliases
}

fn collect_renamed_imports(tree: &UseTree, aliases: &mut BTreeSet<String>) {
    match tree {
        UseTree::Path(path) => collect_renamed_imports(&path.tree, aliases),
        UseTree::Rename(rename) => {
            aliases.insert(ident_unraw_string(&rename.rename));
        }
        UseTree::Group(group) => {
            for nested in &group.items {
                collect_renamed_imports(nested, aliases);
            }
        }
        UseTree::Name(_) | UseTree::Glob(_) => {}
    }
}

fn scan_crate_module_file(
    source_path: &Path,
    module_dir: &Path,
    defining_source: &Path,
    syntax: &CrateSyntaxContext,
    visited: &mut BTreeSet<PathBuf>,
    inventories: &mut BTreeMap<String, SourceMemberInventory>,
) -> TaskResult {
    if !visited.insert(
        source_path
            .canonicalize()
            .map_err(|error| format!("cannot canonicalize crate module source: {error}"))?,
    ) {
        return Err(format!(
            "crate module source is reachable more than once: {}",
            source_path.display()
        ));
    }
    let file = parse_rust_source(source_path)?;
    reject_rustdoc_sensitive_module_cfg(
        &file.attrs,
        &format!("crate module source {}", source_path.display()),
    )?;
    scan_crate_module_items(
        &file.items,
        source_path,
        module_dir,
        source_path != defining_source,
        true,
        defining_source,
        syntax,
        visited,
        inventories,
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_crate_module_items(
    items: &[Item],
    source_path: &Path,
    module_dir: &Path,
    inspect_current_module: bool,
    file_scope: bool,
    defining_source: &Path,
    syntax: &CrateSyntaxContext,
    visited: &mut BTreeSet<PathBuf>,
    inventories: &mut BTreeMap<String, SourceMemberInventory>,
) -> TaskResult {
    let unresolved_macro_glob = module_macro_glob_is_ambiguous(items, syntax);
    for item in items {
        reject_nested_member_items_with_context(item, source_path, syntax, unresolved_macro_glob)?;
        if inspect_current_module {
            match item {
                Item::Impl(implementation) => {
                    if implementation.trait_.is_some() {
                        let type_name = impl_type_name(implementation)
                            .unwrap_or_else(|| "<unsupported self type>".to_string());
                        reject_trait_surface_escape(implementation, &type_name)
                            .map_err(|error| format!("{}: {error}", source_path.display()))?;
                    } else {
                        let type_name =
                            checked_inherent_impl_type_name(implementation, &syntax.alias_names)
                                .map_err(|error| format!("{}: {error}", source_path.display()))?;
                        inventory_inherent_impl(
                            implementation,
                            &type_name,
                            inventories.entry(type_name.clone()).or_default(),
                        )?;
                    }
                }
                Item::Macro(item) => {
                    let name = item
                        .ident
                        .as_ref()
                        .or_else(|| item.mac.path.segments.last().map(|segment| &segment.ident))
                        .map_or_else(|| "invocation".to_string(), ToString::to_string);
                    return Err(format!(
                        "crate module {} contains macro `{name}`; expansion-aware member inventory is required",
                        source_path.display()
                    ));
                }
                _ => {}
            }
        }

        let Item::Mod(module) = item else {
            continue;
        };
        if cfg_test_only(&module.attrs) {
            continue;
        }
        reject_rustdoc_sensitive_module_cfg(
            &module.attrs,
            &format!(
                "crate module `{}` in {}",
                module.ident,
                source_path.display()
            ),
        )?;
        let explicit_path = checked_module_path(module, source_path, file_scope)?;
        let module_name = ident_unraw_string(&module.ident);
        let child_module_dir = module_dir.join(&module_name);
        if let Some((_, child_items)) = &module.content {
            scan_crate_module_items(
                child_items,
                source_path,
                &child_module_dir,
                true,
                false,
                defining_source,
                syntax,
                visited,
                inventories,
            )?;
            continue;
        }

        let (child_source, child_module_dir) =
            resolve_crate_module(module, source_path, module_dir, explicit_path)?;
        scan_crate_module_file(
            &child_source,
            &child_module_dir,
            defining_source,
            syntax,
            visited,
            inventories,
        )?;
    }
    Ok(())
}

/// Reject API-bearing items nested inside another item's body. Rust permits an
/// `impl` inside an anonymous const block, and its members remain callable even
/// though Rustdoc does not render them on the type's page. Resolving the full
/// expansion and scope rules here would be fragile, so this uncommon syntax
/// must be moved to module scope before it can enter the supported facade.
#[cfg(test)]
fn reject_nested_member_items(root_item: &Item, source_path: &Path) -> TaskResult {
    reject_nested_member_items_with_context(
        root_item,
        source_path,
        &CrateSyntaxContext::default(),
        false,
    )
}

fn reject_nested_member_items_with_context(
    root_item: &Item,
    source_path: &Path,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> TaskResult {
    struct NestedItemVisitor<'scope> {
        item_depth: usize,
        violation: Option<String>,
        syntax: &'scope CrateSyntaxContext,
        unresolved_macro_glob: bool,
        helpers: Vec<DeriveHelpers>,
    }

    impl<'ast> Visit<'ast> for NestedItemVisitor<'_> {
        fn visit_item(&mut self, item: &'ast Item) {
            let helpers = match item_derive_helpers(item, self.syntax, self.unresolved_macro_glob) {
                Ok(helpers) => helpers,
                Err(error) => {
                    self.violation = Some(error);
                    return;
                }
            };
            let is_root_module = self.item_depth == 0 && matches!(item, Item::Mod(_));
            self.helpers.push(helpers);
            if is_root_module && self.violation.is_none() {
                for attribute in item_attributes(item).into_iter().flatten() {
                    // File selection is validated by checked_module_path in
                    // both crate walkers, not by the macro attribute census.
                    if rust_path_is_ident(attribute.path(), "path") {
                        continue;
                    }
                    self.visit_attribute(attribute);
                }
            }
            if self.item_depth > 0 && self.violation.is_none() {
                self.violation = match item {
                    Item::Impl(implementation) if implementation.trait_.is_none() => {
                        Some("contains a nested inherent implementation; move API-bearing items to module scope for explicit facade inventory".to_string())
                    }
                    Item::Impl(implementation) => reject_trait_surface_escape(
                        implementation,
                        "nested nominal type",
                    )
                    .err(),
                    Item::Macro(_) => Some("contains a nested macro; move API-bearing items to module scope for explicit facade inventory".to_string()),
                    Item::Mod(_) => Some("contains a nested module; move API-bearing items to module scope for explicit facade inventory".to_string()),
                    _ => None,
                };
            }
            self.item_depth += 1;
            if !is_root_module && self.violation.is_none() {
                visit::visit_item(self, item);
            }
            self.item_depth -= 1;
            self.helpers.pop();
        }

        fn visit_stmt_macro(&mut self, statement: &'ast syn::StmtMacro) {
            if self.item_depth > 0 {
                self.violation = Some("contains a nested item-position macro; move API-bearing items to module scope for explicit facade inventory".to_string());
            } else {
                visit::visit_stmt_macro(self, statement);
            }
        }

        fn visit_expr_macro(&mut self, expression: &'ast syn::ExprMacro) {
            if self.item_depth == 0 || self.violation.is_some() {
                return;
            }
            let segments = expression
                .mac
                .path
                .segments
                .iter()
                .map(|segment| ident_unraw_string(&segment.ident))
                .collect::<Vec<_>>();
            let token_hazard = expression_macro_token_hazard(
                &expression.mac.tokens,
                self.syntax,
                self.unresolved_macro_glob,
            );
            let rejection_reason = match segments.as_slice() {
                [_] if expression.mac.path.leading_colon.is_some() => {
                    Some("its path is not a bare allowlisted macro name".to_string())
                }
                [name] if !is_allowlisted_expression_macro_name(name) => {
                    Some("its name is outside the standard/compiler allowlist".to_string())
                }
                [_] if self.unresolved_macro_glob => {
                    Some("a glob import makes its origin ambiguous".to_string())
                }
                [name] if self.syntax.shadowed_expression_macro_names.contains(name) => {
                    Some("an import or local macro can shadow the allowlisted name".to_string())
                }
                [_] => token_hazard.map(|hazard| format!("its opaque arguments contain {hazard}")),
                _ => Some("its path is not a bare allowlisted macro name".to_string()),
            };
            if let Some(reason) = rejection_reason {
                let identity = rust_path_identity(&expression.mac.path);
                self.violation = Some(format!(
                    "contains expression macro `{identity}` whose expansion is not proven item-free ({reason}); use an unshadowed allowlisted standard/compiler expression macro or add explicit inventory support"
                ));
            }
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            if self.item_depth > 0 && self.violation.is_none() {
                let identity = rust_path_identity(&invocation.path);
                self.violation = Some(format!(
                    "contains non-expression macro `{identity}`; expansion-aware member inventory is required"
                ));
            }
        }

        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            if self.violation.is_none() {
                if let Err(error) = check_non_generating_attribute(
                    &attribute.meta,
                    self.syntax,
                    self.unresolved_macro_glob,
                    self.helpers.last().copied().unwrap_or_default(),
                ) {
                    self.violation = Some(error);
                }
            }
        }
    }

    let mut visitor = NestedItemVisitor {
        item_depth: 0,
        violation: None,
        syntax,
        unresolved_macro_glob,
        helpers: Vec::new(),
    };
    visitor.visit_item(root_item);
    if let Some(violation) = visitor.violation {
        return Err(format!(
            "crate module {} {violation}",
            source_path.display()
        ));
    }
    Ok(())
}

/// Standard/compiler macros whose allowlisted expression forms cannot synthesize
/// facade members. Bare names are accepted only when the crate-wide name
/// prepass found no import, local definition, or unresolved glob that could
/// shadow the standard macro.
fn is_allowlisted_expression_macro_name(name: &str) -> bool {
    matches!(
        name,
        "assert"
            | "assert_eq"
            | "assert_ne"
            | "cfg"
            | "column"
            | "concat"
            | "dbg"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "eprint"
            | "eprintln"
            | "env"
            | "file"
            | "format"
            | "format_args"
            | "include_bytes"
            | "include_str"
            | "line"
            | "matches"
            | "module_path"
            | "option_env"
            | "panic"
            | "print"
            | "println"
            | "stringify"
            | "todo"
            | "unimplemented"
            | "unreachable"
            | "vec"
            | "write"
            | "writeln"
    )
}

/// An allowlisted outer macro still parses its arguments from an opaque token
/// stream. Reject tokens that can introduce a nested macro/attribute, change
/// macro-name origin inside a block, or hide a direct trait/inherent
/// implementation. Attribute punctuation is rejected conservatively; macro
/// calls are distinguished from ordinary unary `!` and `!=` operators by their
/// `name!` + delimiter shape.
fn expression_macro_token_hazard(
    tokens: &proc_macro2::TokenStream,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> Option<String> {
    let tokens = tokens.clone().into_iter().collect::<Vec<_>>();
    tokens
        .iter()
        .enumerate()
        .find_map(|(index, token)| match token {
            proc_macro2::TokenTree::Group(group) => {
                expression_macro_token_hazard(&group.stream(), syntax, unresolved_macro_glob)
            }
            proc_macro2::TokenTree::Ident(ident) => {
                let semantic_ident = ident_unraw_string(ident);
                if matches!(
                    semantic_ident.as_str(),
                    "impl" | "use" | "extern" | "macro" | "macro_rules"
                ) {
                    return Some(format!(
                        "a binding- or member-changing `{semantic_ident}` token"
                    ));
                }
                let Some(proc_macro2::TokenTree::Punct(bang)) = tokens.get(index + 1) else {
                    return None;
                };
                if bang.as_char() != '!'
                    || !matches!(
                        tokens.get(index + 2),
                        Some(proc_macro2::TokenTree::Group(_))
                    )
                {
                    return None;
                }
                let name = semantic_ident;
                let qualified = index > 1
                    && matches!(
                        tokens.get(index - 1),
                        Some(proc_macro2::TokenTree::Punct(colon)) if colon.as_char() == ':'
                    )
                    && matches!(
                        tokens.get(index - 2),
                        Some(proc_macro2::TokenTree::Punct(colon)) if colon.as_char() == ':'
                    );
                if qualified {
                    Some(format!("a qualified nested macro `{name}!`"))
                } else if unresolved_macro_glob {
                    Some(format!(
                        "a nested macro `{name}!` with a glob-ambiguous origin"
                    ))
                } else if !is_allowlisted_expression_macro_name(&name) {
                    Some(format!("an unlisted nested macro `{name}!`"))
                } else if syntax.shadowed_expression_macro_names.contains(&name) {
                    Some(format!("a shadowable nested macro `{name}!`"))
                } else {
                    None
                }
            }
            proc_macro2::TokenTree::Punct(punctuation) if punctuation.as_char() == '#' => {
                Some("attribute-like `#` punctuation".to_string())
            }
            proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => None,
        })
}

/// Accept only allowlisted built-in/inert attributes and derive macros whose
/// locked implementations are known not to synthesize inherent public API.
/// Unknown attributes fail closed because a procedural macro can emit sibling
/// items or hidden inherent members that the syntax inventory cannot see.
fn check_non_generating_attribute(
    meta: &Meta,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
    helpers: DeriveHelpers,
) -> TaskResult {
    let path = meta_path_identity(meta.path());
    match path.as_str() {
        "derive" => {
            checked_derive_helpers(meta, syntax, unresolved_macro_glob)?;
            Ok(())
        }
        "cfg_attr" => {
            let Meta::List(list) = meta else {
                return Err("contains malformed `cfg_attr`; expansion-aware facade inventory is required".to_string());
            };
            let nested = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .map_err(|_| {
                    "contains an unparseable `cfg_attr`; expansion-aware facade inventory is required"
                        .to_string()
                })?;
            if nested.len() < 2 {
                return Err("contains malformed `cfg_attr`; expansion-aware facade inventory is required".to_string());
            }
            if is_charon_rename_cfg_attr(&nested) {
                return Ok(());
            }
            for attribute in nested.iter().skip(1) {
                check_non_generating_attribute(
                    attribute,
                    syntax,
                    unresolved_macro_glob,
                    helpers,
                )?;
            }
            Ok(())
        }
        "serde" if helpers.serde => {
            check_helper_attribute_origin("serde", syntax, unresolved_macro_glob)
        }
        "error" if helpers.thiserror => {
            check_helper_attribute_origin("error", syntax, unresolved_macro_glob)
        }
        "serde" | "error" => Err(format!(
            "contains helper attribute `{path}` without an origin-validated derive on the same item"
        )),
        // This is deliberately an allowlist, not an attempt to reproduce
        // rustc's attribute resolver. Adding another attribute requires an
        // an explicit check of whether it can expand into public API.
        "allow" | "cfg" | "cold" | "default" | "deny" | "deprecated" | "doc"
        | "expect" | "forbid" | "inline" | "macro_export" | "must_use"
        | "non_exhaustive" | "repr" | "should_panic" | "test" | "track_caller" | "warn" => {
            Ok(())
        }
        _ => Err(format!(
            "contains unlisted attribute macro candidate `{path}`; expansion-aware facade inventory is required"
        )),
    }
}

/// The allowlisted tool attribute uses either `charon` alone or the exact
/// `all(charon, feature = "aeneas-color-clone")` predicate. rustc never sets
/// `cfg(charon)` (only the Charon/Aeneas extraction command line does, together
/// with `register_tool(charon)`), so in every compiled build the attribute is
/// erased before resolution; under Charon it is a registered tool attribute,
/// which rustc never expands. It therefore cannot synthesize API. Any other
/// predicate, any extra attribute in the same `cfg_attr`, or any other shape
/// of `charon::rename` stays outside the allowlist and fails closed.
fn is_charon_rename_cfg_attr(nested: &Punctuated<Meta, Token![,]>) -> bool {
    if nested.len() != 2 {
        return false;
    }
    let is_charon = |meta: &Meta| {
        matches!(meta, Meta::Path(path)
            if path.leading_colon.is_none() && meta_path_identity(path) == "charon")
    };
    let allowlisted_predicate = match &nested[0] {
        Meta::Path(_) => is_charon(&nested[0]),
        Meta::List(all)
            if all.path.leading_colon.is_none()
                && meta_path_identity(&all.path) == "all"
                && matches!(all.delimiter, syn::MacroDelimiter::Paren(_)) =>
        {
            all.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .is_ok_and(|terms| {
                    terms.len() == 2
                        && is_charon(&terms[0])
                        && matches!(&terms[1], Meta::NameValue(feature)
                            if feature.path.leading_colon.is_none()
                                && meta_path_identity(&feature.path) == "feature"
                                && matches!(&feature.value,
                                    syn::Expr::Lit(syn::ExprLit { attrs, lit: syn::Lit::Str(value) })
                                    if attrs.is_empty() && value.value() == "aeneas-color-clone"))
                })
        }
        _ => false,
    };
    let Meta::List(rename) = &nested[1] else {
        return false;
    };
    allowlisted_predicate
        && rename.path.leading_colon.is_none()
        && meta_path_identity(&rename.path) == "charon::rename"
        && matches!(rename.delimiter, syn::MacroDelimiter::Paren(_))
        && syn::parse2::<syn::LitStr>(rename.tokens.clone()).is_ok()
}

fn item_derive_helpers(
    item: &Item,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> TaskResult<DeriveHelpers> {
    let mut helpers = DeriveHelpers::default();
    for attribute in item_attributes(item).into_iter().flatten() {
        merge_derive_helpers_from_meta(
            &attribute.meta,
            syntax,
            unresolved_macro_glob,
            &mut helpers,
        )?;
    }
    Ok(helpers)
}

fn merge_derive_helpers_from_meta(
    meta: &Meta,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
    helpers: &mut DeriveHelpers,
) -> TaskResult {
    if rust_path_is_ident(meta.path(), "derive") {
        let found = checked_derive_helpers(meta, syntax, unresolved_macro_glob)?;
        helpers.serde |= found.serde;
        helpers.thiserror |= found.thiserror;
        return Ok(());
    }
    if !rust_path_is_ident(meta.path(), "cfg_attr") {
        return Ok(());
    }
    let Meta::List(list) = meta else {
        return Err(
            "contains malformed `cfg_attr`; expansion-aware facade inventory is required".into(),
        );
    };
    let nested = list
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .map_err(|_| {
            "contains an unparseable `cfg_attr`; expansion-aware facade inventory is required"
                .to_string()
        })?;
    if nested.len() < 2 {
        return Err(
            "contains malformed `cfg_attr`; expansion-aware facade inventory is required".into(),
        );
    }
    for attribute in nested.iter().skip(1) {
        merge_derive_helpers_from_meta(attribute, syntax, unresolved_macro_glob, helpers)?;
    }
    Ok(())
}

fn checked_derive_helpers(
    meta: &Meta,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> TaskResult<DeriveHelpers> {
    let Meta::List(list) = meta else {
        return Err(
            "contains malformed `derive`; expansion-aware facade inventory is required".into(),
        );
    };
    let derives = list
        .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
        .map_err(|_| {
            "contains an unparseable `derive`; expansion-aware facade inventory is required"
                .to_string()
        })?;
    let mut helpers = DeriveHelpers::default();
    for derive in derives {
        let found = checked_derive(&derive, syntax, unresolved_macro_glob)?;
        helpers.serde |= found.serde;
        helpers.thiserror |= found.thiserror;
    }
    Ok(helpers)
}

fn checked_derive(
    derive: &syn::Path,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> TaskResult<DeriveHelpers> {
    let identity = rust_path_identity(derive);
    let segments = derive
        .segments
        .iter()
        .map(|segment| ident_unraw_string(&segment.ident))
        .collect::<Vec<_>>();
    if let [name] = segments.as_slice() {
        if !is_allowlisted_derive_name(name) {
            return Err(unlisted_derive_error(&identity));
        }
        if unresolved_macro_glob || syntax.shadowed_macro_names.contains(name) {
            return Err(format!(
                "contains derive macro `{identity}` whose bare name may be shadowed by an import; expansion-aware facade inventory is required"
            ));
        }
        // Bare derive origin is collected crate-wide. Even an absolute
        // import in one module cannot safely exempt a same-named local
        // provider in another without lexical name resolution, so any
        // provider-root shadow keeps the bounded gate fail-closed.
        return match name.as_str() {
            "Serialize" | "Deserialize"
                if syntax.trusted_bare_derives.contains(name)
                    && !syntax.shadowed_provider_roots.contains("serde") =>
            {
                Ok(DeriveHelpers {
                    serde: true,
                    thiserror: false,
                })
            }
            "Error"
                if syntax.trusted_bare_derives.contains(name)
                    && !syntax.shadowed_provider_roots.contains("thiserror") =>
            {
                Ok(DeriveHelpers {
                    serde: false,
                    thiserror: true,
                })
            }
            "Serialize" | "Deserialize" | "Error" => Err(format!(
                "contains derive macro `{identity}` without an unambiguous exact allowlisted provider import"
            )),
            _ => Ok(DeriveHelpers::default()),
        };
    }
    let ([provider, name], true) = (
        segments.as_slice(),
        matches!(
            identity.as_str(),
            "serde::Serialize"
                | "serde::Deserialize"
                | "::serde::Serialize"
                | "::serde::Deserialize"
                | "thiserror::Error"
                | "::thiserror::Error"
        ),
    ) else {
        return Err(unlisted_derive_error(&identity));
    };
    if !syntax.trusted_macro_providers.contains(provider) {
        return Err(format!(
            "contains derive macro `{identity}` without an exact allowlisted Cargo provider"
        ));
    }
    let provider_is_shadowed = if derive.leading_colon.is_some() {
        syntax.shadowed_absolute_provider_roots.contains(provider)
    } else {
        syntax.shadowed_provider_roots.contains(provider)
    };
    if provider_is_shadowed || (derive.leading_colon.is_none() && unresolved_macro_glob) {
        return Err(format!(
            "contains derive macro `{identity}` whose provider root may be shadowed; remove the competing binding"
        ));
    }
    Ok(DeriveHelpers {
        serde: matches!(provider.as_str(), "serde")
            && matches!(name.as_str(), "Serialize" | "Deserialize"),
        thiserror: provider == "thiserror" && name == "Error",
    })
}

fn unlisted_derive_error(identity: &str) -> String {
    format!(
        "contains unlisted derive macro `{identity}`; expansion-aware facade inventory is required"
    )
}

fn check_helper_attribute_origin(
    helper: &str,
    syntax: &CrateSyntaxContext,
    unresolved_macro_glob: bool,
) -> TaskResult {
    if unresolved_macro_glob || syntax.shadowed_helper_names.contains(helper) {
        return Err(format!(
            "contains helper attribute `{helper}` whose name may resolve to an imported procedural macro"
        ));
    }
    Ok(())
}

fn meta_path_identity(path: &syn::Path) -> String {
    rust_path_identity(path)
}

fn rust_path_identity(path: &syn::Path) -> String {
    let body = path
        .segments
        .iter()
        .map(|segment| ident_unraw_string(&segment.ident))
        .collect::<Vec<_>>()
        .join("::");
    if path.leading_colon.is_some() {
        format!("::{body}")
    } else {
        body
    }
}

/// Compare a single-segment path by semantic spelling. Rust accepts raw
/// spellings such as `r#cfg`, `r#derive`, and `r#doc`; security-sensitive
/// syntax classification must therefore treat them exactly like their ordinary
/// built-in spellings while keeping a leading `::` distinct.
fn rust_path_is_ident(path: &syn::Path, expected: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && ident_matches_unraw(&path.segments[0].ident, expected)
}

fn cfg_test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let Meta::List(list) = &attribute.meta else {
            return false;
        };
        rust_path_is_ident(&list.path, "cfg")
            && list
                .parse_args::<Meta>()
                .is_ok_and(|predicate| cfg_predicate_requires_test(&predicate))
    })
}

fn cfg_predicate_requires_test(predicate: &Meta) -> bool {
    match predicate {
        Meta::Path(path) => rust_path_is_ident(path, "test"),
        Meta::List(list) if rust_path_is_ident(&list.path, "all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|conjuncts| conjuncts.iter().any(cfg_predicate_requires_test)),
        Meta::List(_) | Meta::NameValue(_) => false,
    }
}

fn impl_type_name(implementation: &syn::ItemImpl) -> Option<String> {
    let syn::Type::Path(path) = implementation.self_ty.as_ref() else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| ident_unraw_string(&segment.ident))
}

/// Return the nominal self type of an inherent implementation, rejecting any
/// name that may be a transparent type/import alias in the scanned crate.
fn checked_inherent_impl_type_name(
    implementation: &syn::ItemImpl,
    alias_names: &BTreeSet<String>,
) -> TaskResult<String> {
    let type_name = impl_type_name(implementation)
        .ok_or_else(|| "inherent impl has an unsupported self type".to_string())?;
    if alias_names.contains(&type_name) {
        return Err(format!(
            "inherent impl self type `{type_name}` may be a transparent type/import alias; alias-based inherent implementations require explicit name-resolution support"
        ));
    }
    Ok(type_name)
}

/// Reject trait implementations hidden from Rustdoc or able to add
/// unmanifested members through Rust's automatic dereference lookup.
///
/// Trait paths alone are insufficient: `use core::ops::Deref as Clone` is a
/// legal spelling, and an imported impl-item macro can provide the required
/// members. The method names are fixed by `Deref`/`DerefMut`, so checking both
/// the path and item shape keeps this bounded syntax gate fail-closed without
/// pretending to perform Rust name resolution. Visible ordinary trait impls
/// remain represented by Rustdoc instead of the inherent-member manifest, but
/// an impl-level `doc(hidden)` would remove that only public record while its
/// methods and operators remain callable.
fn reject_trait_surface_escape(implementation: &syn::ItemImpl, type_name: &str) -> TaskResult {
    let Some((_, trait_path, _)) = &implementation.trait_ else {
        return Ok(());
    };
    if doc_hidden(&implementation.attrs) {
        return Err(format!(
            "type `{type_name}` in a supported facade's defining crate has a doc-hidden trait implementation; keep callable trait API visible in Rustdoc or add explicit trait-surface inventory support"
        ));
    }
    if implementation
        .attrs
        .iter()
        .any(|attribute| meta_has_rustdoc_sensitive_cfg(&attribute.meta))
    {
        return Err(format!(
            "type `{type_name}` in a supported facade's defining crate has a trait implementation whose configuration depends on Rustdoc; keep callable trait API visible under `cfg(doc)` or add explicit trait-surface inventory support"
        ));
    }
    let named_autoderef_trait = trait_path.segments.last().is_some_and(|segment| {
        ident_matches_unraw(&segment.ident, "Deref")
            || ident_matches_unraw(&segment.ident, "DerefMut")
    });
    let shaped_like_autoderef = implementation.items.iter().any(|item| {
        matches!(item, ImplItem::Fn(method)
            if ident_matches_unraw(&method.sig.ident, "deref")
                || ident_matches_unraw(&method.sig.ident, "deref_mut"))
    });
    let contains_macro = implementation
        .items
        .iter()
        .any(|item| matches!(item, ImplItem::Macro(_)));
    if named_autoderef_trait || shaped_like_autoderef || contains_macro {
        return Err(format!(
            "type `{type_name}` in a supported facade's defining crate has an autoderef-capable or macro-generated trait implementation; explicit member-surface inventory support is required"
        ));
    }
    Ok(())
}

/// Reject Rustdoc-sensitive `cfg`/`cfg_attr` on a module boundary.
///
/// [`reject_trait_surface_escape`] inspects the attributes attached directly
/// to an `impl`, but Rust configuration is inherited: an implementation inside
/// `#[cfg(not(doc))] mod hidden { ... }`, or in a child file whose inner
/// attributes carry the same predicate, is compiled into every normal build
/// while Rustdoc omits it. Ordinary trait implementations have no manifest
/// record other than the rendered Rustdoc page, so the whole module boundary
/// fails closed instead of propagating configuration state through the scan.
fn reject_rustdoc_sensitive_module_cfg(
    attributes: &[Attribute],
    module_description: &str,
) -> TaskResult {
    if attributes
        .iter()
        .any(|attribute| meta_has_rustdoc_sensitive_cfg(&attribute.meta))
    {
        return Err(format!(
            "{module_description} is an enclosing module whose configuration depends on Rustdoc; keep every module that may contain callable trait API visible under `cfg(doc)` or add explicit trait-surface inventory support"
        ));
    }
    Ok(())
}

/// Whether `cfg`/`cfg_attr` can make an item differ between normal compilation
/// and Rustdoc, which enables `cfg(doc)` while rendering the public surface.
///
/// Only predicates that mention `doc` or `doctest` are rejected here. Ordinary
/// feature/target configuration remains acceptable because the isolated Cargo
/// audit builds source and Rustdoc with the same target and all-feature graph.
/// Malformed configuration fails closed as well, so syntax-only unit fixtures
/// cannot bypass the check before rustc reports their parse error.
fn meta_has_rustdoc_sensitive_cfg(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if rust_path_is_ident(&list.path, "cfg") {
        return list
            .parse_args::<Meta>()
            .map_or(true, |predicate| cfg_predicate_mentions_rustdoc(&predicate));
    }
    if !rust_path_is_ident(&list.path, "cfg_attr") {
        return false;
    }
    let Ok(nested) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        return true;
    };
    if nested.len() < 2 {
        return true;
    }
    nested.first().is_some_and(cfg_predicate_mentions_rustdoc)
        || nested.iter().skip(1).any(meta_has_rustdoc_sensitive_cfg)
}

fn cfg_predicate_mentions_rustdoc(predicate: &Meta) -> bool {
    if rust_path_is_ident(predicate.path(), "doc")
        || rust_path_is_ident(predicate.path(), "doctest")
    {
        return true;
    }
    let Meta::List(list) = predicate else {
        return false;
    };
    list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .map_or(true, |nested| {
            nested.iter().any(cfg_predicate_mentions_rustdoc)
        })
}

/// Compare a parsed identifier by its semantic spelling. Rust raw identifiers
/// such as `r#Deref` and `r#deref` name the same items as their ordinary forms
/// and must not bypass a syntax-only public-surface gate.
fn ident_matches_unraw(ident: &syn::Ident, expected: &str) -> bool {
    ident_unraw_string(ident) == expected
}

fn ident_unraw_string(ident: &syn::Ident) -> String {
    unraw_ident_text(&ident.to_string()).to_string()
}

fn unraw_ident_text(identifier: &str) -> &str {
    identifier.strip_prefix("r#").unwrap_or(identifier)
}

fn parse_rust_source(path: &Path) -> TaskResult<syn::File> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("cannot read Rust source {}: {error}", path.display()))?;
    syn::parse_file(&source)
        .map_err(|error| format!("cannot parse Rust source {}: {error}", path.display()))
}

fn item_definition_ident(item: &Item) -> Option<&syn::Ident> {
    match item {
        Item::Const(item) => Some(&item.ident),
        Item::Enum(item) => Some(&item.ident),
        Item::Fn(item) => Some(&item.sig.ident),
        Item::Static(item) => Some(&item.ident),
        Item::Struct(item) => Some(&item.ident),
        Item::Trait(item) => Some(&item.ident),
        Item::TraitAlias(item) => Some(&item.ident),
        Item::Type(item) => Some(&item.ident),
        Item::Union(item) => Some(&item.ident),
        _ => None,
    }
}

fn item_attributes(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::ExternCrate(item) => Some(&item.attrs),
        Item::Fn(item) => Some(&item.attrs),
        Item::ForeignMod(item) => Some(&item.attrs),
        Item::Impl(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        Item::Mod(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Trait(item) => Some(&item.attrs),
        Item::TraitAlias(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Union(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        Item::Verbatim(_) => None,
        _ => None,
    }
}

fn doc_hidden(attributes: &[Attribute]) -> bool {
    attributes
        .iter()
        .any(|attribute| meta_contains_doc_hidden(&attribute.meta))
}

fn meta_contains_doc_hidden(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if !rust_path_is_ident(&list.path, "doc") && !rust_path_is_ident(&list.path, "cfg_attr") {
        return false;
    }
    let nested = list
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .ok();
    // A malformed `doc`/`cfg_attr` list will fail rustc too. Treating it as
    // hidden here keeps this syntax-only inventory fail-closed even when used
    // in a focused unit test without compiling the fixture.
    let Some(nested) = nested else {
        return true;
    };
    if rust_path_is_ident(&list.path, "doc") {
        return nested
            .iter()
            .any(|meta| rust_path_is_ident(meta.path(), "hidden"));
    }
    // The first cfg_attr argument is its predicate; only the remaining
    // conditional attributes can change Rustdoc visibility.
    nested.iter().skip(1).any(meta_contains_doc_hidden)
}

fn has_documentation(attributes: &[Attribute]) -> TaskResult<bool> {
    let mut documented = false;
    for attribute in attributes
        .iter()
        .filter(|attribute| rust_path_is_ident(attribute.path(), "doc"))
    {
        match &attribute.meta {
            Meta::NameValue(value) => match &value.value {
                Expr::Lit(literal) => match &literal.lit {
                    syn::Lit::Str(text) => documented |= !text.value().trim().is_empty(),
                    _ => {
                        return Err("supported API has a non-string `doc` attribute".to_string());
                    }
                },
                _ => {
                    return Err(
                        "supported API has a computed `doc` attribute; inventory support must be explicit"
                            .to_string(),
                    );
                }
            },
            Meta::List(_) | Meta::Path(_) => {}
        }
    }
    Ok(documented)
}

fn inventory_source_members(
    definition: &Item,
    module_items: &[Item],
    type_name: &str,
) -> TaskResult<SourceMemberInventory> {
    let alias_names = module_alias_names(module_items);
    match definition {
        Item::Trait(_) | Item::TraitAlias(_) => {
            return Err(format!(
                "supported facade trait `{type_name}` needs explicit trait-member inventory support"
            ));
        }
        Item::Type(_) => {
            return Err(format!(
                "supported facade type alias `{type_name}` needs explicit underlying-member inventory support"
            ));
        }
        _ => {}
    }
    for item in module_items {
        if let Item::Macro(item) = item {
            let name = item
                .ident
                .as_ref()
                .or_else(|| item.mac.path.segments.last().map(|segment| &segment.ident))
                .map_or_else(|| "invocation".to_string(), ToString::to_string);
            return Err(format!(
                "defining module for supported facade type `{type_name}` contains macro `{name}`; expansion-aware member inventory is required"
            ));
        }
    }

    let mut inventory = SourceMemberInventory::default();
    match definition {
        Item::Struct(item) => {
            for (field_index, field) in item.fields.iter().enumerate() {
                if !is_public(&field.vis) {
                    continue;
                }
                if doc_hidden(&field.attrs) {
                    return Err(format!(
                        "supported facade field `{type_name}.{}` is public but doc-hidden",
                        field
                            .ident
                            .as_ref()
                            .map_or_else(|| "<tuple>".to_string(), ToString::to_string)
                    ));
                }
                let field_name = field
                    .ident
                    .as_ref()
                    .map_or_else(|| field_index.to_string(), ident_unraw_string);
                inventory
                    .member_identities
                    .insert(format!("field {field_name}"));
                inventory.struct_fields += 1;
                if !has_documentation(&field.attrs)? {
                    inventory.undocumented_struct_fields += 1;
                    inventory.debt.fields += 1;
                }
            }
        }
        Item::Enum(item) => {
            for variant in &item.variants {
                if doc_hidden(&variant.attrs) {
                    return Err(format!(
                        "supported facade variant `{type_name}::{}` is public but doc-hidden",
                        variant.ident
                    ));
                }
                inventory
                    .member_identities
                    .insert(format!("variant {}", ident_unraw_string(&variant.ident)));
                inventory.variants += 1;
                if !has_documentation(&variant.attrs)? {
                    inventory.debt.variants += 1;
                    inventory
                        .undocumented_variants
                        .insert(ident_unraw_string(&variant.ident));
                }
                for (field_index, field) in variant.fields.iter().enumerate() {
                    if doc_hidden(&field.attrs) {
                        return Err(format!(
                            "supported facade field `{type_name}::{}.{}` is public but doc-hidden",
                            variant.ident,
                            field
                                .ident
                                .as_ref()
                                .map_or_else(|| field_index.to_string(), ToString::to_string)
                        ));
                    }
                    let field_name = field
                        .ident
                        .as_ref()
                        .map_or_else(|| field_index.to_string(), ident_unraw_string);
                    inventory.member_identities.insert(format!(
                        "field {}.{field_name}",
                        ident_unraw_string(&variant.ident)
                    ));
                    match &variant.fields {
                        Fields::Named(_) => inventory.named_enum_fields += 1,
                        Fields::Unnamed(_) => inventory.tuple_enum_fields += 1,
                        Fields::Unit => unreachable!("unit variants have no fields"),
                    }
                    if !has_documentation(&field.attrs)? {
                        match &variant.fields {
                            Fields::Named(_) => inventory.undocumented_named_enum_fields += 1,
                            Fields::Unnamed(_) => inventory.undocumented_tuple_enum_fields += 1,
                            Fields::Unit => unreachable!("unit variants have no fields"),
                        }
                        inventory.debt.fields += 1;
                    }
                }
            }
        }
        Item::Union(item) => {
            for field in &item.fields.named {
                if !is_public(&field.vis) {
                    continue;
                }
                if doc_hidden(&field.attrs) {
                    return Err(format!(
                        "supported facade field `{type_name}.{}` is public but doc-hidden",
                        field
                            .ident
                            .as_ref()
                            .map_or_else(|| "<unnamed>".to_string(), ToString::to_string)
                    ));
                }
                let field_name = field
                    .ident
                    .as_ref()
                    .map_or_else(|| "<unnamed>".to_string(), ident_unraw_string);
                inventory
                    .member_identities
                    .insert(format!("field {field_name}"));
                inventory.struct_fields += 1;
                if !has_documentation(&field.attrs)? {
                    inventory.undocumented_struct_fields += 1;
                    inventory.debt.fields += 1;
                }
            }
        }
        _ => {}
    }

    for implementation in module_items.iter().filter_map(|item| match item {
        Item::Impl(implementation) => Some(implementation),
        _ => None,
    }) {
        if implementation.trait_.is_some() {
            let implementation_type = impl_type_name(implementation)
                .unwrap_or_else(|| "<unsupported self type>".to_string());
            reject_trait_surface_escape(implementation, &implementation_type)?;
            continue;
        }
        if checked_inherent_impl_type_name(implementation, &alias_names)? != type_name {
            continue;
        }
        inventory_inherent_impl(implementation, type_name, &mut inventory)?;
    }
    Ok(inventory)
}

fn inventory_inherent_impl(
    implementation: &syn::ItemImpl,
    type_name: &str,
    inventory: &mut SourceMemberInventory,
) -> TaskResult {
    if doc_hidden(&implementation.attrs) {
        return Err(format!(
            "supported facade impl for `{type_name}` is doc-hidden"
        ));
    }
    for member in &implementation.items {
        match member {
            ImplItem::Fn(method) if is_public(&method.vis) => {
                if doc_hidden(&method.attrs) {
                    return Err(format!(
                        "supported facade method `{type_name}::{}` is public but doc-hidden",
                        method.sig.ident
                    ));
                }
                inventory
                    .member_identities
                    .insert(format!("method {}", ident_unraw_string(&method.sig.ident)));
                inventory.methods += 1;
                if !has_documentation(&method.attrs)? {
                    inventory.debt.methods += 1;
                }
            }
            ImplItem::Const(constant) if is_public(&constant.vis) => {
                if doc_hidden(&constant.attrs) {
                    return Err(format!(
                        "supported facade associated constant `{type_name}::{}` is public but doc-hidden",
                        constant.ident
                    ));
                }
                inventory.member_identities.insert(format!(
                    "associated-const {}",
                    ident_unraw_string(&constant.ident)
                ));
                inventory.associated_constants += 1;
                if !has_documentation(&constant.attrs)? {
                    inventory.debt.associated_constants += 1;
                }
            }
            ImplItem::Macro(item) => {
                let name = item.mac.path.segments.last().map_or_else(
                    || "invocation".to_string(),
                    |segment| segment.ident.to_string(),
                );
                return Err(format!(
                    "supported facade impl for `{type_name}` contains macro `{name}`; expansion-aware member inventory is required"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Member inventories for types defined directly in the compiler facade.
///
/// Root identity and member reconciliation are deliberately separate checks:
/// the former makes adding a type a support decision, while this one proves
/// that adding a field or inherent member cannot hide inside an already
/// approved type name.
fn direct_facade_member_inventories_from_source(
    source: &str,
    external_inherent: &BTreeMap<String, SourceMemberInventory>,
) -> TaskResult<Vec<(String, SourceMemberInventory)>> {
    let file = syn::parse_file(source)
        .map_err(|error| format!("cannot parse compiler facade Rust source: {error}"))?;
    let mut inventories = Vec::new();
    for definition in &file.items {
        let name = match definition {
            Item::Struct(item) if is_public(&item.vis) => Some(ident_unraw_string(&item.ident)),
            Item::Enum(item) if is_public(&item.vis) => Some(ident_unraw_string(&item.ident)),
            Item::Union(item) if is_public(&item.vis) => Some(ident_unraw_string(&item.ident)),
            Item::Trait(item) if is_public(&item.vis) => {
                return Err(format!(
                    "supported facade trait `{}` needs explicit trait-member inventory support",
                    item.ident
                ));
            }
            Item::TraitAlias(item) if is_public(&item.vis) => {
                return Err(format!(
                    "supported facade trait alias `{}` needs explicit trait-member inventory support",
                    item.ident
                ));
            }
            _ => None,
        };
        if let Some(name) = name {
            let mut inventory = inventory_source_members(definition, &file.items, &name)?;
            if let Some(additional) = external_inherent.get(&name) {
                inventory.merge(additional);
            }
            inventories.push((name, inventory));
        }
    }
    Ok(inventories)
}

fn supported_reexports_from_source(source: &str) -> TaskResult<Vec<SupportedReexport>> {
    let file = syn::parse_file(source)
        .map_err(|error| format!("cannot parse compiler facade Rust source: {error}"))?;
    let root_hazards = facade_root_path_hazards(&file.items);
    let mut reexports = Vec::new();
    let mut public_names = BTreeSet::new();
    for item in &file.items {
        match item {
            Item::Use(item_use) if is_public(&item_use.vis) => {
                if doc_hidden(&item_use.attrs) {
                    return Err("supported facade re-export is public but doc-hidden".to_string());
                }
                let mut prefix = Vec::new();
                flatten_supported_use(
                    &item_use.tree,
                    &mut prefix,
                    &mut reexports,
                    &mut public_names,
                    item_use.leading_colon.is_some(),
                    &root_hazards,
                )?;
            }
            Item::Mod(item_mod) if is_public(&item_mod.vis) => {
                return Err(format!(
                    "supported facade module `{}` needs explicit recursive inventory support",
                    item_mod.ident
                ));
            }
            _ => {}
        }
    }
    if reexports.is_empty() {
        return Err("compiler facade has no supported re-exports".into());
    }
    Ok(reexports)
}

/// Names that can make a bare facade re-export resolve somewhere other than
/// the extern prelude.
///
/// This intentionally does not try to reproduce rustc's resolver. Any
/// crate-root type-namespace definition, explicit import binding, renamed
/// `extern crate` binding, or glob import is enough to make a bare origin
/// ambiguous. A renamed `extern crate` binding also shadows a leading-`::`
/// external name, unlike an ordinary module or `use` binding, so that hazard is
/// tracked separately.
fn facade_root_path_hazards(items: &[Item]) -> FacadeRootPathHazards {
    let mut hazards = FacadeRootPathHazards::default();
    hazards.bare_bindings.extend(
        items
            .iter()
            .filter_map(item_type_namespace_ident)
            .map(ident_unraw_string),
    );
    for item in items {
        match item {
            Item::ExternCrate(item) => {
                let source = ident_unraw_string(&item.ident);
                let local = item
                    .rename
                    .as_ref()
                    .map_or_else(|| source.clone(), |(_, rename)| ident_unraw_string(rename));
                if source != local {
                    hazards.bare_bindings.insert(local.clone());
                    hazards.absolute_extern_aliases.insert(local);
                }
            }
            Item::Use(item) => {
                let mut imports = Vec::new();
                flatten_use_tree(&item.tree, &mut Vec::new(), &mut imports);
                for (_, local) in imports {
                    if let Some(local) = local {
                        hazards.bare_bindings.insert(local);
                    } else {
                        hazards.has_glob = true;
                    }
                }
            }
            _ => {}
        }
    }
    hazards
}

/// Exact, reviewable crate-root surface. Rustdoc's `missing_docs` proves that a
/// new item has prose; this inventory separately proves that its API exposure
/// was an intentional support decision.
fn supported_facade_root_items_from_source(source: &str) -> TaskResult<BTreeSet<String>> {
    let file = syn::parse_file(source)
        .map_err(|error| format!("cannot parse compiler facade Rust source: {error}"))?;
    let mut items = supported_reexports_from_source(source)?
        .into_iter()
        .map(|reexport| supported_reexport_identity(&reexport))
        .collect::<BTreeSet<_>>();

    for item in &file.items {
        let direct = match item {
            Item::Const(item) if is_public(&item.vis) => Some((
                "const",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Enum(item) if is_public(&item.vis) => Some((
                "enum",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Fn(item) if is_public(&item.vis) => Some((
                "fn",
                ident_unraw_string(&item.sig.ident),
                item.attrs.as_slice(),
            )),
            Item::Static(item) if is_public(&item.vis) => Some((
                "static",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Struct(item) if is_public(&item.vis) => Some((
                "struct",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Trait(item) if is_public(&item.vis) => {
                return Err(format!(
                    "supported facade trait `{}` needs explicit trait-member inventory support",
                    item.ident
                ));
            }
            Item::TraitAlias(item) if is_public(&item.vis) => {
                return Err(format!(
                    "supported facade trait alias `{}` needs explicit trait-member inventory support",
                    item.ident
                ));
            }
            Item::Type(item) if is_public(&item.vis) => Some((
                "type",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Union(item) if is_public(&item.vis) => Some((
                "union",
                ident_unraw_string(&item.ident),
                item.attrs.as_slice(),
            )),
            Item::Mod(item) if is_public(&item.vis) => {
                return Err(format!(
                    "supported facade module `{}` needs explicit recursive inventory support",
                    item.ident
                ));
            }
            Item::ExternCrate(item) if is_public(&item.vis) => {
                return Err(format!(
                    "public extern crate `{}` needs explicit facade inventory support",
                    item.ident
                ));
            }
            Item::ForeignMod(_) => {
                return Err(
                    "compiler-root extern blocks need explicit facade inventory support".into(),
                );
            }
            Item::Macro(item) => {
                let name = item
                    .ident
                    .as_ref()
                    .or_else(|| item.mac.path.segments.last().map(|segment| &segment.ident))
                    .map_or_else(|| "invocation".to_string(), ToString::to_string);
                return Err(format!(
                    "compiler-root macro `{name}` needs expansion-aware facade inventory support"
                ));
            }
            _ => None,
        };
        let Some((kind, name, attributes)) = direct else {
            continue;
        };
        if doc_hidden(attributes) {
            return Err(format!(
                "supported compiler-root item `{name}` is public but doc-hidden"
            ));
        }
        let entry = match item {
            Item::Type(item) => supported_type_alias_identity(item),
            _ => format!("{kind} {name}"),
        };
        if !items.insert(entry.clone()) {
            return Err(format!("compiler facade repeats root item `{entry}`"));
        }
    }
    Ok(items)
}

/// Type aliases are transparent, so changing only the right-hand side can
/// expose an entirely different method surface without changing the alias
/// name. Pin the complete generic declaration and target token stream in the
/// root manifest. Formatting is canonicalized by `quote` and remains locked by
/// Cargo.lock; a dependency update that changes it requires a manifest review.
/// Raw-versus-ordinary spellings inside the generic declaration or target are
/// intentionally review-visible: this bounded gate does not claim to resolve
/// arbitrary type syntax. The public alias name itself is canonicalized by its
/// semantic spelling.
fn supported_type_alias_identity(item: &syn::ItemType) -> String {
    let generics = item.generics.to_token_stream().to_string();
    let target = item.ty.to_token_stream().to_string();
    let name = ident_unraw_string(&item.ident);
    if generics.is_empty() {
        format!("type {name} = {target}")
    } else {
        format!("type {name} {generics} = {target}")
    }
}

fn supported_reexport_identity(reexport: &SupportedReexport) -> String {
    let mut origin = Vec::with_capacity(reexport.origin_modules.len() + 2);
    origin.push(reexport.origin_crate.as_str());
    origin.extend(reexport.origin_modules.iter().map(String::as_str));
    origin.push(reexport.origin_name.as_str());
    format!("reexport {} = {}", reexport.public_name, origin.join("::"))
}

fn check_identity_manifest(
    inventory: &str,
    marker: &str,
    expected: &BTreeSet<String>,
    label: &str,
) -> TaskResult {
    let begin = format!("<!-- {marker}-begin -->");
    let end = format!("<!-- {marker}-end -->");
    let begins = inventory.match_indices(&begin).collect::<Vec<_>>();
    let ends = inventory.match_indices(&end).collect::<Vec<_>>();
    if begins.len() != 1 || ends.len() != 1 || begins[0].0 >= ends[0].0 {
        return Err(format!(
            "docs/rust-api.md must contain exactly one ordered `{begin}` / `{end}` manifest"
        ));
    }
    let body = &inventory[begins[0].0 + begin.len()..ends[0].0];
    let mut recorded = BTreeSet::new();
    for line in body.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        if !recorded.insert(line.to_string()) {
            return Err(format!(
                "docs/rust-api.md repeats {label} identity `{line}`"
            ));
        }
    }
    if &recorded == expected {
        return Ok(());
    }
    let missing = expected.difference(&recorded).cloned().collect::<Vec<_>>();
    let stale = recorded.difference(expected).cloned().collect::<Vec<_>>();
    Err(format!(
        "{label} identity manifest changed; missing [{}]; stale [{}]; exact expected entries:\n{}",
        missing.join(", "),
        stale.join(", "),
        expected.iter().cloned().collect::<Vec<_>>().join("\n")
    ))
}

fn flatten_supported_use(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    reexports: &mut Vec<SupportedReexport>,
    public_names: &mut BTreeSet<String>,
    absolute: bool,
    root_hazards: &FacadeRootPathHazards,
) -> TaskResult {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_unraw_string(&path.ident));
            let result = flatten_supported_use(
                &path.tree,
                prefix,
                reexports,
                public_names,
                absolute,
                root_hazards,
            );
            prefix.pop();
            result
        }
        UseTree::Name(name) => push_supported_reexport(
            reexports,
            public_names,
            prefix,
            &ident_unraw_string(&name.ident),
            &ident_unraw_string(&name.ident),
            absolute,
            root_hazards,
        ),
        UseTree::Rename(rename) => push_supported_reexport(
            reexports,
            public_names,
            prefix,
            &ident_unraw_string(&rename.ident),
            &ident_unraw_string(&rename.rename),
            absolute,
            root_hazards,
        ),
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_supported_use(
                    item,
                    prefix,
                    reexports,
                    public_names,
                    absolute,
                    root_hazards,
                )?;
            }
            Ok(())
        }
        UseTree::Glob(_) => {
            Err("globbed supported re-export needs an explicit item-level inventory".to_string())
        }
    }
}

fn push_supported_reexport(
    reexports: &mut Vec<SupportedReexport>,
    public_names: &mut BTreeSet<String>,
    prefix: &[String],
    origin_name: &str,
    public_name: &str,
    absolute: bool,
    root_hazards: &FacadeRootPathHazards,
) -> TaskResult {
    let Some((origin_crate, origin_modules)) = prefix.split_first() else {
        return Err(format!(
            "supported re-export `{origin_name} as {public_name}` has no origin crate"
        ));
    };
    if matches!(origin_crate.as_str(), "crate" | "self" | "super") {
        return Err(format!(
            "supported re-export `{origin_crate}::{origin_name}` must name its external defining crate"
        ));
    }
    if root_hazards.absolute_extern_aliases.contains(origin_crate) {
        return Err(format!(
            "supported re-export origin `{origin_crate}` is shadowed by a renamed `extern crate` binding, including for a leading-`::` path; remove the competing binding"
        ));
    }
    if !absolute && (root_hazards.has_glob || root_hazards.bare_bindings.contains(origin_crate)) {
        let reason = if root_hazards.bare_bindings.contains(origin_crate) {
            "a same-named crate-root type/import binding"
        } else {
            "a crate-root glob import"
        };
        return Err(format!(
            "bare supported re-export origin `{origin_crate}` may resolve through {reason}; use an unambiguous leading-`::` external origin or remove the competing binding"
        ));
    }
    if !public_names.insert(public_name.to_string()) {
        return Err(format!(
            "compiler facade re-exports `{public_name}` more than once"
        ));
    }
    reexports.push(SupportedReexport {
        origin_crate: origin_crate.clone(),
        origin_modules: origin_modules.to_vec(),
        origin_name: origin_name.to_string(),
        public_name: public_name.to_string(),
    });
    Ok(())
}

fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public(_))
}

fn rustdoc_page_for_name(root: &Path, name: &str) -> TaskResult<PathBuf> {
    let matches = rustdoc_pages_for_name(root, name)?;
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "rendered rustdoc has no page for supported re-export `{name}` in {}",
            root.display()
        )),
        _ => Err(format!(
            "rendered rustdoc has ambiguous pages for supported re-export `{name}`: {}",
            matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn rustdoc_pages_for_name(root: &Path, name: &str) -> TaskResult<Vec<PathBuf>> {
    let suffix = format!(".{name}.html");
    let mut matches = Vec::new();
    collect_rustdoc_pages(root, &suffix, &mut matches)?;
    matches.sort();
    Ok(matches)
}

fn collect_rustdoc_pages(root: &Path, suffix: &str, matches: &mut Vec<PathBuf>) -> TaskResult {
    for entry in fs::read_dir(root)
        .map_err(|error| format!("cannot read rendered rustdoc {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot inspect rendered rustdoc: {error}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rustdoc_pages(&path, suffix, matches)?;
        } else if path
            .file_name()
            .and_then(|file| file.to_str())
            .is_some_and(|file| file.ends_with(suffix))
        {
            matches.push(path);
        }
    }
    Ok(())
}

fn rustdoc_meta_description(html: &str) -> Option<&str> {
    html.split_once("<meta name=\"description\" content=\"")
        .and_then(|(_, rest)| rest.split_once("\">"))
        .map(|(description, _)| description)
}

fn rustdoc_top_doc(html: &str) -> Option<&str> {
    let (_, top_doc) = html.split_once("<details class=\"toggle top-doc\"")?;
    let (_, docblock) = top_doc.split_once("<div class=\"docblock\">")?;
    docblock
        .split_once("</div></details>")
        .map(|(contents, _)| contents)
}

fn normalize_rustdoc_docblock(docblock: &str) -> String {
    let mut normalized = docblock.to_string();
    for attribute in [" href=\"", " title=\""] {
        while let Some(start) = normalized.find(attribute) {
            let value_start = start + attribute.len();
            let Some(value_end) = normalized[value_start..].find('"') else {
                break;
            };
            normalized.replace_range(start..=(value_start + value_end), "");
        }
    }
    normalized
        .replace("<a>", "")
        .replace("</a>", "")
        .replace("[<code>", "<code>")
        .replace("</code>]", "</code>")
}

fn check_originating_description(
    public_name: &str,
    facade_description: &str,
    origin_descriptions: &BTreeSet<String>,
) -> TaskResult {
    check_originating_contract(
        public_name,
        "meta description",
        facade_description,
        origin_descriptions,
    )
}

fn check_originating_contract(
    public_name: &str,
    contract_name: &str,
    facade_contract: &str,
    origin_contracts: &BTreeSet<String>,
) -> TaskResult {
    if origin_contracts.contains(facade_contract) {
        return Ok(());
    }
    Err(format!(
        "pinewasm_compiler::{public_name} overrides its originating rustdoc {contract_name}; remove documentation from the grouped `pub use` and put group prose in the crate overview"
    ))
}

fn member_doc_debt(html: &str, source: SourceMemberInventory) -> TaskResult<MemberDocDebt> {
    let rendered_struct_fields = rustdoc_section(html, "fields")
        .map(|section| marker_starts(section, "<span id=\"structfield.").len())
        .unwrap_or_default();
    if rendered_struct_fields != source.struct_fields {
        return Err(format!(
            "rustdoc struct-field inventory changed: rendered {rendered_struct_fields}, source has {}",
            source.struct_fields
        ));
    }
    let rendered_undocumented_struct_fields = rustdoc_section(html, "fields")
        .map(|section| undocumented_marker_segments(section, "<span id=\"structfield."))
        .unwrap_or_default();
    if rendered_undocumented_struct_fields != source.undocumented_struct_fields {
        return Err(format!(
            "rustdoc struct-field documentation disagrees with source: rendered {rendered_undocumented_struct_fields}, source has {}",
            source.undocumented_struct_fields
        ));
    }

    let variants = rustdoc_section(html, "variants");
    let rendered_variants = variants
        .map(|section| marker_starts(section, "<section id=\"variant.").len())
        .unwrap_or_default();
    if rendered_variants != source.variants {
        return Err(format!(
            "rustdoc variant inventory changed: rendered {rendered_variants}, source has {}",
            source.variants
        ));
    }
    let mut rendered_undocumented_variants = 0;
    let mut rendered_named_enum_fields = 0;
    let mut rendered_undocumented_named_enum_fields = 0;
    let mut rendered_tuple_enum_fields = 0;
    let mut rendered_undocumented_tuple_enum_fields = 0;
    if let Some(variants) = variants {
        let starts = marker_starts(variants, "<section id=\"variant.");
        for (index, start) in starts.iter().copied().enumerate() {
            let end = starts.get(index + 1).copied().unwrap_or(variants.len());
            let variant = &variants[start..end];
            let header_end = variant
                .find("<div class=\"sub-variant\"")
                .unwrap_or(variant.len());
            if !variant[..header_end].contains("<div class=\"docblock\">") {
                rendered_undocumented_variants += 1;
            }
            let fields = marker_starts(variant, "<div class=\"sub-variant-field\">").len();
            let undocumented_fields =
                undocumented_marker_segments(variant, "<div class=\"sub-variant-field\">");
            if variant.contains("<h4>Fields</h4>") {
                rendered_named_enum_fields += fields;
                rendered_undocumented_named_enum_fields += undocumented_fields;
            } else if variant.contains("<h4>Tuple Fields</h4>") {
                rendered_tuple_enum_fields += fields;
                rendered_undocumented_tuple_enum_fields += undocumented_fields;
            } else if fields != 0 {
                return Err(
                    "rustdoc rendered enum fields without a recognized field-kind heading"
                        .to_string(),
                );
            }
        }
    }
    if rendered_undocumented_variants != source.debt.variants {
        return Err(format!(
            "rustdoc variant documentation disagrees with source: rendered {rendered_undocumented_variants}, source has {}",
            source.debt.variants
        ));
    }
    if rendered_named_enum_fields != source.named_enum_fields
        || rendered_undocumented_named_enum_fields != source.undocumented_named_enum_fields
    {
        return Err(format!(
            "rustdoc named enum-field inventory disagrees with source: rendered {rendered_named_enum_fields} fields ({rendered_undocumented_named_enum_fields} undocumented), source has {} fields ({} undocumented)",
            source.named_enum_fields, source.undocumented_named_enum_fields
        ));
    }
    if rendered_tuple_enum_fields != source.tuple_enum_fields
        || rendered_undocumented_tuple_enum_fields != source.undocumented_tuple_enum_fields
    {
        return Err(format!(
            "rustdoc tuple enum-field inventory disagrees with source: rendered {rendered_tuple_enum_fields} fields ({rendered_undocumented_tuple_enum_fields} undocumented), source has {} fields ({} undocumented)",
            source.tuple_enum_fields, source.undocumented_tuple_enum_fields
        ));
    }

    let implementations = rustdoc_section(html, "implementations");
    let rendered_methods = implementations
        .map(|section| marker_starts(section, "<section id=\"method.").len())
        .unwrap_or_default();
    let rendered_constants = implementations
        .map(|section| marker_starts(section, "<section id=\"associatedconstant.").len())
        .unwrap_or_default();
    let rendered_undocumented_methods = implementations
        .map(|section| undocumented_impl_items(section, "<section id=\"method."))
        .unwrap_or_default();
    let rendered_undocumented_constants = implementations
        .map(|section| undocumented_impl_items(section, "<section id=\"associatedconstant."))
        .unwrap_or_default();
    if rendered_methods != source.methods
        || rendered_constants != source.associated_constants
        || rendered_undocumented_methods != source.debt.methods
        || rendered_undocumented_constants != source.debt.associated_constants
    {
        return Err(format!(
            "rustdoc inherent-member inventory disagrees with source: rendered {rendered_methods} methods/{rendered_constants} constants ({rendered_undocumented_methods}/{} undocumented), source has {} methods/{} constants ({}/{} undocumented)",
            rendered_undocumented_constants,
            source.methods,
            source.associated_constants,
            source.debt.methods,
            source.debt.associated_constants
        ));
    }

    // Keep the source census authoritative as well as the rendered one. This
    // catches both a rustdoc-format parser regression and tuple payloads being
    // accidentally dropped by either side of the reconciliation.
    debug_assert!(source.undocumented_tuple_enum_fields <= source.tuple_enum_fields);
    Ok(source.debt)
}

fn rustdoc_section<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let heading = format!("<h2 id=\"{id}\"");
    let (_, after_heading) = html.split_once(&heading)?;
    let (_, contents) = after_heading.split_once("</h2>")?;
    Some(
        contents
            .split_once("<h2")
            .map_or(contents, |(section, _)| section),
    )
}

fn marker_starts(haystack: &str, marker: &str) -> Vec<usize> {
    haystack
        .match_indices(marker)
        .map(|(index, _)| index)
        .collect()
}

fn undocumented_marker_segments(haystack: &str, marker: &str) -> usize {
    let starts = marker_starts(haystack, marker);
    starts
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(haystack.len());
            !haystack[*start..end].contains("<div class=\"docblock\">")
        })
        .count()
}

fn undocumented_impl_items(haystack: &str, marker: &str) -> usize {
    haystack
        .match_indices(marker)
        .filter(|(start, matched)| {
            let after_start = *start + matched.len();
            let remainder = &haystack[after_start..];
            let end = remainder
                .find("<section id=\"")
                .map_or(haystack.len(), |offset| after_start + offset);
            !haystack[*start..end].contains("<div class=\"docblock\">")
        })
        .count()
}

fn check_member_doc_debt_marker(inventory: &str, actual: MemberDocDebt) -> TaskResult {
    const PREFIX: &str = "<!-- rust-api-member-doc-debt: undocumented-variants=";
    let markers = inventory
        .match_indices(PREFIX)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if markers.len() != 1 {
        return Err(format!(
            "docs/rust-api.md must contain exactly one `{PREFIX}...` marker (found {})",
            markers.len()
        ));
    }
    let marker = &inventory[markers[0] + PREFIX.len()..];
    let (variants, marker) = marker
        .split_once(" undocumented-fields=")
        .ok_or_else(|| "malformed Rust API member-documentation debt marker".to_string())?;
    let (fields, marker) = marker
        .split_once(" undocumented-methods=")
        .ok_or_else(|| "malformed Rust API member-documentation debt marker".to_string())?;
    let (methods, marker) = marker
        .split_once(" undocumented-associated-constants=")
        .ok_or_else(|| "malformed Rust API member-documentation debt marker".to_string())?;
    let (associated_constants, _) = marker
        .split_once(" -->")
        .ok_or_else(|| "malformed Rust API member-documentation debt marker".to_string())?;
    let expected = MemberDocDebt {
        variants: variants
            .parse()
            .map_err(|_| "invalid undocumented-variants count in Rust API marker".to_string())?,
        fields: fields
            .parse()
            .map_err(|_| "invalid undocumented-fields count in Rust API marker".to_string())?,
        methods: methods
            .parse()
            .map_err(|_| "invalid undocumented-methods count in Rust API marker".to_string())?,
        associated_constants: associated_constants.parse().map_err(|_| {
            "invalid undocumented-associated-constants count in Rust API marker".to_string()
        })?,
    };
    if actual != expected {
        return Err(format!(
            "supported re-export member-documentation debt changed: rendered rustdoc has {} undocumented variants, {} undocumented fields, {} undocumented methods, and {} undocumented associated constants; docs/rust-api.md records {}, {}, {}, and {}",
            actual.variants,
            actual.fields,
            actual.methods,
            actual.associated_constants,
            expected.variants,
            expected.fields,
            expected.methods,
            expected.associated_constants,
        ));
    }
    Ok(())
}

/// Resolve Cargo's complete workspace set by package identity rather than by
/// assuming a directory layout. `cargo metadata` already expands globs and
/// validates member manifests, so a future member under `tools/`, at the
/// repository root, or behind another valid member path cannot be skipped.
fn workspace_package_names_from_metadata(metadata: &[u8]) -> TaskResult<BTreeSet<String>> {
    let metadata: serde_json::Value = serde_json::from_slice(metadata)
        .map_err(|error| format!("cannot parse cargo metadata: {error}"))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its packages array".to_string())?;
    let members = metadata
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata omitted its workspace_members array".to_string())?;

    let mut package_names = BTreeMap::new();
    for package in packages {
        let id = package
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "cargo metadata contains a package without a string id".to_string())?;
        let name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("cargo metadata package `{id}` has no string package name"))?;
        if package_names.insert(id, name).is_some() {
            return Err(format!("cargo metadata repeats package id `{id}`"));
        }
    }

    let mut workspace_crates = BTreeSet::new();
    for member in members {
        let id = member.as_str().ok_or_else(|| {
            "cargo metadata contains a non-string workspace member id".to_string()
        })?;
        let name = package_names.get(id).ok_or_else(|| {
            format!("cargo metadata workspace member `{id}` has no matching package")
        })?;
        if !workspace_crates.insert((*name).to_string()) {
            return Err(format!(
                "cargo metadata resolves more than one workspace member to package `{name}`"
            ));
        }
    }
    if workspace_crates.is_empty() {
        return Err("cargo metadata did not report any workspace members".into());
    }
    Ok(workspace_crates)
}

fn check_rust_api_inventory_text(
    workspace_crates: &BTreeSet<String>,
    inventory: &str,
) -> TaskResult {
    let inventory_section = inventory
        .split_once("## Complete crate inventory")
        .map(|(_, section)| section)
        .and_then(|section| section.split_once("\n## ").map(|(section, _)| section))
        .ok_or_else(|| {
            "docs/rust-api.md must contain a bounded `Complete crate inventory` section".to_string()
        })?;

    let mut inventory_crates = BTreeMap::<&str, usize>::new();
    for line in inventory_section.lines() {
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let Some((crate_name, _)) = row.split_once("` |") else {
            continue;
        };
        *inventory_crates.entry(crate_name).or_default() += 1;
    }

    for crate_name in workspace_crates {
        let occurrences = inventory_crates
            .get(crate_name.as_str())
            .copied()
            .unwrap_or_default();
        if occurrences != 1 {
            return Err(format!(
                "docs/rust-api.md must classify workspace crate `{crate_name}` exactly once (found {occurrences})"
            ));
        }
    }
    let unexpected = inventory_crates
        .keys()
        .filter(|name| !workspace_crates.contains(**name))
        .copied()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(format!(
            "docs/rust-api.md classifies non-workspace crate row(s): {}",
            unexpected.join(", ")
        ));
    }
    Ok(())
}

/// Check the deliberately supported feature-minimal library surfaces in
/// isolation so workspace feature unification cannot silently enable them.
pub(crate) fn feature_check(args: &[String]) -> TaskResult {
    no_args("feature-check", args)?;
    let root = repo_root()?;
    require_minimal_feature_inventory(&root)?;
    for package in MINIMAL_FEATURE_PACKAGES {
        let mut command = pinned_cargo(&root)?;
        run(command.args([
            "check",
            "--package",
            package,
            "--lib",
            "--no-default-features",
            "--locked",
        ]))?;
    }
    Ok(())
}

fn require_minimal_feature_inventory(root: &Path) -> TaskResult {
    let mut command = pinned_cargo(root)?;
    let metadata =
        output(command.args(["metadata", "--format-version", "1", "--no-deps", "--locked"]))?;
    let metadata: Value = serde_json::from_slice(&metadata.stdout)
        .map_err(|error| format!("cannot parse cargo metadata: {error}"))?;
    validate_minimal_feature_inventory(&metadata)
}

/// Whether a `cargo metadata` feature entry is an extraction-only feature
/// that enables no other feature or dependency.
fn is_extraction_only_feature((name, enables): (&String, &Value)) -> bool {
    EXTRACTION_ONLY_FEATURES.contains(&name.as_str())
        && enables.as_array().is_some_and(Vec::is_empty)
}

fn validate_minimal_feature_inventory(metadata: &Value) -> TaskResult {
    let member_ids = metadata["workspace_members"]
        .as_array()
        .ok_or_else(|| "cargo metadata workspace_members is not an array".to_string())?
        .iter()
        .map(|id| {
            id.as_str()
                .map(ToString::to_string)
                .ok_or_else(|| "cargo metadata workspace member id is not a string".to_string())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| "cargo metadata packages is not an array".to_string())?;
    let mut seen_members = BTreeSet::new();
    let mut feature_packages = BTreeSet::new();
    for package in packages {
        let id = package["id"]
            .as_str()
            .ok_or_else(|| "cargo metadata package id is not a string".to_string())?;
        if !member_ids.contains(id) {
            continue;
        }
        seen_members.insert(id.to_string());
        let features = package["features"]
            .as_object()
            .ok_or_else(|| format!("workspace package {id} has no feature map"))?;
        if features.is_empty() || features.iter().all(is_extraction_only_feature) {
            continue;
        }
        let name = package["name"]
            .as_str()
            .ok_or_else(|| format!("workspace package {id} has no name"))?;
        if !feature_packages.insert(name.to_string()) {
            return Err(format!(
                "feature-bearing workspace package name is ambiguous: {name}"
            ));
        }
    }
    if seen_members != member_ids {
        let missing = member_ids.difference(&seen_members).collect::<Vec<_>>();
        return Err(format!(
            "cargo metadata workspace members are missing package records: {missing:?}"
        ));
    }

    let checked = MINIMAL_FEATURE_PACKAGES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    let tested = MINIMAL_FEATURE_TESTS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect::<BTreeSet<_>>();
    if checked != tested {
        return Err(format!(
            "minimal-feature check and test inventories differ: checks={checked:?}, tests={tested:?}"
        ));
    }
    if feature_packages != checked {
        return Err(format!(
            "feature-bearing workspace packages and minimal-feature inventory differ: cargo={feature_packages:?}, configured={checked:?}"
        ));
    }
    Ok(())
}

pub(crate) fn coverage_check(args: &[String]) -> TaskResult {
    no_args("coverage-check", args)?;
    coverage::check()
}

pub(crate) fn dependency_policy(args: &[String]) -> TaskResult {
    no_args("dependency-policy", args)?;
    let root = repo_root()?;
    require_cargo_deny(&root)?;
    let mut workspace = pinned_cargo(&root)?;
    run(workspace.args(["deny", "check"]))?;
    let mut runtime = pinned_cargo(&root)?;
    run(runtime.args([
        "deny",
        "--manifest-path",
        "crates/pinewasm_aot/runtime/Cargo.toml",
        "check",
        "-A",
        "license-not-encountered",
        "-A",
        "unmatched-skip",
    ]))?;
    licenses::run(&["--check".to_string()])
}

pub(crate) fn ci(args: &[String]) -> TaskResult {
    no_args("ci", args)?;
    check(&[])?;
    dependency_policy(&[])?;
    aot_runtime_all(&[])?;
    wasm(&[])?;
    wasm_test(&[])?;
    let root = repo_root()?;
    run(Command::new("git").current_dir(root).args([
        "diff",
        "--exit-code",
        "--",
        "crates/pinewasm_aot/runtime/pinewasm_aot_runtime.wasm",
        "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_standalone_wasm.wasm",
        "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_broker.wasm",
        "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_ta.wasm",
    ]))
}

pub(crate) fn release_preflight(args: &[String]) -> TaskResult {
    no_args("release-preflight", args)?;
    require_clean("release-check must start from a clean worktree")
}

pub(crate) fn release_check(args: &[String]) -> TaskResult {
    no_args("release-check", args)?;
    release_preflight(&[])?;
    // Release-only gates: the licensing wording must agree across documents,
    // the generated-artifact notice bundle must build and verify, and the
    // notice text must match the notice model. These gates are deliberately
    // absent from `ci` so pull requests stay green; only a release is blocked.
    policy::public_check(&[])?;
    ci(&[])?;
    artifact_notice::release_gate()?;
    leak_scan(&[])?;
    require_clean("release-check requires a clean worktree after every generator")
}

pub(crate) fn language_server(args: &[String]) -> TaskResult {
    no_args("language-server", args)?;
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command.args([
        "run",
        "--locked",
        "-p",
        "pinewasm_lsp",
        "--bin",
        "pinewasm",
        "--",
        "lsp",
        "--workspace",
        ".",
    ]))
}

pub(crate) fn check_file(args: &[String]) -> TaskResult {
    let [file] = args else {
        return Err("usage: check-file FILE".into());
    };
    let root = repo_root()?;
    let mut command = pinned_cargo(&root)?;
    run(command
        .args([
            "run",
            "--locked",
            "-p",
            "pinewasm_compiler",
            "--bin",
            "pinewasm-check",
            "--",
        ])
        .arg(file))
}

fn require_cargo_deny(root: &Path) -> TaskResult {
    let mut command = pinned_cargo(root)?;
    let result = output_allow_failure(command.args(["deny", "--version"]))?;
    let expected = format!("cargo-deny {CARGO_DENY_VERSION}");
    if !result.status.success() || String::from_utf8_lossy(&result.stdout).trim() != expected {
        return Err(format!(
            "cargo-deny {CARGO_DENY_VERSION} is required; install it with `cargo install cargo-deny --version {CARGO_DENY_VERSION} --locked`"
        ));
    }
    Ok(())
}

fn require_clean(message: &str) -> TaskResult {
    let root = repo_root()?;
    let status = output(Command::new("git").current_dir(&root).args([
        "status",
        "--porcelain=v1",
        "--untracked-files=normal",
        "--ignore-submodules",
    ]))?;
    if status.stdout.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{message}:\n{}",
        String::from_utf8_lossy(&status.stdout).trim_end()
    ))
}

fn tool_prefix(root: &Path, tool: &str, version: &str) -> PathBuf {
    root.join(".tools").join(tool).join(version).join(format!(
        "{}-{}",
        env::consts::OS,
        env::consts::ARCH
    ))
}

fn utf8_path(path: &Path) -> TaskResult<&str> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

fn no_args(command: &str, args: &[String]) -> TaskResult {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("usage: {command}"))
    }
}

fn source_snapshot(root: &Path) -> TaskResult<BTreeMap<PathBuf, (u64, u128)>> {
    let mut snapshot = BTreeMap::new();
    for file in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        record_source(root, &root.join(file), &mut snapshot)?;
    }
    collect_sources(root, &root.join("crates"), &mut snapshot)?;
    Ok(snapshot)
}

fn collect_sources(
    root: &Path,
    directory: &Path,
    snapshot: &mut BTreeMap<PathBuf, (u64, u128)>,
) -> TaskResult {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_sources(root, &entry.path(), snapshot)?;
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs" || extension == "toml")
        {
            record_source(root, &entry.path(), snapshot)?;
        }
    }
    Ok(())
}

fn record_source(
    root: &Path,
    file: &Path,
    snapshot: &mut BTreeMap<PathBuf, (u64, u128)>,
) -> TaskResult {
    let metadata = fs::metadata(file)
        .map_err(|error| format!("cannot inspect {}: {error}", file.display()))?;
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let relative = file.strip_prefix(root).unwrap_or(file).to_path_buf();
    snapshot.insert(relative, (metadata.len(), modified));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        cfg_test_only, check_identity_manifest, check_member_doc_debt_marker,
        check_originating_contract, check_originating_description, check_rust_api_inventory,
        check_rust_api_inventory_text, check_supported_reexport_dependency,
        configure_adversarial_command, configure_adversarial_test_command,
        crate_inherent_member_inventories, crate_inherent_member_inventories_with_providers,
        crate_syntax_context, declared_macro_providers_from_package,
        direct_facade_member_inventories_from_source, impl_type_name, inventory_source_members,
        member_doc_debt, module_has_local_macro_glob, module_has_unresolved_macro_glob,
        module_macro_glob_is_ambiguous, normalize_rustdoc_docblock, output,
        reject_nested_member_items, reject_nested_member_items_with_context,
        reject_trait_surface_escape, repo_root, require_exact_ignored_test,
        require_unique_adversarial_tests, supported_facade_root_items_from_source,
        supported_reexports_from_source, trusted_macro_providers_from_metadata,
        validate_minimal_feature_inventory, workspace_libraries_from_metadata,
        workspace_package_names_from_metadata, IsolatedCargo, MemberDocDebt, SourceMemberInventory,
        ADVERSARIAL_SMOKE_CASES, ADVERSARIAL_TESTS, CRATES_IO_REGISTRY_SOURCE,
        MINIMAL_FEATURE_PACKAGES, PROPTEST_ENVIRONMENT,
    };
    use crate::command::{
        pinned_fixture_cargo, pinned_toolchain, reject_untrusted_cargo_sources,
        remove_cargo_build_overrides, sha256, temp_dir,
    };
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, BTreeSet};
    use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use syn::Item;

    #[test]
    fn adversarial_smoke_ignores_hostile_property_configuration() {
        let mut command = Command::new("cargo");
        for variable in PROPTEST_ENVIRONMENT {
            command.env(variable, "hostile");
        }
        command.env("PINEWASM_ADVERSARIAL_CASES", "100000");

        configure_adversarial_command(&mut command);

        for variable in PROPTEST_ENVIRONMENT {
            let value = command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(variable))
                .unwrap_or_else(|| panic!("{variable} should have an explicit removal"))
                .1;
            assert_eq!(value, None, "{variable} must not reach the smoke tests");
        }
        let cases = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("PINEWASM_ADVERSARIAL_CASES"))
            .and_then(|(_, value)| value);
        assert_eq!(cases, Some(OsStr::new(ADVERSARIAL_SMOKE_CASES)));
    }

    #[test]
    fn adversarial_smoke_requires_each_exact_ignored_test_identity() {
        let expected = "tests::adversarial_binding_dto_deserialization_is_total";
        assert_eq!(
            require_exact_ignored_test(format!("{expected}: test\n").as_bytes(), expected),
            Ok(())
        );

        let missing = require_exact_ignored_test(b"0 tests, 0 benchmarks\n", expected)
            .expect_err("a zero-match libtest listing must fail closed");
        assert!(missing.contains("found []"), "unexpected error: {missing}");

        let renamed = require_exact_ignored_test(
            b"tests::renamed_adversarial_binding_test: test\n",
            expected,
        )
        .expect_err("a renamed ignored test must fail closed");
        assert!(renamed.contains("renamed_adversarial_binding_test"));
    }

    #[test]
    fn adversarial_smoke_manifest_constructs_exact_test_filters() {
        assert_eq!(ADVERSARIAL_TESTS.len(), 7);
        assert_eq!(require_unique_adversarial_tests(&ADVERSARIAL_TESTS), Ok(()));
        for test in ADVERSARIAL_TESTS {
            let mut command = Command::new("cargo");
            configure_adversarial_test_command(&mut command, test);
            command.args(["--", "--ignored", "--exact", test.name, "--test-threads=1"]);
            let args: Vec<_> = command.get_args().collect();
            let exact = args
                .windows(2)
                .any(|pair| pair == [OsStr::new("--exact"), OsStr::new(test.name)]);
            assert!(exact, "{} must be selected by exact identity", test.name);
        }

        let duplicate = [ADVERSARIAL_TESTS[0], ADVERSARIAL_TESTS[0]];
        assert!(require_unique_adversarial_tests(&duplicate).is_err());
    }

    #[test]
    fn charon_rename_is_allowlisted_only_in_its_exact_cfg_attr_shape() {
        use super::is_charon_rename_cfg_attr;
        use syn::{punctuated::Punctuated, Meta, Token};
        let accepted = [
            "#[cfg_attr(charon, charon::rename(\"DispatchChargeLimit\"))] pub(crate) enum E { A }",
            "#[cfg_attr(all(charon, feature = \"aeneas-color-clone\"), charon::rename(\"CloneColorValue\"))] pub enum E { A }",
        ];
        for source in accepted {
            let item: Item = syn::parse_str(source).expect("fixture item must parse");
            assert_eq!(
                reject_nested_member_items(&item, Path::new("fixture.rs")),
                Ok(()),
                "{source}"
            );
        }
        // Even a synthetic syntax tree must not admit an attributed literal.
        let mut nested = syn::parse_str::<Meta>(
            "cfg_attr(all(charon, feature = \"aeneas-color-clone\"), charon::rename(\"X\"))",
        )
        .expect("fixture meta");
        let Meta::List(ref mut cfg) = nested else {
            unreachable!()
        };
        let mut terms = cfg
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .unwrap();
        let Meta::List(ref mut all) = terms[0] else {
            unreachable!()
        };
        let mut predicates = all
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .unwrap();
        let Meta::NameValue(ref mut feature) = predicates[1] else {
            unreachable!()
        };
        let syn::Expr::Lit(ref mut literal) = feature.value else {
            unreachable!()
        };
        literal.attrs.push(syn::parse_quote!(#[hostile::conceal]));
        use quote::ToTokens;
        all.tokens = predicates.to_token_stream();
        // Parsing this malformed predicate also fails closed.
        assert!(!is_charon_rename_cfg_attr(&terms));
        let rejected = [
            "#[charon::rename(\"X\")] pub enum E { A }",
            "#[cfg_attr(not(charon), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(feature = \"charon\", charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(charon, charon::rename(\"X\"), hostile::conceal)] pub enum E { A }",
            "#[cfg_attr(charon, charon::rename(X))] pub enum E { A }",
            "#[cfg_attr(charon, charon::rename[\"X\"])] pub enum E { A }",
            "#[cfg_attr(charon, ::charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(charon, charon::opaque)] pub enum E { A }",
            "#[cfg_attr(feature = \"aeneas-color-clone\", charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(any(charon, feature = \"aeneas-color-clone\"), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(not(charon), feature = \"aeneas-color-clone\"), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(charon, feature = \"unlisted\"), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(charon), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(charon, feature = true), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(charon, feature = \"aeneas-color-clone\", unix), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(::charon, feature = \"aeneas-color-clone\"), charon::rename(\"X\"))] pub enum E { A }",
            "#[cfg_attr(all(charon, feature = \"aeneas-color-clone\"), charon::rename(\"X\"), hostile::conceal)] pub enum E { A }",
        ];
        for source in rejected {
            let item: Item = syn::parse_str(source).expect("fixture item must parse");
            let error = reject_nested_member_items(&item, Path::new("fixture.rs"))
                .expect_err("an unlisted charon attribute shape must fail closed");
            assert!(
                error.contains("attribute macro candidate"),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn charon_rename_cfg_attr_is_erased_by_the_pinned_compiler() {
        // Compiler-level positive control: without `cfg(charon)` the allowlisted
        // attribute expands to nothing, so Rustdoc of the item is identical
        // with and without it.
        let fixture = temp_dir("charon-rename-erasure").expect("temporary fixture directory");
        let render = |name: &str, attribute: &str, feature_enabled: bool| {
            // Same file name in each variant so source links match.
            let directory = fixture.path().join(name);
            fs::create_dir_all(&directory).expect("create fixture variant directory");
            let source = directory.join("lib.rs");
            fs::write(
                &source,
                format!(
                    "//! Fixture.\n\n/// Which ceiling.\n{attribute}\npub enum ChargeLimit {{\n    /// Bar.\n    Bar,\n    /// Run.\n    Run,\n}}\n"
                ),
            )
            .expect("write fixture source");
            let docs = directory.join("docs");
            let mut command = pinned_rust_tool("rustdoc", fixture.path());
            command
                .args([
                    "--crate-name",
                    "fixture",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg("--check-cfg=cfg(charon)")
                .arg("--check-cfg=cfg(feature,values(\"aeneas-color-clone\"))")
                .arg("-Dwarnings");
            if feature_enabled {
                command.arg("--cfg=feature=\"aeneas-color-clone\"");
            }
            command.arg(&source).arg("--out-dir").arg(&docs);
            assert_command_succeeds(&mut command, "charon rename fixture Rustdoc build");
            fs::read_to_string(docs.join("fixture").join("enum.ChargeLimit.html"))
                .expect("read generated Rustdoc page")
        };
        let plain = render("plain", "", false);
        let renamed = render(
            "renamed",
            "#[cfg_attr(charon, charon::rename(\"DispatchChargeLimit\"))]",
            false,
        );
        assert_eq!(plain, renamed);
        assert!(!renamed.contains("DispatchChargeLimit"));
        for (name, enabled) in [("dedicated-disabled", false), ("dedicated-enabled", true)] {
            let dedicated = render(
                name,
                "#[cfg_attr(all(charon, feature = \"aeneas-color-clone\"), charon::rename(\"CloneColorValue\"))]",
                enabled,
            );
            assert_eq!(plain, dedicated);
            assert!(!dedicated.contains("CloneColorValue"));
        }
    }

    fn fixture_toolchain() -> String {
        std::env::var("PINEWASM_FIXTURE_RUST_TOOLCHAIN").unwrap_or_else(|_| {
            let root = repo_root().expect("repository root must be discoverable");
            pinned_toolchain(&root).expect("repository toolchain must resolve")
        })
    }

    fn pinned_rust_tool(tool: &str, working_directory: &Path) -> Command {
        // The override lets a developer replay compiler-level fixtures on the
        // declared MSRV without making every ordinary CI job install it.
        let toolchain = fixture_toolchain();
        let mut command = Command::new("rustup");
        command
            .current_dir(working_directory)
            .args(["run", &toolchain, tool]);
        command
    }

    fn pinned_fixture_cargo_command(working_directory: &Path) -> Command {
        let mut command = pinned_fixture_cargo(working_directory, &fixture_toolchain())
            .expect("fixture Cargo command must resolve");
        remove_cargo_build_overrides(&mut command);
        command
    }

    fn isolated_fixture_cargo(working_directory: &Path, cargo_home: &Path) -> Command {
        let mut command = pinned_fixture_cargo_command(working_directory);
        command.env("CARGO_HOME", cargo_home);
        command
    }

    fn assert_command_succeeds(command: &mut Command, context: &str) {
        let output = command.output().expect("Rust tool must start");
        assert!(
            output.status.success(),
            "{context} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_directory_source_package(
        directory: &Path,
        package: &str,
        version: &str,
        manifest_suffix: &str,
        library: &str,
    ) {
        let package_root = directory.join(format!("{package}-{version}"));
        fs::create_dir_all(package_root.join("src")).expect("create directory-source package");
        let manifest = format!(
            "[package]\nname = \"{package}\"\nversion = \"{version}\"\nedition = \"2021\"\n{manifest_suffix}"
        );
        fs::write(package_root.join("Cargo.toml"), &manifest)
            .expect("write directory-source manifest");
        fs::write(package_root.join("src/lib.rs"), library)
            .expect("write directory-source library");
        let checksum = serde_json::json!({
            "files": {
                "Cargo.toml": sha256(manifest.as_bytes()),
                "src/lib.rs": sha256(library.as_bytes()),
            },
            // Cargo's own documentation explicitly says this directory-source
            // metadata detects accidents, not malicious substitution. The
            // fixture intentionally supplies its own plausible value.
            "package": "0000000000000000000000000000000000000000000000000000000000000000",
        });
        fs::write(
            package_root.join(".cargo-checksum.json"),
            serde_json::to_vec(&checksum).expect("serialize directory-source checksum"),
        )
        .expect("write directory-source checksum");
    }

    #[test]
    fn rust_api_inventory_classifies_every_workspace_member() {
        let root = repo_root().expect("repository root must be discoverable");
        let cargo = IsolatedCargo::new(&root).expect("fresh isolated Cargo state");
        check_rust_api_inventory(&root, &cargo)
            .expect("Rust API inventory must cover the workspace");
    }

    #[test]
    fn isolated_cargo_uses_fresh_home_target_and_configuration_root() {
        let root = repo_root().expect("repository root must be discoverable");
        let cargo = IsolatedCargo::new(&root).expect("fresh isolated Cargo state");
        let command = cargo.command(&root).expect("construct isolated Cargo");
        let environment = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            command.get_current_dir(),
            Some(cargo.working_directory.as_path())
        );
        assert!(cargo.working_directory.is_absolute());
        assert!(cargo.cargo_home.is_absolute());
        assert!(cargo.target_directory.is_absolute());
        assert_eq!(
            environment.get(OsStr::new("CARGO_HOME")),
            Some(&Some(cargo.cargo_home.as_os_str().to_os_string()))
        );
        assert_eq!(
            environment.get(OsStr::new("CARGO_TARGET_DIR")),
            Some(&Some(cargo.target_directory.as_os_str().to_os_string()))
        );
        for variable in [
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_ENCODED_RUSTDOCFLAGS",
            "RUSTFLAGS",
            "RUSTC_BOOTSTRAP",
            "RUSTDOCFLAGS",
        ] {
            assert_eq!(
                environment.get(OsStr::new(variable)),
                Some(&None),
                "{variable} must not alter the fresh isolated build"
            );
        }
        for directory in [&cargo.cargo_home, &cargo.target_directory] {
            assert!(
                fs::read_dir(directory)
                    .expect("read fresh isolated directory")
                    .next()
                    .is_none(),
                "{} must not reuse mutable build state",
                directory.display()
            );
        }
    }

    #[test]
    fn isolated_cargo_ignores_repository_config_created_after_preflight() {
        let fixture = temp_dir("rust-api-config-swap").expect("temporary fixture directory");
        let root = fixture.path().join("repository");
        fs::create_dir_all(root.join("src")).expect("create fixture source directory");
        fs::write(
            root.join("rust-toolchain.toml"),
            include_str!("../../../rust-toolchain.toml"),
        )
        .expect("write toolchain pin");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'isolated-config-swap'\nversion = '0.0.0'\nedition = '2021'\n[workspace]\n",
        )
        .expect("write fixture manifest");
        fs::write(
            root.join("Cargo.lock"),
            "version = 3\n[[package]]\nname = 'isolated-config-swap'\nversion = '0.0.0'\n",
        )
        .expect("write fixture lockfile");
        fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n")
            .expect("write fixture library");
        let root = root
            .canonicalize()
            .expect("canonicalize fixture repository");

        let cargo = IsolatedCargo::new(&root).expect("preflight clean fixture");
        let mut command = cargo
            .command(&root)
            .expect("construct isolated Cargo command");
        command
            .arg("check")
            .arg("--manifest-path")
            .arg(root.join("Cargo.toml"))
            .args(["--offline", "--locked"]);

        // Simulate the original check-to-spawn window. Cargo discovers config
        // from its isolated working directory, not from the manifest path, so
        // even a newly created repository config cannot redirect the launch.
        fs::create_dir(root.join(".cargo")).expect("create late Cargo configuration directory");
        fs::write(
            root.join(".cargo/config.toml"),
            "[source.crates-io\nreplace-with = 'hostile'\n",
        )
        .expect("write deliberately invalid late Cargo configuration");

        let result = output(&mut command)
            .expect("isolated Cargo must not discover the late repository configuration");
        assert!(result.status.success());
        assert!(cargo.target_directory.join("debug").is_dir());
    }

    #[test]
    fn a_workspace_member_outside_crates_cannot_pass_unclassified() {
        let metadata = br#"{
            "packages": [
                {
                    "id": "path+file:///repo/crates/pinewasm_compiler#0.1.0",
                    "name": "pinewasm_compiler"
                },
                {
                    "id": "path+file:///repo/tools/release_helper#0.1.0",
                    "name": "release_helper"
                }
            ],
            "workspace_members": [
                "path+file:///repo/crates/pinewasm_compiler#0.1.0",
                "path+file:///repo/tools/release_helper#0.1.0"
            ]
        }"#;
        let workspace_crates =
            workspace_package_names_from_metadata(metadata).expect("metadata must parse");
        assert!(workspace_crates.contains("release_helper"));

        let inventory = r"
## Complete crate inventory

| Crate | Classification |
| --- | --- |
| `pinewasm_compiler` | **Supported Rust facade** |

## Supported facade inventory
";
        let error = check_rust_api_inventory_text(&workspace_crates, inventory)
            .expect_err("the out-of-layout member must require classification");
        assert_eq!(
            error,
            "docs/rust-api.md must classify workspace crate `release_helper` exactly once (found 0)"
        );
    }

    #[test]
    fn grouped_reexport_override_cannot_impersonate_originating_docs() {
        let origins =
            BTreeSet::from(["OHLCV bar data plus the time axis and symbol metadata.".to_string()]);
        let error = check_originating_description(
            "MarketData",
            "Host inputs, execution controls, outputs, and structured runtime failures.",
            &origins,
        )
        .expect_err("a generic re-export override must be rejected");
        assert!(error.contains("pinewasm_compiler::MarketData overrides"));

        let full_origin = BTreeSet::from(["<p>Exact lead.</p><p>Critical limits.</p>".to_string()]);
        let error = check_originating_contract(
            "MarketData",
            "item docblock",
            "<p>Exact lead.</p>",
            &full_origin,
        )
        .expect_err("copying only the lead paragraph must not mask doc erasure");
        assert!(error.contains("item docblock"));

        assert_eq!(
            normalize_rustdoc_docblock(
                r#"<a href="struct.X.html" title="struct crate_a::X"><code>X</code></a>"#
            ),
            r"<code>X</code>"
        );
    }

    #[test]
    fn supported_reexport_parser_is_exact_and_rejects_globs() {
        let source = r"
pub use pinewasm_csil::CsilProgram as Program;
#[doc(inline)]
pub use pinewasm_vm::{MarketData, nested::RuntimeError as Error};
";
        let parsed = supported_reexports_from_source(source).expect("explicit exports must parse");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].public_name, "Program");
        assert_eq!(parsed[0].origin_name, "CsilProgram");
        assert_eq!(parsed[2].public_name, "Error");
        assert_eq!(parsed[2].origin_modules, ["nested"]);

        let error = supported_reexports_from_source("pub use pinewasm_vm::*;")
            .expect_err("a glob must not silently expand the supported surface");
        assert!(error.contains("globbed supported re-export"));

        let error = supported_reexports_from_source("pub mod hidden_surface;")
            .expect_err("a public child module must not evade the root inventory");
        assert!(error.contains("explicit recursive inventory support"));

        for (competing_binding, expected) in [
            (
                "mod r#pinewasm_vm {}",
                "bare supported re-export origin `pinewasm_vm` may resolve",
            ),
            (
                "use alternate_vm as r#pinewasm_vm;",
                "bare supported re-export origin `pinewasm_vm` may resolve",
            ),
            (
                "extern crate alternate_vm as r#pinewasm_vm;",
                "shadowed by a renamed `extern crate` binding",
            ),
        ] {
            let error = supported_reexports_from_source(&format!(
                "{competing_binding}\npub use pinewasm_vm::MarketData;\n"
            ))
            .expect_err("a bare origin with a competing root binding must fail closed");
            assert!(
                error.contains(expected),
                "unexpected competing-root rejection: {error}"
            );
        }

        let error = supported_reexports_from_source(
            "use alternate_vm::*;\npub use pinewasm_vm::MarketData;\n",
        )
        .expect_err("a root glob makes a bare supported re-export origin ambiguous");
        assert!(error.contains("crate-root glob import"));

        supported_reexports_from_source(
            "mod r#pinewasm_vm {}\npub use ::r#pinewasm_vm::r#MarketData;\n",
        )
        .expect("a leading-colon external origin remains unambiguous");

        let error = supported_reexports_from_source(
            "extern crate alternate_vm as r#pinewasm_vm;\npub use ::r#pinewasm_vm::MarketData;\n",
        )
        .expect_err("a renamed extern crate also shadows a leading-colon origin");
        assert!(error.contains("shadowed by a renamed `extern crate` binding"));
    }

    #[test]
    fn compiled_reexport_resolution_distinguishes_bare_and_absolute_roots() {
        let fixture =
            temp_dir("rust-api-reexport-resolution").expect("temporary fixture directory");
        let external = fixture.path().join("external.rs");
        let external_library = fixture.path().join("libpinewasm_vm.rlib");
        let alternate = fixture.path().join("alternate.rs");
        let alternate_library = fixture.path().join("libalternate_vm.rlib");
        fs::write(
            &external,
            r"
/// External example data.
pub struct MarketData {
    /// Externally visible field.
    pub visible: u32,
}
",
        )
        .expect("write external re-export provider");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "pinewasm_vm",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&external)
                .arg("-o")
                .arg(&external_library),
            "external re-export provider build",
        );
        fs::write(
            &alternate,
            r"
pub struct MarketData {
    pub aliased_only: u32,
}
",
        )
        .expect("write renamed external provider");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "alternate_vm",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&alternate)
                .arg("-o")
                .arg(&alternate_library),
            "renamed external provider build",
        );

        let bare_source = r"
mod r#pinewasm_vm {
    pub struct MarketData {
        pub visible: u32,
        pub local_only: u32,
        #[r#doc(r#hidden)]
        pub concealed: u32,
    }
}

#[doc(inline)]
pub use r#pinewasm_vm::MarketData;

pub fn proof() -> u32 {
    let data = MarketData { visible: 1, local_only: 2, concealed: 3 };
    data.visible + data.local_only + data.concealed
}
";
        let absolute_source = r"
mod r#pinewasm_vm {
    pub struct MarketData {
        pub local_only: u32,
    }
}

#[doc(inline)]
pub use ::r#pinewasm_vm::r#MarketData;

pub fn proof() -> u32 {
    let data = MarketData { visible: 7 };
    data.visible
}
";

        for (label, source, selects_local_type) in [
            ("bare", bare_source, true),
            ("absolute", absolute_source, false),
        ] {
            let consumer = fixture.path().join(format!("{label}.rs"));
            let library = fixture.path().join(format!("lib{label}.rlib"));
            let docs = fixture.path().join(format!("{label}-doc"));
            fs::write(&consumer, source).expect("write re-export consumer");
            assert_command_succeeds(
                pinned_rust_tool("rustc", fixture.path())
                    .arg("--crate-name")
                    .arg(format!("{label}_consumer"))
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer)
                    .arg("--extern")
                    .arg(format!("pinewasm_vm={}", external_library.display()))
                    .arg("-o")
                    .arg(&library),
                &format!("{label} re-export consumer build"),
            );
            fs::create_dir_all(&docs).expect("create re-export Rustdoc directory");
            assert_command_succeeds(
                pinned_rust_tool("rustdoc", fixture.path())
                    .arg("--crate-name")
                    .arg(format!("{label}_consumer"))
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer)
                    .arg("--extern")
                    .arg(format!("pinewasm_vm={}", external_library.display()))
                    .arg("--out-dir")
                    .arg(&docs),
                &format!("{label} re-export consumer Rustdoc build"),
            );
            let html = fs::read_to_string(
                docs.join(format!("{label}_consumer"))
                    .join("struct.MarketData.html"),
            )
            .expect("read re-export Rustdoc page");
            assert!(html.contains("structfield.visible"));
            assert_eq!(
                html.contains("structfield.local_only"),
                selects_local_type,
                "Rustdoc must prove which identically named type rustc resolved"
            );
            assert!(
                !html.contains("structfield.concealed"),
                "normal Rustdoc must omit the local doc-hidden field"
            );
        }

        let error = supported_reexports_from_source(bare_source)
            .expect_err("the bare origin must not impersonate the external provider");
        assert!(error.contains("bare supported re-export origin `pinewasm_vm` may resolve"));
        let absolute = supported_reexports_from_source(absolute_source)
            .expect("the leading-colon origin must select the external provider");
        assert_eq!(absolute.len(), 1);
        assert_eq!(absolute[0].origin_crate, "pinewasm_vm");
        assert_eq!(absolute[0].origin_name, "MarketData");

        let alias_source = r"
extern crate alternate_vm as r#pinewasm_vm;

#[doc(inline)]
pub use ::r#pinewasm_vm::MarketData;

pub fn proof() -> u32 {
    let data = MarketData { aliased_only: 9 };
    data.aliased_only
}
";
        let alias_consumer = fixture.path().join("extern_alias.rs");
        let alias_docs = fixture.path().join("extern-alias-doc");
        fs::write(&alias_consumer, alias_source).expect("write extern-alias consumer");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "extern_alias_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&alias_consumer)
                .arg("--extern")
                .arg(format!("pinewasm_vm={}", external_library.display()))
                .arg("--extern")
                .arg(format!("alternate_vm={}", alternate_library.display())),
            "renamed extern-crate consumer build",
        );
        fs::create_dir_all(&alias_docs).expect("create extern-alias Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "extern_alias_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&alias_consumer)
                .arg("--extern")
                .arg(format!("pinewasm_vm={}", external_library.display()))
                .arg("--extern")
                .arg(format!("alternate_vm={}", alternate_library.display()))
                .arg("--out-dir")
                .arg(&alias_docs),
            "renamed extern-crate consumer Rustdoc build",
        );
        let html = fs::read_to_string(
            alias_docs
                .join("extern_alias_consumer")
                .join("struct.MarketData.html"),
        )
        .expect("read extern-alias Rustdoc page");
        assert!(html.contains("structfield.aliased_only"));
        assert!(!html.contains("structfield.visible"));
        let error = supported_reexports_from_source(alias_source)
            .expect_err("a renamed extern crate must not impersonate the allowlisted provider");
        assert!(error.contains("shadowed by a renamed `extern crate` binding"));
    }

    #[test]
    fn supported_reexport_dependency_matches_inventoried_workspace_package() {
        let fixture =
            temp_dir("rust-api-reexport-dependency").expect("temporary fixture directory");
        for member in ["compiler", "real", "alternate"] {
            fs::create_dir_all(fixture.path().join(member).join("src"))
                .expect("create workspace member source directory");
        }
        fs::write(
            fixture.path().join("Cargo.toml"),
            "[workspace]\nmembers = ['compiler', 'real', 'alternate']\nresolver = '2'\n",
        )
        .expect("write dependency-resolution workspace");
        fs::write(
            fixture.path().join("real/Cargo.toml"),
            "[package]\nname = 'pinewasm_vm'\nversion = '0.0.0'\nedition = '2021'\n",
        )
        .expect("write provider manifest");
        fs::write(
            fixture.path().join("real/src/lib.rs"),
            r"
/// Example record for the fixture.
pub struct MarketData {
    /// Visible field.
    pub visible: u32,
}
",
        )
        .expect("write provider source");
        fs::write(
            fixture.path().join("alternate/Cargo.toml"),
            "[package]\nname = 'alternate_vm'\nversion = '0.0.0'\nedition = '2021'\n",
        )
        .expect("write alternate provider manifest");
        fs::write(
            fixture.path().join("alternate/src/lib.rs"),
            r"
/// Example record for the fixture.
pub struct MarketData {
    /// Visible field.
    pub visible: u32,
    #[doc(hidden)]
    pub concealed: u32,
}
",
        )
        .expect("write alternate provider source");
        fs::write(
            fixture.path().join("compiler/Cargo.toml"),
            r#"[package]
name = "pinewasm_compiler"
version = "0.0.0"
edition = "2021"

[dependencies.pinewasm_vm]
package = "alternate_vm"
path = "../alternate"
"#,
        )
        .expect("write renamed compiler dependency");
        let compiler_source = fixture.path().join("compiler/src/lib.rs");
        fs::write(
            &compiler_source,
            r"
#[doc(inline)]
pub use pinewasm_vm::MarketData;

pub fn proof() -> u32 {
    let data = MarketData { visible: 1, concealed: 2 };
    data.visible + data.concealed
}
",
        )
        .expect("write renamed-dependency compiler source");

        assert_command_succeeds(
            pinned_fixture_cargo_command(fixture.path()).args(["generate-lockfile", "--offline"]),
            "renamed-dependency lockfile",
        );
        assert_command_succeeds(
            pinned_fixture_cargo_command(fixture.path()).args([
                "build",
                "--workspace",
                "--locked",
                "--offline",
            ]),
            "renamed-dependency callable build",
        );
        assert_command_succeeds(
            pinned_fixture_cargo_command(fixture.path()).args([
                "doc",
                "-p",
                "pinewasm_compiler",
                "--no-deps",
                "--locked",
                "--offline",
            ]),
            "renamed-dependency Rustdoc build",
        );
        let html = fs::read_to_string(
            fixture
                .path()
                .join("target/doc/pinewasm_compiler/struct.MarketData.html"),
        )
        .expect("read renamed-dependency facade Rustdoc page");
        assert!(html.contains("structfield.visible"));
        assert!(
            !html.contains("structfield.concealed"),
            "normal Rustdoc must omit the alternate package's callable hidden field"
        );

        let metadata = pinned_fixture_cargo_command(fixture.path())
            .args([
                "metadata",
                "--format-version",
                "1",
                "--all-features",
                "--locked",
                "--offline",
            ])
            .output()
            .expect("renamed-dependency Cargo metadata must start");
        assert!(
            metadata.status.success(),
            "renamed-dependency Cargo metadata failed: {}",
            String::from_utf8_lossy(&metadata.stderr)
        );
        let libraries = workspace_libraries_from_metadata(&metadata.stdout)
            .expect("renamed-dependency metadata must describe workspace libraries");
        let reexport = supported_reexports_from_source(
            &fs::read_to_string(&compiler_source).expect("read compiler source"),
        )
        .expect("renamed-dependency re-export must parse")
        .remove(0);
        let canonical_root = fixture
            .path()
            .canonicalize()
            .expect("canonicalize dependency-resolution workspace");
        let selected = super::source_member_inventory(&canonical_root, &libraries, &reexport)
            .expect("the source-only census must accept the renamed dependency");
        assert_eq!(
            selected.member_identities,
            BTreeSet::from(["field visible".to_string()]),
            "the source-only census must select the unrelated package"
        );
        let metadata_value: serde_json::Value = serde_json::from_slice(&metadata.stdout)
            .expect("renamed-dependency metadata must be JSON");
        let nodes = metadata_value["resolve"]["nodes"]
            .as_array()
            .expect("metadata resolved nodes");
        let compiler = libraries
            .get("pinewasm_compiler")
            .expect("compiler workspace library");
        let error = check_supported_reexport_dependency(nodes, compiler, &libraries, &reexport)
            .expect_err("the actual compiler dependency must match the inventoried package");
        assert!(
            error.contains("resolves package") && error.contains("allowlisted workspace library"),
            "unexpected dependency-identity rejection: {error}"
        );
    }

    #[test]
    fn compiler_root_manifest_rejects_unlisted_public_items() {
        let source = r"
pub use pinewasm_vm::MarketData;
/// Deliberately supported entry point.
pub fn compile() {}
/// Deliberately supported limit.
pub const LIMIT: usize = 1;
";
        let expected = supported_facade_root_items_from_source(source)
            .expect("explicit compiler-root items must parse");
        assert_eq!(
            expected,
            BTreeSet::from([
                "const LIMIT".to_string(),
                "fn compile".to_string(),
                "reexport MarketData = pinewasm_vm::MarketData".to_string(),
            ])
        );
        let inventory = r"
<!-- rust-api-supported-root-items-begin -->
```text
const LIMIT
fn compile
reexport MarketData = pinewasm_vm::MarketData
```
<!-- rust-api-supported-root-items-end -->
";
        check_identity_manifest(
            inventory,
            "rust-api-supported-root-items",
            &expected,
            "supported compiler-root item",
        )
        .expect("exact manifest must pass");

        let expanded = supported_facade_root_items_from_source(&format!(
            "{source}\n/// New, documented, but not in the manifest.\npub fn surprise() {{}}\n"
        ))
        .expect("expanded source must parse");
        let error = check_identity_manifest(
            inventory,
            "rust-api-supported-root-items",
            &expanded,
            "supported compiler-root item",
        )
        .expect_err("new public root item must require manifest review");
        assert!(error.contains("fn surprise"));

        let replaced = supported_facade_root_items_from_source(
            &source.replace("pinewasm_vm::MarketData", "pinewasm_other::MarketData"),
        )
        .expect("replacement origin must still parse as a facade");
        let error = check_identity_manifest(
            inventory,
            "rust-api-supported-root-items",
            &replaced,
            "supported compiler-root item",
        )
        .expect_err("changing a re-export origin must require manifest review");
        assert!(error.contains("pinewasm_other::MarketData"));
        assert!(error.contains("pinewasm_vm::MarketData"));

        let exported_macro =
            format!("{source}\n#[macro_export]\nmacro_rules! surprise {{ () => {{}}; }}\n");
        let error = supported_facade_root_items_from_source(&exported_macro)
            .expect_err("an exported macro must not evade the root inventory");
        assert!(error.contains("expansion-aware facade inventory"));

        let generated_item = format!("{source}\nmake_public_item!();\n");
        let error = supported_facade_root_items_from_source(&generated_item)
            .expect_err("a root macro invocation may generate public API");
        assert!(error.contains("macro `make_public_item`"));
    }

    #[test]
    fn compiler_root_manifest_pins_direct_type_alias_target() {
        let original = r"
pub use pinewasm_vm::MarketData;
/// Deliberately supported callback alias.
pub type Callback<'a> = &'a dyn Fn() -> f64;
";
        let expected = supported_facade_root_items_from_source(original)
            .expect("direct type alias must be inventoried");
        let alias = expected
            .iter()
            .find(|item| item.starts_with("type Callback"))
            .expect("alias identity must be present");
        assert!(
            alias.contains("'a"),
            "generic lifetime must be pinned: {alias}"
        );
        assert!(
            alias.contains("dyn Fn") && alias.contains("f64"),
            "alias target must be pinned: {alias}"
        );

        let inventory = format!(
            "<!-- rust-api-supported-root-items-begin -->\n```text\n{}\n```\n<!-- rust-api-supported-root-items-end -->\n",
            expected.iter().cloned().collect::<Vec<_>>().join("\n")
        );
        let replaced = supported_facade_root_items_from_source(
            &original.replace("&'a dyn Fn() -> f64", "String"),
        )
        .expect("replacement alias target must still parse");
        let error = check_identity_manifest(
            &inventory,
            "rust-api-supported-root-items",
            &replaced,
            "supported compiler-root item",
        )
        .expect_err("same-name alias target replacement must require review");
        assert!(
            error.contains("String"),
            "replacement target is named: {error}"
        );
        assert!(error.contains("dyn Fn"), "stale target is named: {error}");
    }

    #[test]
    fn supported_member_inventory_rejects_public_doc_hidden_items() {
        let source = r#"
/// Example public type.
pub struct Example;
impl Example {
    #[doc(hidden, alias = "concealed_alias")]
    pub fn concealed() {}
}
"#;
        let file = syn::parse_file(source).expect("fixture must parse");
        let definition = file.items.first().expect("fixture has a type");
        let error = inventory_source_members(definition, &file.items, "Example")
            .expect_err("doc-hidden does not make a public method private");
        assert!(error.contains("Example::concealed"));

        let error = direct_facade_member_inventories_from_source(source, &BTreeMap::new())
            .expect_err("the facade workflow must inspect direct root-type members");
        assert!(error.contains("Example::concealed"));

        let aliased_impl = r"
/// Example public type.
pub struct Aliased;
type PrivateAlias = Aliased;
impl PrivateAlias {
    #[doc(hidden)]
    pub fn concealed() {}
}
";
        let file = syn::parse_file(aliased_impl).expect("alias fixture must parse");
        let definition = file.items.first().expect("fixture has a public type");
        let error = inventory_source_members(definition, &file.items, "Aliased")
            .expect_err("a private alias self type must fail closed");
        assert!(error.contains("self type `PrivateAlias`"));
        assert!(error.contains("name-resolution support"));

        let imported_alias = aliased_impl.replace(
            "type PrivateAlias = Aliased;",
            "use crate::Aliased as PrivateAlias;",
        );
        let file = syn::parse_file(&imported_alias).expect("import alias fixture must parse");
        let definition = file.items.first().expect("fixture has a public type");
        let error = inventory_source_members(definition, &file.items, "Aliased")
            .expect_err("an imported alias self type must fail closed");
        assert!(error.contains("self type `PrivateAlias`"));
        assert!(error.contains("name-resolution support"));

        let conditional = r#"
/// Example public type.
pub struct Conditional;
impl Conditional {
    #[cfg_attr(any(), doc(hidden, alias = "conditional_alias"))]
    pub fn concealed() {}
}
"#;
        let error = direct_facade_member_inventories_from_source(conditional, &BTreeMap::new())
            .expect_err("conditionally hidden public members are still public API");
        assert!(error.contains("Conditional::concealed"));

        let generated = r"
/// Example public type.
pub struct Generated;
impl Generated {
    make_members!();
}
";
        let error = direct_facade_member_inventories_from_source(generated, &BTreeMap::new())
            .expect_err("an impl macro may generate public members");
        assert!(error.contains("macro `make_members`"));

        let generated_at_module_scope = r"
/// Example public type.
pub struct ModuleGenerated;
make_members!();
";
        let file = syn::parse_file(generated_at_module_scope).expect("fixture must parse");
        let error = inventory_source_members(
            file.items.first().expect("fixture has a type"),
            &file.items,
            "ModuleGenerated",
        )
        .expect_err("a defining-module macro may generate hidden inherent members");
        assert!(error.contains("defining module"));
        assert!(error.contains("macro `make_members`"));

        let trait_source = r"
/// Example public trait.
pub trait Extension {
    /// An operation.
    fn operate(&self);
}
";
        let file = syn::parse_file(trait_source).expect("fixture must parse");
        let error = inventory_source_members(
            file.items.first().expect("fixture has a trait"),
            &file.items,
            "Extension",
        )
        .expect_err("re-exported traits need a trait-aware member inventory");
        assert!(error.contains("explicit trait-member inventory support"));

        let alias_source = r"
/// Example public alias.
pub type Alias = String;
";
        let file = syn::parse_file(alias_source).expect("fixture must parse");
        let error = inventory_source_members(
            file.items.first().expect("fixture has a type alias"),
            &file.items,
            "Alias",
        )
        .expect_err("re-exported aliases need underlying-member inventory");
        assert!(error.contains("type alias `Alias`"));
        assert!(error.contains("underlying-member inventory support"));

        let error = supported_reexports_from_source(
            r#"
#[doc(hidden, alias = "market_data")]
pub use pinewasm_vm::MarketData;
"#,
        )
        .expect_err("doc-hidden does not make a public re-export private");
        assert!(error.contains("re-export is public but doc-hidden"));
    }

    #[test]
    fn crate_scan_explicit_path_reaches_real_members_and_sibling_children() {
        let fixture = temp_dir("rust-api-explicit-module-path").expect("fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::create_dir_all(fixture.path().join("alt/entry")).expect("child directory");
        fs::create_dir_all(fixture.path().join("alias")).expect("decoy directory");
        fs::write(
            &library,
            "pub struct Example;\n#[r#path = \"alt/entry.rs\"] mod alias;\n",
        )
        .expect("library");
        fs::write(fixture.path().join("alt/entry.rs"), "mod child;\n").expect("entry");
        fs::write(
            fixture.path().join("alt/child.rs"),
            "impl crate::Example { pub fn visible() {} }\n",
        )
        .expect("real child");
        for decoy in ["alias.rs", "alias/mod.rs", "alt/entry/child.rs"] {
            fs::write(fixture.path().join(decoy), "unlisted_macro!();\n").expect("decoy");
        }
        let inventory = crate_inherent_member_inventories(&library, &library)
            .expect("actual path tree scanned");
        assert_eq!(
            inventory["Example"].member_identities,
            BTreeSet::from(["method visible".to_string()])
        );
        fs::write(
            fixture.path().join("alt/child.rs"),
            "impl crate::Example { #[doc(hidden)] pub fn concealed() {} }\n",
        )
        .expect("concealed child");
        assert!(crate_inherent_member_inventories(&library, &library)
            .expect_err("path is no API exemption")
            .contains("Example::concealed"));
        fs::write(
            fixture.path().join("alt/child.rs"),
            "type Example = crate::Example; impl Example { pub fn visible() {} }\n",
        )
        .expect("alias child");
        assert!(crate_inherent_member_inventories(&library, &library)
            .expect_err("syntax walker also follows real path")
            .contains("alias"));
    }

    #[test]
    fn crate_scan_explicit_path_rejects_unsupported_and_unsafe_paths() {
        let fixture = temp_dir("rust-api-module-path-negative").expect("fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::write(fixture.path().join("real.rs"), "").expect("real file");
        fs::create_dir_all(fixture.path().join("directory")).expect("directory");
        for declaration in [
            "#[path = \"real.rs\"] #[path = \"real.rs\"] mod bad;",
            "#[path] mod bad;",
            "#[path(\"real.rs\")] mod bad;",
            "#[path = 7] mod bad;",
            "#[path = concat!(\"real.rs\")] mod bad;",
            "#[path = \"/real.rs\"] mod bad;",
            "#[path = \"../real.rs\"] mod bad;",
            "#[path = \"./real.rs\"] mod bad;",
            "#[path = \"dir//real.rs\"] mod bad;",
            "#[path = \"C:/real.rs\"] mod bad;",
            "#[path = \"dir\\\\real.rs\"] mod bad;",
            "#[path = \"missing.rs\"] mod bad;",
            "#[path = \"directory\"] mod bad;",
            "#[path = \"real.rs\"] mod bad {}",
            "mod inline { #[path = \"real.rs\"] mod bad; }",
            "#[cfg_attr(any(), path = \"real.rs\")] mod bad;",
            "#[cfg_attr(all(), cfg_attr(any(), path = \"real.rs\"))] mod bad;",
            "#[cfg_attr(any(), r#path = \"real.rs\")] mod bad;",
        ] {
            fs::write(&library, declaration).expect("negative source");
            crate_inherent_member_inventories(&library, &library).expect_err(declaration);
        }
        fs::write(
            &library,
            "#[path = \"real.rs\"] mod first; #[path = \"real.rs\"] mod second;",
        )
        .expect("duplicate source");
        assert!(crate_inherent_member_inventories(&library, &library)
            .expect_err("canonical duplicates rejected")
            .contains("reachable more than once"));
        fs::write(&library, "mod real; #[path = \"real.rs\"] mod second;")
            .expect("ordinary and explicit alias");
        assert!(crate_inherent_member_inventories(&library, &library)
            .expect_err("mixed aliases rejected")
            .contains("reachable more than once"));
    }

    #[cfg(unix)]
    #[test]
    fn crate_scan_explicit_path_rejects_leaf_and_ancestor_symlinks() {
        let fixture = temp_dir("rust-api-module-path-symlink").expect("fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::create_dir_all(fixture.path().join("actual")).expect("directory");
        fs::write(fixture.path().join("actual/real.rs"), "").expect("real file");
        std::os::unix::fs::symlink("actual/real.rs", fixture.path().join("link.rs"))
            .expect("leaf link");
        std::os::unix::fs::symlink("actual", fixture.path().join("linked")).expect("ancestor link");
        for path in ["link.rs", "linked/real.rs"] {
            fs::write(&library, format!("#[path = {path:?}] mod bad;")).expect("source");
            assert!(crate_inherent_member_inventories(&library, &library)
                .expect_err("symlink refused")
                .contains("symlinks"));
        }
    }

    #[test]
    fn crate_scan_explicit_path_keeps_configuration_guards() {
        let fixture = temp_dir("rust-api-module-path-config").expect("fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::write(fixture.path().join("real.rs"), "").expect("real file");
        fs::write(
            &library,
            "#[cfg(not(doc))] #[path = \"real.rs\"] mod hidden;",
        )
        .expect("hidden source");
        assert!(crate_inherent_member_inventories(&library, &library)
            .expect_err("the Rustdoc guard must still reject it")
            .contains("Rustdoc"));
        fs::write(&library, "#[cfg(test)] #[path = \"missing.rs\"] mod tests;")
            .expect("test-only source");
        crate_inherent_member_inventories(&library, &library)
            .expect("test-only modules remain skipped");
    }

    #[test]
    fn crate_scan_current_csil_entry_has_inventory_support() {
        let root = repo_root().expect("repository root");
        let library = root.join("crates/pinewasm_csil/src/lib.rs");
        crate_inherent_member_inventories_with_providers(
            &library,
            &library,
            &BTreeSet::from(["serde".to_string(), "thiserror".to_string()]),
        )
        .expect("inspect actual CSIL module tree, including function_entry");
    }

    #[test]
    fn crate_scan_reaches_inherent_impls_in_private_child_modules() {
        let fixture = temp_dir("rust-api-crate-scan").expect("temporary fixture directory");
        let library = fixture.path().join("lib.rs");
        let child = fixture.path().join("hidden.rs");
        fs::write(&library, "pub struct Example;\nmod hidden;\n").expect("write fixture library");
        fs::write(
            &child,
            r"
impl crate::Example {
    /// Visible external implementation member.
    pub fn visible() {}
}
",
        )
        .expect("write visible child module");

        let inventories = crate_inherent_member_inventories(&library, &library)
            .expect("private child module must be scanned");
        assert_eq!(
            inventories["Example"].member_identities,
            BTreeSet::from(["method visible".to_string()])
        );

        fs::write(
            &child,
            r"
impl crate::Example {
    #[doc(hidden)]
    pub fn concealed() {}
}
",
        )
        .expect("replace child module with concealed member");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("doc-hidden public method in a private module must fail closed");
        assert!(error.contains("Example::concealed"));

        let raw_child = fixture.path().join("raw_child.rs");
        let raw_decoy = fixture.path().join("r#raw_child.rs");
        let raw_library = fixture.path().join("libraw_module.rlib");
        let raw_docs = fixture.path().join("raw_doc");
        fs::write(
            &library,
            r"
pub struct Target;
mod r#raw_child;

pub fn proof() -> u32 {
    Target::concealed()
}
",
        )
        .expect("write raw-module fixture library");
        fs::write(
            &raw_child,
            r"
impl crate::Target {
    #[doc(hidden)]
    pub fn concealed() -> u32 { 43 }
}
",
        )
        .expect("write semantically resolved raw child module");
        fs::write(&raw_decoy, "// The syntax gate must not scan this decoy.\n")
            .expect("write raw-spelling decoy module");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "raw_module_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&library)
                .arg("-o")
                .arg(&raw_library),
            "raw-module consumer build and hidden-method call",
        );
        fs::create_dir_all(&raw_docs).expect("create raw-module Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "raw_module_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&library)
                .arg("--out-dir")
                .arg(&raw_docs),
            "raw-module consumer Rustdoc build",
        );
        let html = fs::read_to_string(
            raw_docs
                .join("raw_module_consumer")
                .join("struct.Target.html"),
        )
        .expect("read raw-module target Rustdoc page");
        assert!(
            !html.contains("method.concealed"),
            "normal Rustdoc must omit the raw-module doc-hidden method"
        );
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("raw module names must resolve to their semantic filesystem component");
        assert!(
            error.contains("Target::concealed"),
            "unexpected raw-module rejection: {error}"
        );
    }

    #[test]
    fn supported_member_census_rejects_autoderef_surface_escape() {
        let fixture = temp_dir("rust-api-autoderef").expect("temporary fixture directory");
        let provider = fixture.path().join("provider.rs");
        let consumer = fixture.path().join("consumer.rs");
        let library = fixture.path().join("libderef_provider.rlib");
        let docs = fixture.path().join("doc");
        let external_dir = fixture.path().join("external");
        let external_root = external_dir.join("lib.rs");
        let external_impl = external_dir.join("facade_impl.rs");
        let external_library = fixture.path().join("libexternal_deref.rlib");
        let source = r"
#![allow(dead_code)]
use core::ops::Deref as r#Clone;

#[doc(hidden)]
pub struct Hidden;

impl Hidden {
    pub fn concealed(&self) -> u32 { 42 }
}

/// Example supported facade type.
pub struct Example(Hidden);

impl r#Clone for Example {
    type Target = Hidden;

    fn r#deref(&self) -> &Self::Target { &self.0 }
}
";
        fs::write(&provider, source).expect("write autoderef provider");
        fs::write(
            &consumer,
            r"
extern crate deref_provider;

pub fn proof(value: &deref_provider::Example) -> u32 {
    value.concealed()
}
",
        )
        .expect("write downstream autoderef consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "deref_provider",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("-o")
                .arg(&library),
            "autoderef provider build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "deref_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("deref_provider={}", library.display())),
            "facade-only autoderef call",
        );
        fs::create_dir_all(&docs).expect("create Rustdoc output directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "deref_provider",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("--out-dir")
                .arg(&docs),
            "autoderef provider Rustdoc build",
        );
        let html = fs::read_to_string(docs.join("deref_provider").join("struct.Example.html"))
            .expect("read supported type Rustdoc page");
        assert!(
            !html.contains("method.concealed"),
            "normal Rustdoc must omit the doc-hidden target's method"
        );

        let file = syn::parse_file(source).expect("autoderef fixture must parse");
        let definition = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Struct(item) if item.ident == "Example"))
            .expect("fixture supported type must exist");
        let error = inventory_source_members(definition, &file.items, "Example")
            .expect_err("autoderef may not add an unmanifested callable surface");
        assert!(error.contains("autoderef"));
        assert!(error.contains("Example"));

        fs::create_dir_all(&external_dir).expect("create external-module fixture directory");
        fs::write(
            &external_root,
            r"
#![allow(dead_code)]
mod facade_impl;

#[doc(hidden)]
pub struct Hidden;

impl Hidden {
    pub fn concealed(&self) -> u32 { 42 }
}

/// Example supported facade type.
pub struct Example(Hidden);
",
        )
        .expect("write external-module library root");
        fs::write(
            &external_impl,
            r"
use core::ops::Deref as r#Clone;
use super::{Example, Hidden};

impl r#Clone for Example {
    type Target = Hidden;

    fn r#deref(&self) -> &Self::Target { &self.0 }
}
",
        )
        .expect("write external-module implementation");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "external_deref",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&external_root)
                .arg("-o")
                .arg(&external_library),
            "external-module autoderef provider build",
        );
        let external_error = crate_inherent_member_inventories(&external_root, &external_root)
            .expect_err("autoderef in a private child module must also fail closed");
        assert!(external_error.contains("autoderef"));
        assert!(external_error.contains("Example"));

        let nested_source = r"
use core::ops::Deref as Clone;

pub struct Hidden;
pub struct Example(Hidden);

const _: () = {
    impl Clone for Example {
        type Target = Hidden;

        fn deref(&self) -> &Self::Target { &self.0 }
    }
};
";
        let nested_file = syn::parse_file(nested_source).expect("nested autoderef must parse");
        let nested_impl = nested_file
            .items
            .iter()
            .find(|item| matches!(item, Item::Const(_)))
            .expect("nested implementation container must exist");
        let nested_error = reject_nested_member_items(nested_impl, &provider)
            .expect_err("nested autoderef must not evade the module scan");
        assert!(nested_error.contains("autoderef"));

        let generated_source = r"
/// Example supported facade type.
pub struct Example;

impl RenamedTrait for Example {
    external_provider::deref_items!();
}
";
        let generated_file =
            syn::parse_file(generated_source).expect("generated autoderef fixture must parse");
        let generated_definition = generated_file
            .items
            .iter()
            .find(|item| matches!(item, Item::Struct(item) if item.ident == "Example"))
            .expect("generated fixture supported type must exist");
        let generated_error =
            inventory_source_members(generated_definition, &generated_file.items, "Example")
                .expect_err("impl-item macros may generate an aliased autoderef body");
        assert!(generated_error.contains("macro-generated trait implementation"));

        let ordinary_source = r"
use core::default::Default as Ordinary;

/// Example supported facade type.
pub struct Example;

impl Ordinary for Example {
    fn default() -> Self { Self }
}
";
        let ordinary_file =
            syn::parse_file(ordinary_source).expect("ordinary trait fixture must parse");
        let ordinary_definition = ordinary_file
            .items
            .iter()
            .find(|item| matches!(item, Item::Struct(item) if item.ident == "Example"))
            .expect("ordinary fixture supported type must exist");
        inventory_source_members(ordinary_definition, &ordinary_file.items, "Example")
            .expect("ordinary renamed trait methods do not change inherent lookup");
    }

    #[test]
    fn supported_member_census_rejects_doc_hidden_trait_impls() {
        let fixture = temp_dir("rust-api-doc-hidden-trait")
            .expect("temporary doc-hidden trait fixture directory");
        let provider = fixture.path().join("provider.rs");
        let consumer = fixture.path().join("consumer.rs");
        let library = fixture.path().join("libhidden_trait_provider.rlib");
        let docs = fixture.path().join("doc");
        let visible_provider = fixture.path().join("visible_provider.rs");
        let visible_docs = fixture.path().join("visible-doc");
        let source = r"
/// Example supported facade type.
pub struct Example(u32);

impl Example {
    /// Construct an example value.
    pub fn new(value: u32) -> Self { Self(value) }
}

#[doc(hidden)]
impl Iterator for Example {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        self.0 = self.0.saturating_add(1);
        Some(self.0)
    }
}
";
        fs::write(&provider, source).expect("write doc-hidden trait provider");
        fs::write(
            &consumer,
            r"
extern crate hidden_trait_provider;

pub fn proof() -> Option<u32> {
    hidden_trait_provider::Example::new(41).next()
}
",
        )
        .expect("write doc-hidden trait consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "hidden_trait_provider",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("-o")
                .arg(&library),
            "doc-hidden trait provider build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "hidden_trait_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("hidden_trait_provider={}", library.display())),
            "facade-only doc-hidden trait call",
        );
        fs::create_dir_all(&docs).expect("create doc-hidden trait Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "hidden_trait_provider",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("--out-dir")
                .arg(&docs),
            "doc-hidden trait provider Rustdoc build",
        );
        let visible_source = source.replacen("#[doc(hidden)]\nimpl Iterator", "impl Iterator", 1);
        fs::write(&visible_provider, visible_source)
            .expect("write visible trait Rustdoc positive control");
        fs::create_dir_all(&visible_docs)
            .expect("create visible trait Rustdoc positive-control directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "visible_trait_provider",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&visible_provider)
                .arg("--out-dir")
                .arg(&visible_docs),
            "visible trait provider Rustdoc positive-control build",
        );
        let visible_html = fs::read_to_string(
            visible_docs
                .join("visible_trait_provider")
                .join("struct.Example.html"),
        )
        .expect("read visible trait provider Rustdoc page");
        assert!(
            visible_html.contains("id=\"impl-Iterator-for-Example\""),
            "the Rustdoc id used by the absence assertion must first be visible in a positive control"
        );
        let html = fs::read_to_string(
            docs.join("hidden_trait_provider")
                .join("struct.Example.html"),
        )
        .expect("read doc-hidden trait provider Rustdoc page");
        assert!(
            !html.contains("id=\"impl-Iterator-for-Example\""),
            "normal Rustdoc must omit the doc-hidden Iterator implementation"
        );

        let file = syn::parse_file(source).expect("doc-hidden trait fixture must parse");
        let definition = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Struct(item) if item.ident == "Example"))
            .expect("doc-hidden trait fixture supported type must exist");
        let error = inventory_source_members(definition, &file.items, "Example")
            .expect_err("a callable trait implementation may not disappear from Rustdoc");
        assert!(error.contains("doc-hidden trait implementation"));
        assert!(error.contains("Example"));
    }

    #[test]
    fn supported_member_census_rejects_rustdoc_sensitive_trait_cfg() {
        let fixture =
            temp_dir("rust-api-cfg-doc-trait").expect("temporary cfg(doc) trait fixture directory");
        let provider = fixture.path().join("provider.rs");
        let consumer = fixture.path().join("consumer.rs");
        let library = fixture.path().join("libcfg_doc_trait_provider.rlib");
        let docs = fixture.path().join("doc");
        let source = r"
/// Example supported facade type.
pub struct Example(u32);

impl Example {
    /// Construct an example value.
    pub fn new(value: u32) -> Self { Self(value) }
}

#[cfg(not(doc))]
impl Iterator for Example {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        self.0 = self.0.saturating_add(1);
        Some(self.0)
    }
}
";
        fs::write(&provider, source).expect("write cfg(doc)-sensitive trait provider");
        fs::write(
            &consumer,
            r"
extern crate cfg_doc_trait_provider;

pub fn proof() -> Option<u32> {
    cfg_doc_trait_provider::Example::new(41).next()
}
",
        )
        .expect("write cfg(doc)-sensitive trait consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_trait_provider",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("-o")
                .arg(&library),
            "cfg(doc)-sensitive trait provider build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_trait_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("cfg_doc_trait_provider={}", library.display())),
            "facade-only cfg(doc)-sensitive trait call",
        );
        fs::create_dir_all(&docs).expect("create cfg(doc)-sensitive Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_trait_provider",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("--out-dir")
                .arg(&docs),
            "cfg(doc)-sensitive trait provider Rustdoc build",
        );
        let html = fs::read_to_string(
            docs.join("cfg_doc_trait_provider")
                .join("struct.Example.html"),
        )
        .expect("read cfg(doc)-sensitive trait provider Rustdoc page");
        assert!(
            !html.contains("id=\"impl-Iterator-for-Example\""),
            "Rustdoc must omit the normally callable cfg(not(doc)) implementation"
        );

        let file = syn::parse_file(source).expect("cfg(doc)-sensitive trait fixture must parse");
        let definition = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Struct(item) if item.ident == "Example"))
            .expect("cfg(doc)-sensitive fixture supported type must exist");
        let error = inventory_source_members(definition, &file.items, "Example")
            .expect_err("normal-build trait API may not disappear under cfg(doc)");
        assert!(error.contains("configuration depends on Rustdoc"));
        assert!(error.contains("Example"));

        for attribute in [
            "#[r#cfg(r#not(r#doc))]",
            "#[cfg(any(unix, all(feature = \"missing\", not(doc))))]",
            "#[cfg_attr(doc, cfg(any()))]",
            "#[cfg_attr(all(), cfg_attr(all(), cfg(not(doc))))]",
            "#[cfg_attr(not(doctest), allow(dead_code))]",
        ] {
            let variant = syn::parse_file(&format!(
                "pub struct Example;\n{attribute}\nimpl Iterator for Example {{ type Item = (); fn next(&mut self) -> Option<()> {{ None }} }}\n"
            ))
            .expect("nested/raw cfg(doc) variant must parse");
            let implementation = variant
                .items
                .iter()
                .find_map(|item| match item {
                    Item::Impl(implementation) => Some(implementation),
                    _ => None,
                })
                .expect("cfg(doc) variant must contain an implementation");
            let error = reject_trait_surface_escape(implementation, "Example")
                .expect_err("nested/raw Rustdoc configuration must fail closed");
            assert!(
                error.contains("configuration depends on Rustdoc"),
                "unexpected cfg(doc) rejection for {attribute}: {error}"
            );
        }

        let ordinary = syn::parse_file(
            "pub struct Example;\n#[cfg(feature = \"std\")]\nimpl Default for Example { fn default() -> Self { Self } }\n",
        )
        .expect("ordinary feature cfg fixture must parse");
        let ordinary_impl = ordinary
            .items
            .iter()
            .find_map(|item| match item {
                Item::Impl(implementation) => Some(implementation),
                _ => None,
            })
            .expect("ordinary feature cfg fixture must contain an implementation");
        reject_trait_surface_escape(ordinary_impl, "Example")
            .expect("a cfg unrelated to Rustdoc does not change the documented build surface");

        let nested = syn::parse_file(
            "pub struct Example;\nconst _: () = { #[cfg(not(doc))] impl Iterator for Example { type Item = (); fn next(&mut self) -> Option<()> { None } } };\n",
        )
        .expect("nested cfg(doc) trait fixture must parse");
        let nested_container = nested
            .items
            .iter()
            .find(|item| matches!(item, Item::Const(_)))
            .expect("nested cfg(doc) trait fixture must contain a const block");
        let nested_error = reject_nested_member_items(nested_container, &provider)
            .expect_err("nested cfg(doc)-sensitive trait impl must fail closed");
        assert!(nested_error.contains("configuration depends on Rustdoc"));

        let external_dir = fixture.path().join("external");
        let external_root = external_dir.join("lib.rs");
        let external_impl = external_dir.join("hidden.rs");
        fs::create_dir_all(&external_dir).expect("create external cfg(doc) module directory");
        fs::write(&external_root, "pub struct Example;\nmod hidden;\n")
            .expect("write external cfg(doc) fixture root");
        fs::write(
            &external_impl,
            "#[cfg(not(doc))]\nimpl Iterator for crate::Example { type Item = (); fn next(&mut self) -> Option<()> { None } }\n",
        )
        .expect("write external cfg(doc) fixture implementation");
        let external_error = crate_inherent_member_inventories(&external_root, &external_root)
            .expect_err("external cfg(doc)-sensitive trait impl must fail closed");
        assert!(external_error.contains("configuration depends on Rustdoc"));
    }

    #[test]
    fn crate_scan_rejects_rustdoc_sensitive_enclosing_module_cfg() {
        let fixture = temp_dir("rust-api-cfg-doc-module")
            .expect("temporary cfg(doc) module fixture directory");
        let provider = fixture.path().join("provider.rs");
        let consumer = fixture.path().join("consumer.rs");
        let library = fixture.path().join("libcfg_doc_module_provider.rlib");
        let docs = fixture.path().join("doc");
        let source = r"
/// Example supported facade type.
pub struct Example(u32);

impl Example {
    /// Construct an example value.
    pub fn new(value: u32) -> Self { Self(value) }
}

#[cfg(not(doc))]
mod hidden {
    impl Iterator for crate::Example {
        type Item = u32;

        fn next(&mut self) -> Option<Self::Item> {
            self.0 = self.0.saturating_add(1);
            Some(self.0)
        }
    }
}
";
        fs::write(&provider, source).expect("write cfg(doc)-sensitive module provider");
        fs::write(
            &consumer,
            r"
extern crate cfg_doc_module_provider;

pub fn proof() -> Option<u32> {
    cfg_doc_module_provider::Example::new(41).next()
}
",
        )
        .expect("write cfg(doc)-sensitive module consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_module_provider",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("-o")
                .arg(&library),
            "cfg(doc)-sensitive module provider build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_module_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("cfg_doc_module_provider={}", library.display())),
            "facade-only call through a cfg(doc)-sensitive module",
        );
        fs::create_dir_all(&docs).expect("create cfg(doc)-sensitive module Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "cfg_doc_module_provider",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("--out-dir")
                .arg(&docs),
            "cfg(doc)-sensitive module provider Rustdoc build",
        );
        let html = fs::read_to_string(
            docs.join("cfg_doc_module_provider")
                .join("struct.Example.html"),
        )
        .expect("read cfg(doc)-sensitive module provider Rustdoc page");
        assert!(
            !html.contains("id=\"impl-Iterator-for-Example\""),
            "Rustdoc must omit the normally callable implementation inside a cfg(not(doc)) module"
        );

        let inline_error = crate_inherent_member_inventories(&provider, &provider)
            .expect_err("a trait impl inside a cfg(doc)-sensitive module must fail closed");
        assert!(
            inline_error.contains("configuration depends on Rustdoc"),
            "unexpected inline module rejection: {inline_error}"
        );
        assert!(inline_error.contains("`hidden`"));

        for (index, attribute) in [
            "#[r#cfg(r#not(r#doc))]",
            "#[cfg(any(test, not(doc)))]",
            "#[cfg_attr(doc, cfg(any()))]",
            "#[cfg_attr(all(), cfg_attr(all(), cfg(not(doc))))]",
        ]
        .into_iter()
        .enumerate()
        {
            let variant_dir = fixture.path().join(format!("variant-{index}"));
            let variant_root = variant_dir.join("lib.rs");
            fs::create_dir_all(&variant_dir).expect("create cfg(doc) module variant directory");
            fs::write(
                &variant_root,
                format!(
                    "pub struct Example;\nmod outer {{\n    {attribute}\n    mod inner {{\n        impl Iterator for crate::Example {{ type Item = (); fn next(&mut self) -> Option<()> {{ None }} }}\n    }}\n}}\n"
                ),
            )
            .expect("write cfg(doc) module variant");
            let error = crate_inherent_member_inventories(&variant_root, &variant_root)
                .expect_err("nested/raw Rustdoc configuration on a module must fail closed");
            assert!(
                error.contains("configuration depends on Rustdoc"),
                "unexpected cfg(doc) module rejection for {attribute}: {error}"
            );
        }

        let external_dir = fixture.path().join("external");
        let external_root = external_dir.join("lib.rs");
        let external_module = external_dir.join("hidden.rs");
        fs::create_dir_all(&external_dir).expect("create external cfg(doc) module directory");
        fs::write(&external_root, "pub struct Example;\nmod hidden;\n")
            .expect("write external cfg(doc) module fixture root");
        fs::write(
            &external_module,
            "#![cfg(not(doc))]\nimpl Iterator for crate::Example { type Item = (); fn next(&mut self) -> Option<()> { None } }\n",
        )
        .expect("write external cfg(doc) module fixture");
        let external_error = crate_inherent_member_inventories(&external_root, &external_root)
            .expect_err("a child file whose inner attributes depend on Rustdoc must fail closed");
        assert!(
            external_error.contains("configuration depends on Rustdoc"),
            "unexpected child-file rejection: {external_error}"
        );
        assert!(external_error.contains("hidden.rs"));

        let ordinary_dir = fixture.path().join("ordinary");
        let ordinary_root = ordinary_dir.join("lib.rs");
        fs::create_dir_all(&ordinary_dir).expect("create ordinary module cfg directory");
        fs::write(
            &ordinary_root,
            "pub struct Example;\n#[cfg(feature = \"std\")]\nmod visible {\n    impl Default for crate::Example { fn default() -> Self { Self } }\n}\n",
        )
        .expect("write ordinary module cfg fixture");
        crate_inherent_member_inventories(&ordinary_root, &ordinary_root).expect(
            "a module cfg unrelated to Rustdoc does not change the documented build surface",
        );
    }

    #[test]
    fn crate_scan_rejects_cross_module_type_alias_impls() {
        let fixture = temp_dir("rust-api-cross-module-alias").expect("temporary fixture directory");
        let library = fixture.path().join("lib.rs");
        let child = fixture.path().join("hidden.rs");

        for (label, alias_declaration, impl_name, hidden_attribute, expected_rejection) in [
            (
                "raw_declaration",
                "type r#Alias = Target;",
                "Alias",
                "#[doc(hidden)]",
                "self type `Alias`",
            ),
            (
                "raw_impl",
                "type Alias = Target;",
                "r#Alias",
                "#[r#doc(hidden)]",
                "self type `Alias`",
            ),
            (
                "raw_import",
                "use self::Target as r#Alias;",
                "Alias",
                "#[doc(r#hidden)]",
                "self type `Alias`",
            ),
            (
                "raw_nominal_impl",
                "",
                "r#Target",
                "#[r#doc(r#hidden)]",
                "is public but doc-hidden",
            ),
        ] {
            let consumer = fixture.path().join(format!("{label}.rs"));
            let docs = fixture.path().join(format!("{label}_doc"));
            let crate_name = format!("{label}_consumer");
            fs::write(
                &consumer,
                format!(
                    r"
pub struct Target;
{alias_declaration}

impl {impl_name} {{
    {hidden_attribute}
    pub fn concealed() -> u32 {{ 41 }}
}}

pub fn proof() -> u32 {{
    Target::concealed()
}}
"
                ),
            )
            .expect("write raw-alias consumer");
            assert_command_succeeds(
                pinned_rust_tool("rustc", fixture.path())
                    .arg("--crate-name")
                    .arg(&crate_name)
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer),
                &format!("{label} consumer build and hidden-method call"),
            );
            fs::create_dir_all(&docs).expect("create raw-alias Rustdoc directory");
            assert_command_succeeds(
                pinned_rust_tool("rustdoc", fixture.path())
                    .arg("--crate-name")
                    .arg(&crate_name)
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer)
                    .arg("--out-dir")
                    .arg(&docs),
                &format!("{label} consumer Rustdoc build"),
            );
            let html = fs::read_to_string(docs.join(&crate_name).join("struct.Target.html"))
                .expect("read raw-alias target Rustdoc page");
            assert!(
                !html.contains("method.concealed"),
                "normal Rustdoc must omit the {label} alias method"
            );
            let source = fs::read_to_string(&consumer).expect("read raw-alias consumer");
            let file = syn::parse_file(&source).expect("raw-alias consumer must parse");
            let definition = file
                .items
                .iter()
                .find(|item| matches!(item, Item::Struct(item) if item.ident == "Target"))
                .expect("raw-alias target must exist");
            let error = inventory_source_members(definition, &file.items, "Target")
                .expect_err("raw and ordinary identifier spellings must compare semantically");
            assert!(
                error.contains(expected_rejection),
                "unexpected {label} rejection: {error}"
            );
        }

        fs::write(
            &library,
            "pub struct Example;\ntype Alias = Example;\nmod hidden;\n",
        )
        .expect("write fixture library");
        fs::write(
            &child,
            r"
use crate::Alias;
impl Alias {
    #[doc(hidden)]
    pub fn concealed() {}
}
",
        )
        .expect("write child implementation through an unrenamed parent alias");

        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("a parent-module type alias must fail closed in a child impl");
        assert!(error.contains("self type `Alias`"));
        assert!(error.contains("name-resolution support"));

        fs::write(
            &library,
            "pub struct Example;\ntype Alias = Example;\nuse Alias as Other;\nmod hidden;\n",
        )
        .expect("add a renamed intermediate alias");
        fs::write(
            &child,
            r"
use crate::Other as DeepAlias;
impl DeepAlias {
    #[doc(hidden)]
    pub fn concealed() {}
}
",
        )
        .expect("write child implementation through a renamed alias chain");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("renamed alias chains must also fail closed");
        assert!(error.contains("self type `DeepAlias`"));
        assert!(error.contains("name-resolution support"));

        fs::write(
            &library,
            "mod alias_holder { type Record = u32; }\nmod hidden;\n",
        )
        .expect("write a same-spelled alias in an unrelated module");
        fs::write(
            &child,
            r"
pub struct Record;
impl Record {
    pub fn visible() {}
}
",
        )
        .expect("write an unrelated nominal type with the same name");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("the syntax-only alias guard is deliberately crate-wide");
        assert!(error.contains("self type `Record`"));
        assert!(error.contains("may be a transparent type/import alias"));
    }

    #[test]
    fn crate_scan_rejects_compiled_procedural_api_generation() {
        let fixture =
            temp_dir("rust-api-procedural-attribute").expect("temporary fixture directory");
        let macro_source = fixture.path().join("hostile_api.rs");
        let macro_library = fixture
            .path()
            .join(format!("{DLL_PREFIX}hostile_api{DLL_SUFFIX}"));
        let consumer = fixture.path().join("consumer.rs");
        let bare_consumer = fixture.path().join("bare_consumer.rs");
        let absolute_consumer = fixture.path().join("absolute_consumer.rs");
        let raw_provider_consumer = fixture.path().join("raw_provider_consumer.rs");
        let raw_provider_docs = fixture.path().join("raw_provider_doc");
        let docs = fixture.path().join("doc");
        fs::write(
            &macro_source,
            r#"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn conceal_api(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    format!(
        "{item}\nimpl crate::AttributeTarget {{ #[doc(hidden)] pub fn concealed_by_attribute() -> u32 {{ 7 }} }}"
    )
    .parse()
    .unwrap()
}

#[proc_macro_derive(Conceal)]
pub fn conceal_derive(_item: TokenStream) -> TokenStream {
    "impl crate::DeriveTarget { #[doc(hidden)] pub fn concealed_by_derive() -> u32 { 11 } }"
        .parse()
        .unwrap()
}

#[proc_macro_attribute]
pub fn conceal_error(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    format!(
        "{item}\nimpl crate::RenamedAttributeTarget {{ #[doc(hidden)] pub fn concealed_by_renamed_attribute() -> u32 {{ 13 }} }}"
    )
    .parse()
    .unwrap()
}

#[proc_macro_derive(ConcealSerialize)]
pub fn conceal_serialize(_item: TokenStream) -> TokenStream {
    "impl crate::RenamedSerializeTarget { #[doc(hidden)] pub fn concealed_by_renamed_serialize() -> u32 { 17 } }"
        .parse()
        .unwrap()
}

#[proc_macro_derive(ConcealDebug)]
pub fn conceal_debug(_item: TokenStream) -> TokenStream {
    "impl crate::RenamedDebugTarget { #[doc(hidden)] pub fn concealed_by_renamed_debug() -> u32 { 19 } }"
        .parse()
        .unwrap()
}

#[proc_macro_derive(ConcealDeserialize)]
pub fn conceal_deserialize(_item: TokenStream) -> TokenStream {
    "impl crate::GlobDeserializeTarget { #[doc(hidden)] pub fn concealed_by_glob_deserialize() -> u32 { 23 } }"
        .parse()
        .unwrap()
}

#[proc_macro_derive(Serialize)]
pub fn conceal_provider(_item: TokenStream) -> TokenStream {
    "impl crate::ProviderTarget { #[doc(hidden)] pub fn concealed_by_provider_shadow() -> u32 { 29 } }"
        .parse()
        .unwrap()
}
"#,
        )
        .expect("write hostile procedural macro");
        fs::write(
            &consumer,
            r"
extern crate hostile_api;

use hostile_api::{
    conceal_error as error, ConcealDebug as Debug, ConcealSerialize as Serialize,
};

mod names {
    pub use hostile_api::ConcealDeserialize as Deserialize;
}
use names::*;

mod providers {
    pub use hostile_api::Serialize;
}
use providers as serde;

pub struct AttributeTarget;

#[hostile_api::conceal_api]
pub fn install_attribute_members() {}

#[derive(hostile_api::Conceal)]
pub struct DeriveTarget;

pub struct RenamedAttributeTarget;

#[error]
pub fn install_renamed_attribute_members() {}

#[derive(Serialize)]
pub struct RenamedSerializeTarget;

#[derive(Debug)]
pub struct RenamedDebugTarget;

#[derive(Deserialize)]
pub struct GlobDeserializeTarget;

#[derive(serde::Serialize)]
pub struct ProviderTarget;

pub fn proof() -> (u32, u32, u32, u32, u32, u32, u32) {
    (
        AttributeTarget::concealed_by_attribute(),
        DeriveTarget::concealed_by_derive(),
        RenamedAttributeTarget::concealed_by_renamed_attribute(),
        RenamedSerializeTarget::concealed_by_renamed_serialize(),
        RenamedDebugTarget::concealed_by_renamed_debug(),
        GlobDeserializeTarget::concealed_by_glob_deserialize(),
        ProviderTarget::concealed_by_provider_shadow(),
    )
}
",
        )
        .expect("write procedural-macro consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "hostile_api",
                    "--crate-type",
                    "proc-macro",
                    "--edition=2021",
                ])
                .arg(&macro_source)
                .arg("-o")
                .arg(&macro_library),
            "procedural macro build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "procedural_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display())),
            "procedural consumer build and generated-method call",
        );
        fs::create_dir_all(&docs).expect("create Rustdoc output directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "procedural_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display()))
                .arg("--out-dir")
                .arg(&docs),
            "procedural consumer Rustdoc build",
        );
        for (target, member) in [
            ("AttributeTarget", "concealed_by_attribute"),
            ("DeriveTarget", "concealed_by_derive"),
            ("RenamedAttributeTarget", "concealed_by_renamed_attribute"),
            ("RenamedSerializeTarget", "concealed_by_renamed_serialize"),
            ("RenamedDebugTarget", "concealed_by_renamed_debug"),
            ("GlobDeserializeTarget", "concealed_by_glob_deserialize"),
            ("ProviderTarget", "concealed_by_provider_shadow"),
        ] {
            let html = fs::read_to_string(
                docs.join("procedural_consumer")
                    .join(format!("struct.{target}.html")),
            )
            .expect("read generated Rustdoc page");
            assert!(
                !html.contains(&format!("method.{member}")),
                "normal Rustdoc must omit the generated doc-hidden method"
            );
        }

        let source = fs::read_to_string(&consumer).expect("read procedural consumer");
        let file = syn::parse_file(&source).expect("procedural consumer must parse");
        let syntax = crate_syntax_context(&consumer, fixture.path(), &BTreeSet::new())
            .expect("procedural consumer syntax context");
        let unresolved_macro_glob = module_macro_glob_is_ambiguous(&file.items, &syntax);
        let attribute_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_attribute_members"))
            .expect("attribute-bearing function must exist");
        let error = reject_nested_member_items(attribute_item, &consumer)
            .expect_err("an unlisted attribute macro must fail closed");
        assert!(error.contains("attribute macro candidate `hostile_api::conceal_api`"));

        let derive_item = file
            .items
            .iter()
            .find(
                |item| matches!(item, Item::Struct(structure) if structure.ident == "DeriveTarget"),
            )
            .expect("derive-bearing type must exist");
        let error = reject_nested_member_items(derive_item, &consumer)
            .expect_err("an unlisted derive macro must fail closed");
        assert!(error.contains("unlisted derive macro `hostile_api::Conceal`"));

        for (target, expected) in [
            (
                "RenamedSerializeTarget",
                "derive macro `Serialize` whose bare name may be shadowed",
            ),
            (
                "RenamedDebugTarget",
                "derive macro `Debug` whose bare name may be shadowed",
            ),
            (
                "GlobDeserializeTarget",
                "derive macro `Deserialize` whose bare name may be shadowed",
            ),
            (
                "ProviderTarget",
                "derive macro `serde::Serialize` without an exact allowlisted Cargo provider",
            ),
        ] {
            let item = file
                .items
                .iter()
                .find(|item| matches!(item, Item::Struct(structure) if structure.ident == target))
                .expect("renamed derive target must exist");
            let error = reject_nested_member_items_with_context(
                item,
                &consumer,
                &syntax,
                unresolved_macro_glob,
            )
            .expect_err("renamed derive macro must fail closed");
            assert!(error.contains(expected), "unexpected rejection: {error}");
        }

        let renamed_attribute = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_renamed_attribute_members"))
            .expect("renamed attribute target must exist");
        let error = reject_nested_member_items_with_context(
            renamed_attribute,
            &consumer,
            &syntax,
            unresolved_macro_glob,
        )
        .expect_err("renamed helper-looking attribute macro must fail closed");
        assert!(
            error.contains("helper attribute `error` without an origin-validated derive"),
            "unexpected rejection: {error}"
        );

        fs::write(
            &bare_consumer,
            r"
extern crate hostile_api as r#serde;
use serde::Serialize;

pub struct ProviderTarget;

#[derive(Serialize)]
pub struct BareAliasTrigger;

pub fn proof() -> u32 {
    ProviderTarget::concealed_by_provider_shadow()
}
",
        )
        .expect("write bare provider-alias consumer");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "bare_alias_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&bare_consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display())),
            "bare provider-alias consumer build and generated-method call",
        );
        let error = crate_inherent_member_inventories_with_providers(
            &bare_consumer,
            &bare_consumer,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect_err("a bare import through an aliased provider must fail closed");
        assert!(
            error.contains("without an unambiguous exact allowlisted provider import"),
            "unexpected rejection: {error}"
        );

        fs::write(
            &absolute_consumer,
            r"
extern crate hostile_api as serde;

pub struct ProviderTarget;

#[r#derive(::r#serde::r#Serialize)]
pub struct AbsoluteAliasTrigger;

pub fn proof() -> u32 {
    ProviderTarget::concealed_by_provider_shadow()
}
",
        )
        .expect("write absolute provider-alias consumer");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "absolute_alias_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&absolute_consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display())),
            "absolute provider-alias consumer build and generated-method call",
        );
        let error = crate_inherent_member_inventories_with_providers(
            &absolute_consumer,
            &absolute_consumer,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect_err("an absolute extern-crate alias must not impersonate an allowlisted provider");
        assert!(
            error.contains("provider root may be shadowed"),
            "unexpected rejection: {error}"
        );

        fs::write(
            &raw_provider_consumer,
            r"
extern crate hostile_api;

mod r#serde {
    pub use hostile_api::Serialize;
}

pub struct ProviderTarget;

#[r#derive(r#serde::r#Serialize)]
pub struct RawProviderTrigger;

pub fn proof() -> u32 {
    ProviderTarget::concealed_by_provider_shadow()
}
",
        )
        .expect("write raw provider-root consumer");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "raw_provider_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&raw_provider_consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display())),
            "raw provider-root consumer build and generated-method call",
        );
        fs::create_dir_all(&raw_provider_docs).expect("create raw provider-root Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "raw_provider_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&raw_provider_consumer)
                .arg("--extern")
                .arg(format!("hostile_api={}", macro_library.display()))
                .arg("--out-dir")
                .arg(&raw_provider_docs),
            "raw provider-root consumer Rustdoc build",
        );
        let html = fs::read_to_string(
            raw_provider_docs
                .join("raw_provider_consumer")
                .join("struct.ProviderTarget.html"),
        )
        .expect("read raw provider-root target Rustdoc page");
        assert!(
            !html.contains("method.concealed_by_provider_shadow"),
            "normal Rustdoc must omit the raw provider-root generated method"
        );
        let error = crate_inherent_member_inventories_with_providers(
            &raw_provider_consumer,
            &raw_provider_consumer,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect_err("a raw local provider root must shadow its ordinary spelling");
        assert!(
            error.contains("provider root may be shadowed"),
            "unexpected raw provider-root rejection: {error}"
        );
    }

    #[test]
    fn absolute_qualified_derive_paths_ignore_bare_only_root_shadowing() {
        let fixture = temp_dir("rust-api-absolute-derive").expect("temporary fixture directory");
        let derive_source = fixture.path().join("serde_derive.rs");
        let derive_library = fixture
            .path()
            .join(format!("{DLL_PREFIX}serde{DLL_SUFFIX}"));
        let consumer = fixture.path().join("consumer.rs");
        let docs = fixture.path().join("doc");
        fs::write(
            &derive_source,
            r"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Serialize)]
pub fn serialize(_item: TokenStream) -> TokenStream {
    TokenStream::new()
}
",
        )
        .expect("write derive stand-in");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "serde",
                    "--crate-type",
                    "proc-macro",
                    "--edition=2021",
                ])
                .arg(&derive_source)
                .arg("-o")
                .arg(&derive_library),
            "derive stand-in build",
        );

        fs::write(
            &consumer,
            r"
mod r#serde {}

#[r#derive(::r#serde::r#Serialize)]
pub struct Qualified;
",
        )
        .expect("write absolute derive consumer");
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "absolute_derive_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("serde={}", derive_library.display())),
            "absolute derive consumer build",
        );
        fs::create_dir_all(&docs).expect("create absolute derive Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "absolute_derive_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("serde={}", derive_library.display()))
                .arg("--out-dir")
                .arg(&docs),
            "absolute derive consumer Rustdoc build",
        );
        assert!(docs
            .join("absolute_derive_consumer")
            .join("struct.Qualified.html")
            .is_file());
        crate_inherent_member_inventories_with_providers(
            &consumer,
            &consumer,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect("a leading-colon qualified provider must bypass only bare local-module shadowing");
    }

    #[test]
    fn crate_scan_rejects_conditional_value_and_macro_namespace_glob_spoof() {
        let fixture = temp_dir("rust-api-cfg-gated-glob").expect("temporary fixture directory");
        let hostile_source = fixture.path().join("hostile_api.rs");
        let hostile_library = fixture
            .path()
            .join(format!("{DLL_PREFIX}hostile_api{DLL_SUFFIX}"));
        let serde_source = fixture.path().join("serde.rs");
        let serde_library = fixture
            .path()
            .join(format!("{DLL_PREFIX}serde{DLL_SUFFIX}"));
        let local_consumer = fixture.path().join("local_consumer.rs");
        let local_docs = fixture.path().join("local_doc");

        fs::write(
            &hostile_source,
            r#"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Serialize)]
pub fn conceal(_item: TokenStream) -> TokenStream {
    "impl crate::Target { #[doc(hidden)] pub fn concealed_by_cfg_glob() -> u32 { 31 } }"
        .parse()
        .unwrap()
}
"#,
        )
        .expect("write hostile glob macro");
        fs::write(
            &serde_source,
            r"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Serialize)]
pub fn serialize(_item: TokenStream) -> TokenStream {
    TokenStream::new()
}
",
        )
        .expect("write provider stand-in");
        fs::write(
            &local_consumer,
            r"
extern crate serde;

mod ext {
    pub use serde::Serialize;
}

use ext::*;

#[derive(Serialize)]
pub struct Allowlisted;
",
        )
        .expect("write unconditional local-root positive control");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "hostile_api",
                    "--crate-type",
                    "proc-macro",
                    "--edition=2021",
                ])
                .arg(&hostile_source)
                .arg("-o")
                .arg(&hostile_library),
            "hostile glob macro build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "serde",
                    "--crate-type",
                    "proc-macro",
                    "--edition=2021",
                ])
                .arg(&serde_source)
                .arg("-o")
                .arg(&serde_library),
            "provider stand-in build",
        );
        for (label, apparent_local_root) in [
            ("cfg", "#[cfg(any())]\nmod ext {}"),
            ("raw_cfg", "#[r#cfg(r#any())]\nmod r#ext {}"),
            (
                "raw_cfg_attr",
                "#[r#cfg_attr(r#all(), r#cfg(r#any()))]\nmod r#ext {}",
            ),
            ("function", "#[allow(dead_code)]\nfn ext() {}"),
            (
                "constant",
                "#[allow(non_upper_case_globals)]\nconst ext: () = ();",
            ),
            (
                "static",
                "#[allow(non_upper_case_globals)]\nstatic ext: () = ();",
            ),
            ("macro", "macro_rules! ext { () => {} }"),
        ] {
            let consumer = fixture.path().join(format!("{label}_consumer.rs"));
            let docs = fixture.path().join(format!("{label}_doc"));
            let crate_name = format!("{label}_glob_consumer");
            fs::write(
                &consumer,
                format!(
                    r"
extern crate serde;
extern crate hostile_api as ext;

#[allow(unused_imports)]
mod unrelated {{
    use serde::Serialize;
}}

{apparent_local_root}

use ext::*;

pub struct Target;

#[derive(Serialize)]
pub struct Trigger;

pub fn proof() -> u32 {{
    Target::concealed_by_cfg_glob()
}}
"
                ),
            )
            .expect("write hostile glob consumer");
            assert_command_succeeds(
                pinned_rust_tool("rustc", fixture.path())
                    .arg("--crate-name")
                    .arg(&crate_name)
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer)
                    .arg("--extern")
                    .arg(format!("serde={}", serde_library.display()))
                    .arg("--extern")
                    .arg(format!("hostile_api={}", hostile_library.display())),
                &format!("{label} glob consumer build and generated-method call"),
            );
            fs::create_dir_all(&docs).expect("create hostile glob Rustdoc directory");
            assert_command_succeeds(
                pinned_rust_tool("rustdoc", fixture.path())
                    .arg("--crate-name")
                    .arg(&crate_name)
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&consumer)
                    .arg("--extern")
                    .arg(format!("serde={}", serde_library.display()))
                    .arg("--extern")
                    .arg(format!("hostile_api={}", hostile_library.display()))
                    .arg("--out-dir")
                    .arg(&docs),
                &format!("{label} glob consumer Rustdoc build"),
            );
            let html = fs::read_to_string(docs.join(&crate_name).join("struct.Target.html"))
                .expect("read hostile glob target Rustdoc page");
            assert!(
                !html.contains("method.concealed_by_cfg_glob"),
                "normal Rustdoc must omit the {label}-glob-generated doc-hidden method"
            );

            let error = crate_inherent_member_inventories_with_providers(
                &consumer,
                &consumer,
                &BTreeSet::from(["serde".to_string()]),
            )
            .expect_err("an apparent non-type root must not disguise an external macro glob");
            let expected_rejection = if label == "macro" {
                "contains non-expression macro `macro_rules`"
            } else {
                "derive macro `Serialize` whose bare name may be shadowed"
            };
            assert!(
                error.contains(expected_rejection),
                "unexpected {label} rejection: {error}"
            );
        }

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "local_glob_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&local_consumer)
                .arg("--extern")
                .arg(format!("serde={}", serde_library.display())),
            "unconditional local-root positive-control build",
        );
        fs::create_dir_all(&local_docs).expect("create unconditional local-root Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "local_glob_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&local_consumer)
                .arg("--extern")
                .arg(format!("serde={}", serde_library.display()))
                .arg("--out-dir")
                .arg(&local_docs),
            "unconditional local-root positive-control Rustdoc build",
        );
        assert!(
            local_docs
                .join("local_glob_consumer")
                .join("struct.Allowlisted.html")
                .is_file(),
            "positive control must produce ordinary Rustdoc"
        );

        crate_inherent_member_inventories_with_providers(
            &local_consumer,
            &local_consumer,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect("an unconditional local module may relay an exact allowlisted derive provider");
    }

    #[test]
    fn only_unconditional_type_namespace_items_make_globs_local() {
        for definition in [
            "fn ext() {}",
            "const ext: () = ();",
            "static ext: () = ();",
            "macro_rules! ext { () => {} }",
        ] {
            let source = format!("{definition}\nuse ext::*;\n");
            let file = syn::parse_file(&source).expect("value/macro-root fixture must parse");
            assert!(
                module_has_unresolved_macro_glob(&file.items),
                "a value- or macro-namespace definition must not shadow an extern-prelude path root: {definition}"
            );
        }

        for definition in [
            "mod ext {}",
            "enum ext { Value }",
            "struct ext;",
            "trait ext {}",
            "trait ext = Copy;",
            "type ext = ();",
            "union ext { value: u8 }",
        ] {
            let source = format!("{definition}\nuse ext::*;\n");
            let file = syn::parse_file(&source).expect("type-root fixture must parse");
            assert!(
                !module_has_unresolved_macro_glob(&file.items),
                "an unconditional type-namespace definition shadows an extern-prelude path root: {definition}"
            );
        }

        for attribute in [
            "#[cfg(any())]",
            "#[cfg_attr(all(), cfg(any()))]",
            "#[cfg_attr(all(), cfg_attr(all(), cfg(any())))]",
            "#[r#cfg(any())]",
            "#[r#cfg_attr(all(), r#cfg(any()))]",
            "#[r#cfg_attr(all(), r#cfg_attr(all(), r#cfg(any())))]",
        ] {
            for definition in [
                "mod ext {}",
                "enum ext { Value }",
                "struct ext;",
                "trait ext {}",
                "type ext = ();",
                "union ext { value: u8 }",
            ] {
                let source = format!("{attribute}\n{definition}\nuse ext::*;\n");
                let file =
                    syn::parse_file(&source).expect("conditional type-root fixture must parse");
                assert!(
                    module_has_unresolved_macro_glob(&file.items),
                    "a conditionally removable type root must leave the same-named glob unresolved: {attribute} {definition}"
                );
            }
        }

        for alias in [
            "extern crate hostile_api as ext;",
            "use hostile_api as ext;",
            "use crate::names as ext;",
        ] {
            let source = format!("{alias}\nuse ext::*;\n");
            let file = syn::parse_file(&source).expect("alias-root fixture must parse");
            assert!(
                module_has_unresolved_macro_glob(&file.items),
                "an alias root requires name resolution and must fail closed: {alias}"
            );
        }

        let file = syn::parse_file("mod ext {}\nuse ::ext::*;\n")
            .expect("absolute external-glob fixture must parse");
        assert!(
            module_has_unresolved_macro_glob(&file.items),
            "a leading-colon glob resolves from the extern prelude despite a same-named local root"
        );
        assert!(
            !module_has_local_macro_glob(&file.items),
            "a leading-colon glob must never be classified as local"
        );

        let file = syn::parse_file("mod r#ext {}\nuse ext::*;\n")
            .expect("raw local-glob fixture must parse");
        assert!(
            !module_has_unresolved_macro_glob(&file.items),
            "raw and ordinary spellings of a local type-namespace root are identical"
        );
        assert!(
            module_has_local_macro_glob(&file.items),
            "a raw-spelled local root must be classified as local"
        );

        let file = syn::parse_file("mod r#ext {}\nuse ::r#ext::*;\n")
            .expect("raw absolute external-glob fixture must parse");
        assert!(
            module_has_unresolved_macro_glob(&file.items),
            "a leading-colon raw glob remains external despite a same-named local root"
        );
        assert!(
            !module_has_local_macro_glob(&file.items),
            "a leading-colon raw glob must never be classified as local"
        );
    }

    #[test]
    fn absolute_macro_glob_cannot_impersonate_a_same_named_local_root() {
        let fixture =
            temp_dir("rust-api-absolute-macro-glob").expect("temporary fixture directory");
        let provider = fixture.path().join("leading_glob_provider.rs");
        let library = fixture.path().join("libleading_glob_provider.rlib");
        let consumer = fixture.path().join("consumer.rs");
        let docs = fixture.path().join("doc");
        fs::write(
            &provider,
            r"
#[macro_export]
macro_rules! vec {
    () => {{
        impl core::ops::Deref for crate::Target {
            type Target = crate::Hidden;

            fn deref(&self) -> &Self::Target { &self.0 }
        }
        ()
    }};
}
",
        )
        .expect("write absolute-glob macro provider");
        fs::write(
            &consumer,
            r"
#![no_implicit_prelude]
#![allow(non_local_definitions)]

extern crate core;

mod leading_glob_provider {}
use ::leading_glob_provider::*;

#[doc(hidden)]
pub struct Hidden;

impl Hidden {
    #[doc(hidden)]
    pub fn concealed(&self) -> u32 { 37 }
}

pub struct Target(Hidden);

pub fn install() {
    let () = vec!();
}

pub fn proof(value: &Target) -> u32 {
    value.concealed()
}
",
        )
        .expect("write absolute-glob consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "leading_glob_provider",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&provider)
                .arg("-o")
                .arg(&library),
            "absolute-glob macro provider build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "absolute_glob_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("leading_glob_provider={}", library.display())),
            "absolute-glob consumer build and generated-method call",
        );
        fs::create_dir_all(&docs).expect("create absolute-glob Rustdoc directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "absolute_glob_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("leading_glob_provider={}", library.display()))
                .arg("--out-dir")
                .arg(&docs),
            "absolute-glob consumer Rustdoc build",
        );
        let html = fs::read_to_string(
            docs.join("absolute_glob_consumer")
                .join("struct.Target.html"),
        )
        .expect("read absolute-glob target Rustdoc page");
        assert!(
            !html.contains("method.concealed"),
            "normal Rustdoc must omit the generated doc-hidden target member"
        );

        let source = fs::read_to_string(&consumer).expect("read absolute-glob consumer");
        let file = syn::parse_file(&source).expect("absolute-glob consumer must parse");
        assert!(
            module_has_unresolved_macro_glob(&file.items),
            "the leading-colon glob must remain externally ambiguous"
        );
        let error = crate_inherent_member_inventories(&consumer, &consumer)
            .expect_err("an absolute external macro glob must fail closed");
        assert!(
            error.contains("glob import makes its origin ambiguous"),
            "unexpected absolute-glob rejection: {error}"
        );
    }

    #[test]
    fn crate_scan_skips_only_cfg_predicates_that_require_test() {
        for (attribute, expected) in [
            ("#[cfg(test)]", true),
            ("#[r#cfg(r#test)]", true),
            ("#[cfg(all(test, target_pointer_width = \"64\"))]", true),
            (
                "#[r#cfg(r#all(r#test, target_pointer_width = \"64\"))]",
                true,
            ),
            (
                "#[cfg(all(target_family = \"unix\", all(test, feature = \"fixture\")))]",
                true,
            ),
            ("#[cfg(any(test, target_pointer_width = \"64\"))]", false),
            ("#[cfg(not(test))]", false),
            (
                "#[cfg(all(target_pointer_width = \"64\", any(test, doctest)))]",
                false,
            ),
            ("#[cfg_attr(all(), cfg(test))]", false),
        ] {
            let file = syn::parse_file(&format!("{attribute}\nmod tests {{}}\n"))
                .expect("cfg predicate fixture must parse");
            let Item::Mod(module) = &file.items[0] else {
                panic!("fixture must contain a module")
            };
            assert_eq!(
                cfg_test_only(&module.attrs),
                expected,
                "unexpected test-only classification for {attribute}"
            );
        }

        let fixture = temp_dir("rust-api-compound-test-cfg").expect("temporary fixture directory");
        for (label, predicate, should_skip) in [
            ("required", "all(test, target_pointer_width = \"64\")", true),
            (
                "optional",
                "any(test, target_pointer_width = \"64\")",
                false,
            ),
        ] {
            let source = fixture.path().join(format!("{label}.rs"));
            let docs = fixture.path().join(format!("{label}-doc"));
            fs::write(
                &source,
                format!(
                    r"
#[cfg({predicate})]
mod tests {{
    pub fn statement_macro() {{
        assert!(true);
    }}
}}

pub struct Supported;
"
                ),
            )
            .expect("write compound cfg fixture");
            assert_command_succeeds(
                pinned_rust_tool("rustc", fixture.path())
                    .arg("--crate-name")
                    .arg(format!("compound_cfg_{label}"))
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&source),
                &format!("{label} compound-cfg compiler control"),
            );
            fs::create_dir_all(&docs).expect("create compound-cfg Rustdoc directory");
            assert_command_succeeds(
                pinned_rust_tool("rustdoc", fixture.path())
                    .arg("--crate-name")
                    .arg(format!("compound_cfg_{label}"))
                    .args(["--crate-type", "lib", "--edition=2021"])
                    .arg(&source)
                    .arg("--out-dir")
                    .arg(&docs),
                &format!("{label} compound-cfg Rustdoc control"),
            );

            let result = crate_inherent_member_inventories(&source, &source);
            if should_skip {
                result.expect("a conjunction requiring test is absent from public builds");
            } else {
                let error = result.expect_err(
                    "a disjunction that can compile without test must remain in the API scan",
                );
                assert!(
                    error.contains("nested item-position macro"),
                    "unexpected compound-cfg rejection: {error}"
                );
            }
        }
    }

    #[test]
    fn crate_scan_accepts_only_allowlisted_derive_providers_and_helper_contexts() {
        let fixture =
            temp_dir("rust-api-allowlisted-derive-providers").expect("temporary fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::write(
            &library,
            r#"
use serde::{self, Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wire {
    #[serde(default)]
    pub value: u32,
}

#[derive(Debug, Error)]
pub enum Fault {
    #[error("bad input")]
    Bad,
}
"#,
        )
        .expect("write derive fixture");
        crate_inherent_member_inventories_with_providers(
            &library,
            &library,
            &BTreeSet::from(["serde".to_string(), "thiserror".to_string()]),
        )
        .expect("allowlisted providers and their scoped helper attributes must pass");

        fs::write(
            &library,
            r"
#[serde(default)]
pub struct UnboundHelper;
",
        )
        .expect("write unbound helper fixture");
        let error = crate_inherent_member_inventories_with_providers(
            &library,
            &library,
            &BTreeSet::from(["serde".to_string()]),
        )
        .expect_err("a helper without its allowlisted derive context must fail closed");
        assert!(error.contains("without an origin-validated derive"));

        fs::write(
            &library,
            r"
mod relay {
    pub use hostile_api::*;
}
use relay::*;

#[derive(Debug)]
pub struct GlobRelayedBuiltin;
",
        )
        .expect("write relayed external-glob fixture");
        let error =
            crate_inherent_member_inventories_with_providers(&library, &library, &BTreeSet::new())
                .expect_err(
                    "an external macro glob relayed through a local module must taint the crate",
                );
        assert!(
            error.contains("derive macro `Debug` whose bare name may be shadowed"),
            "unexpected rejection: {error}"
        );
    }

    #[test]
    fn cargo_macro_provider_census_requires_exact_declaration_and_resolution() {
        let valid = serde_json::json!({
            "dependencies": [
                {
                    "name": "serde",
                    "rename": null,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": null,
                    "optional": false,
                    "features": ["derive"]
                },
                {
                    "name": "thiserror",
                    "rename": null,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": null,
                    "optional": false,
                    "features": []
                }
            ]
        });
        assert_eq!(
            declared_macro_providers_from_package(&valid, "valid").expect("valid providers"),
            BTreeSet::from(["serde".to_string(), "thiserror".to_string()])
        );

        for invalid in [
            serde_json::json!({
                "dependencies": [{
                    "name": "serde",
                    "rename": "serde",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": null,
                    "optional": false,
                    "features": ["derive"]
                }]
            }),
            serde_json::json!({
                "dependencies": [{
                    "name": "serde",
                    "rename": null,
                    "source": "path+file:///tmp/serde",
                    "kind": null,
                    "optional": false,
                    "features": ["derive"]
                }]
            }),
            serde_json::json!({
                "dependencies": [{
                    "name": "serde",
                    "rename": null,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": null,
                    "optional": false,
                    "features": []
                }]
            }),
            serde_json::json!({
                "dependencies": [{
                    "name": "serde",
                    "rename": null,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": "dev",
                    "optional": false,
                    "features": ["derive"]
                }]
            }),
            serde_json::json!({
                "dependencies": [{
                    "name": "serde",
                    "rename": null,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "kind": null,
                    "features": ["derive"]
                }]
            }),
        ] {
            assert!(declared_macro_providers_from_package(&invalid, "invalid")
                .expect("well-formed invalid provider")
                .is_empty());
        }

        let metadata = serde_json::json!({
            "packages": [
                {
                    "id": "path+file:///workspace/app#0.1.0",
                    "dependencies": valid["dependencies"].clone()
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                    "name": "serde",
                    "source": "registry+https://github.com/rust-lang/crates.io-index"
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#thiserror@2.0.0",
                    "name": "thiserror",
                    "source": "registry+https://github.com/rust-lang/crates.io-index"
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#serde_derive@1.0.0",
                    "name": "serde_derive",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "targets": [{"kind": ["proc-macro"]}]
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#thiserror-impl@2.0.0",
                    "name": "thiserror-impl",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "targets": [{"kind": ["proc-macro"]}]
                }
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "path+file:///workspace/app#0.1.0",
                        "deps": [
                            {
                                "name": "serde",
                                "pkg": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                                "dep_kinds": [{"kind": null, "target": null}]
                            },
                            {
                                "name": "thiserror",
                                "pkg": "registry+https://github.com/rust-lang/crates.io-index#thiserror@2.0.0",
                                "dep_kinds": [{"kind": null, "target": null}]
                            }
                        ]
                    },
                    {
                        "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                        "deps": [{
                            "name": "serde_derive",
                            "pkg": "registry+https://github.com/rust-lang/crates.io-index#serde_derive@1.0.0",
                            "dep_kinds": [{"kind": null, "target": null}]
                        }]
                    },
                    {
                        "id": "registry+https://github.com/rust-lang/crates.io-index#thiserror@2.0.0",
                        "deps": [{
                            "name": "thiserror_impl",
                            "pkg": "registry+https://github.com/rust-lang/crates.io-index#thiserror-impl@2.0.0",
                            "dep_kinds": [{"kind": null, "target": null}]
                        }]
                    }
                ]
            }
        });
        assert_eq!(
            trusted_macro_providers_from_metadata(
                &metadata,
                &metadata["packages"][0],
                "path+file:///workspace/app#0.1.0",
            )
            .expect("exact resolved providers"),
            BTreeSet::from(["serde".to_string(), "thiserror".to_string()])
        );

        let mut patched = metadata.clone();
        patched["packages"][1]["source"] = serde_json::Value::Null;
        assert_eq!(
            trusted_macro_providers_from_metadata(
                &patched,
                &patched["packages"][0],
                "path+file:///workspace/app#0.1.0",
            )
            .expect("path-patched provider census"),
            BTreeSet::from(["thiserror".to_string()]),
            "a path patch must not inherit trust from its crates.io declaration"
        );

        for (package_index, provider) in [(3, "serde"), (4, "thiserror")] {
            let mut patched_implementation = metadata.clone();
            patched_implementation["packages"][package_index]["source"] = serde_json::Value::Null;
            let trusted = trusted_macro_providers_from_metadata(
                &patched_implementation,
                &patched_implementation["packages"][0],
                "path+file:///workspace/app#0.1.0",
            )
            .expect("path-patched implementation census");
            assert!(
                !trusted.contains(provider),
                "a path-patched implementation must remove trust from `{provider}`"
            );
        }

        let mut non_macro_implementation = metadata;
        non_macro_implementation["packages"][3]["targets"] = serde_json::json!([{"kind": ["lib"]}]);
        let trusted = trusted_macro_providers_from_metadata(
            &non_macro_implementation,
            &non_macro_implementation["packages"][0],
            "path+file:///workspace/app#0.1.0",
        )
        .expect("non-macro implementation census");
        assert!(
            !trusted.contains("serde"),
            "the resolved implementation must actually be a proc-macro package"
        );
    }

    #[test]
    fn cargo_macro_provider_census_rejects_callable_patched_implementations() {
        let root = repo_root().expect("repository root must be discoverable");
        // Only the two fixture dependencies need to be cached. Workspace-wide
        // metadata would also require cached, unrelated target-specific crates.
        let lockfile: toml::Value = toml::from_str(
            &fs::read_to_string(root.join("Cargo.lock")).expect("read workspace lockfile"),
        )
        .expect("workspace lockfile must be TOML");
        let packages = lockfile["package"]
            .as_array()
            .expect("workspace lockfile packages");

        for provider in ["serde", "thiserror"] {
            let versions = packages
                .iter()
                .filter(|package| {
                    package["name"].as_str() == Some(provider)
                        && package["source"].as_str() == Some(CRATES_IO_REGISTRY_SOURCE)
                })
                .filter_map(|package| package["version"].as_str())
                .collect::<Vec<_>>();
            let [version] = versions.as_slice() else {
                panic!("workspace must lock exactly one crates.io `{provider}` version")
            };
            let (dependency, implementation_package, implementation_source, consumer_source) =
                match provider {
                    "serde" => (
                        format!("serde = {{ version = \"={version}\", features = [\"derive\"] }}"),
                        "serde_derive",
                        r#"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Serialize, attributes(serde))]
pub fn serialize(_item: TokenStream) -> TokenStream {
    "impl crate::Target { #[doc(hidden)] pub fn concealed_by_patched_derive() -> u32 { 37 } }"
        .parse()
        .expect("valid generated implementation")
}

#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn deserialize(_item: TokenStream) -> TokenStream {
    TokenStream::new()
}
"#,
                        r"
use serde::Serialize;

#[derive(Serialize)]
pub struct Target;

pub fn proof() -> u32 {
    Target::concealed_by_patched_derive()
}
",
                    ),
                    "thiserror" => (
                        format!("thiserror = \"={version}\""),
                        "thiserror-impl",
                        r#"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Error, attributes(error, from, source, backtrace))]
pub fn error(_item: TokenStream) -> TokenStream {
    "impl crate::Target { #[doc(hidden)] pub fn concealed_by_patched_derive() -> u32 { 41 } }"
        .parse()
        .expect("valid generated implementation")
}
"#,
                        r"
use thiserror::Error;

#[derive(Error)]
pub struct Target;

pub fn proof() -> u32 {
    Target::concealed_by_patched_derive()
}
",
                    ),
                    _ => unreachable!("provider fixture list is exhaustive"),
                };

            let fixture = temp_dir(&format!("rust-api-patched-{provider}-implementation"))
                .expect("temporary patched-provider fixture");
            let implementation = fixture.path().join("implementation");
            fs::create_dir_all(fixture.path().join("src")).expect("create consumer source");
            fs::create_dir_all(implementation.join("src")).expect("create implementation source");
            fs::write(
                fixture.path().join("Cargo.toml"),
                format!(
                    r#"[package]
name = "provider-patch-repro"
version = "0.0.0"
edition = "2021"

[dependencies]
{dependency}

[patch.crates-io]
{implementation_package} = {{ path = "implementation" }}

[workspace]
exclude = ["implementation"]
"#
                ),
            )
            .expect("write consumer manifest");
            fs::write(fixture.path().join("src/lib.rs"), consumer_source)
                .expect("write consumer source");
            fs::write(
                implementation.join("Cargo.toml"),
                format!(
                    r#"[package]
name = "{implementation_package}"
version = "{version}"
edition = "2021"

[lib]
proc-macro = true
"#
                ),
            )
            .expect("write patched implementation manifest");
            fs::write(implementation.join("src/lib.rs"), implementation_source)
                .expect("write patched implementation source");

            assert_command_succeeds(
                pinned_fixture_cargo_command(fixture.path())
                    .args(["generate-lockfile", "--offline"]),
                &format!("{provider} patched-implementation lockfile"),
            );
            assert_command_succeeds(
                pinned_fixture_cargo_command(fixture.path()).args([
                    "build",
                    "--locked",
                    "--offline",
                ]),
                &format!("{provider} patched-implementation callable build"),
            );
            assert_command_succeeds(
                pinned_fixture_cargo_command(fixture.path()).args([
                    "doc",
                    "--locked",
                    "--offline",
                    "--no-deps",
                ]),
                &format!("{provider} patched-implementation Rustdoc build"),
            );
            let rendered = fs::read_to_string(
                fixture
                    .path()
                    .join("target/doc/provider_patch_repro/struct.Target.html"),
            )
            .expect("read patched-provider Rustdoc page");
            assert!(
                !rendered.contains("concealed_by_patched_derive"),
                "the hostile callable must remain hidden from Rustdoc"
            );

            let metadata = pinned_fixture_cargo_command(fixture.path())
                .args([
                    "metadata",
                    "--format-version",
                    "1",
                    "--all-features",
                    "--locked",
                    "--offline",
                ])
                .output()
                .expect("patched-provider Cargo metadata must start");
            assert!(
                metadata.status.success(),
                "patched-provider Cargo metadata failed: {}",
                String::from_utf8_lossy(&metadata.stderr)
            );
            let libraries = workspace_libraries_from_metadata(&metadata.stdout)
                .expect("patched-provider metadata must parse");
            let library = libraries
                .get("provider_patch_repro")
                .expect("fixture library must be inventoried");
            assert!(
                !library.trusted_macro_providers.contains(provider),
                "a path-patched proc-macro implementation must revoke `{provider}` trust"
            );
            let consumer = fixture.path().join("src/lib.rs");
            let error = crate_inherent_member_inventories_with_providers(
                &consumer,
                &consumer,
                &library.trusted_macro_providers,
            )
            .expect_err("the API scanner must reject the callable hidden derive output");
            assert!(
                error.contains("derive macro")
                    && (error.contains("may be shadowed by an import")
                        || error
                            .contains("without an unambiguous exact allowlisted provider import")),
                "unexpected patched-provider rejection: {error}"
            );
        }
    }

    #[test]
    fn cargo_macro_provider_census_rejects_opaque_source_replacement() {
        let fixture =
            temp_dir("rust-api-source-replacement").expect("temporary source-replacement fixture");
        let cargo_home = fixture.path().join("cargo-home");
        let vendor = fixture.path().join("vendor");
        fs::create_dir_all(fixture.path().join("src")).expect("create consumer source");
        fs::create_dir_all(fixture.path().join(".cargo")).expect("create Cargo config directory");
        fs::create_dir_all(&cargo_home).expect("create isolated Cargo home");
        fs::create_dir_all(&vendor).expect("create directory source");

        write_directory_source_package(
            &vendor,
            "serde",
            "1.0.0",
            r#"
[features]
default = []
derive = ["dep:serde_derive"]

[dependencies.serde_derive]
version = "=1.0.0"
optional = true
"#,
            r#"
#[cfg(feature = "derive")]
pub use serde_derive::Serialize;
"#,
        );
        write_directory_source_package(
            &vendor,
            "serde_derive",
            "1.0.0",
            r"
[lib]
proc-macro = true
",
            r#"
extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Serialize)]
pub fn serialize(_item: TokenStream) -> TokenStream {
    "impl crate::Target { #[doc(hidden)] pub fn concealed_by_source_replacement() -> u32 { 43 } }"
        .parse()
        .expect("valid generated implementation")
}
"#,
        );
        fs::write(
            fixture.path().join("Cargo.toml"),
            r#"[package]
name = "source-replacement-repro"
version = "0.0.0"
edition = "2021"

[dependencies]
serde = { version = "=1.0.0", features = ["derive"] }

[workspace]
"#,
        )
        .expect("write source-replacement consumer manifest");
        fs::write(
            fixture.path().join("src/lib.rs"),
            r"
use serde::Serialize;

#[derive(Serialize)]
pub struct Target;

pub fn proof() -> u32 {
    Target::concealed_by_source_replacement()
}
",
        )
        .expect("write source-replacement consumer source");
        fs::write(
            fixture.path().join(".cargo/config.toml"),
            r#"[source.crates-io]
replace-with = "hostile-directory"

[source.hostile-directory]
directory = "vendor"
"#,
        )
        .expect("write source-replacement configuration");

        assert_command_succeeds(
            isolated_fixture_cargo(fixture.path(), &cargo_home)
                .args(["generate-lockfile", "--offline"]),
            "source-replacement lockfile",
        );
        assert_command_succeeds(
            isolated_fixture_cargo(fixture.path(), &cargo_home).args([
                "build",
                "--locked",
                "--offline",
            ]),
            "source-replacement callable build",
        );
        assert_command_succeeds(
            isolated_fixture_cargo(fixture.path(), &cargo_home).args([
                "doc",
                "--locked",
                "--offline",
                "--no-deps",
            ]),
            "source-replacement Rustdoc build",
        );
        let rendered = fs::read_to_string(
            fixture
                .path()
                .join("target/doc/source_replacement_repro/struct.Target.html"),
        )
        .expect("read source-replacement Rustdoc page");
        assert!(
            !rendered.contains("concealed_by_source_replacement"),
            "the hostile callable must remain hidden from Rustdoc"
        );

        let metadata = isolated_fixture_cargo(fixture.path(), &cargo_home)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--all-features",
                "--locked",
                "--offline",
            ])
            .output()
            .expect("source-replacement Cargo metadata must start");
        assert!(
            metadata.status.success(),
            "source-replacement Cargo metadata failed: {}",
            String::from_utf8_lossy(&metadata.stderr)
        );
        let metadata_value: serde_json::Value = serde_json::from_slice(&metadata.stdout)
            .expect("source-replacement metadata must be JSON");
        for package in ["serde", "serde_derive"] {
            let source = metadata_value["packages"]
                .as_array()
                .expect("metadata packages")
                .iter()
                .find(|candidate| candidate["name"].as_str() == Some(package))
                .and_then(|candidate| candidate["source"].as_str());
            assert_eq!(
                source,
                Some(CRATES_IO_REGISTRY_SOURCE),
                "Cargo must demonstrate its opaque logical source for `{package}`"
            );
        }
        let libraries = workspace_libraries_from_metadata(&metadata.stdout)
            .expect("source-replacement metadata must parse");
        let library = libraries
            .get("source_replacement_repro")
            .expect("fixture library must be inventoried");
        assert!(
            library.trusted_macro_providers.contains("serde"),
            "metadata identity alone must mark the serde provider as trusted"
        );
        let consumer = fixture.path().join("src/lib.rs");
        crate_inherent_member_inventories_with_providers(
            &consumer,
            &consumer,
            &library.trusted_macro_providers,
        )
        .expect("syntax plus opaque metadata alone is accepted by the census");

        let error = reject_untrusted_cargo_sources(fixture.path())
            .expect_err("the production preflight must reject source replacement");
        let configuration = fixture.path().join(".cargo").join("config.toml");
        assert!(
            error.contains("dependency source override")
                && error.contains(&configuration.display().to_string()),
            "unexpected source-replacement rejection: {error}"
        );
    }

    #[test]
    fn crate_scan_rejects_compiled_statement_and_expression_macro_api_generation() {
        let fixture = temp_dir("rust-api-statement-macro").expect("temporary fixture directory");
        let macro_source = fixture.path().join("hostile_statement.rs");
        let macro_library = fixture.path().join("libhostile_statement.rlib");
        let consumer = fixture.path().join("lib.rs");
        let docs = fixture.path().join("doc");
        fs::write(
            &macro_source,
            r"
#[macro_export]
macro_rules! generate_members {
    () => {
        impl crate::StatementTarget {
            #[doc(hidden)]
            pub fn concealed_by_statement() -> u32 { 13 }
        }
    };
}

#[macro_export]
macro_rules! expression_only {
    () => {{ () }};
}

#[macro_export]
macro_rules! expression_autoderef {
    () => {{
        impl core::ops::Deref for crate::ExpressionTarget {
            type Target = crate::ExpressionHidden;

            fn deref(&self) -> &Self::Target { &self.0 }
        }
        ()
    }};
}

#[macro_export]
macro_rules! raw_safe_name_autoderef {
    () => {{
        impl core::ops::Deref for crate::RawExpressionTarget {
            type Target = crate::RawExpressionHidden;

            fn deref(&self) -> &Self::Target { &self.0 }
        }
        ()
    }};
}

#[macro_export]
macro_rules! block_import_autoderef {
    () => {{
        impl core::ops::Deref for crate::BlockImportTarget {
            type Target = crate::BlockImportHidden;

            fn deref(&self) -> &Self::Target { &self.0 }
        }
        ()
    }};
}

#[macro_export]
macro_rules! trait_item_autoderef {
    () => {
        fn install_trait_item_surface() {
            impl core::ops::Deref for crate::TraitItemTarget {
                type Target = crate::TraitItemHidden;

                fn deref(&self) -> &Self::Target { &self.0 }
            }
        }
    };
}

#[macro_export]
macro_rules! impl_item_autoderef {
    () => {
        fn install_impl_item_surface() {
            impl core::ops::Deref for crate::ImplItemTarget {
                type Target = crate::ImplItemHidden;

                fn deref(&self) -> &Self::Target { &self.0 }
            }
        }
    };
}
",
        )
        .expect("write hostile statement macro");
        fs::write(
            &consumer,
            r"
#![allow(non_local_definitions)]
extern crate hostile_statement;
use hostile_statement::raw_safe_name_autoderef as r#format;

pub struct StatementTarget;

#[doc(hidden)]
pub struct ExpressionHidden;

impl ExpressionHidden {
    pub fn concealed_by_expression(&self) -> u32 { 17 }
}

pub struct ExpressionTarget(ExpressionHidden);

#[doc(hidden)]
pub struct RawExpressionHidden;

impl RawExpressionHidden {
    pub fn concealed_by_raw_expression(&self) -> u32 { 19 }
}

pub struct RawExpressionTarget(RawExpressionHidden);

#[doc(hidden)]
pub struct BlockImportHidden;

impl BlockImportHidden {
    pub fn concealed_by_block_import(&self) -> u32 { 23 }
}

pub struct BlockImportTarget(BlockImportHidden);

#[doc(hidden)]
pub struct TraitItemHidden;

impl TraitItemHidden {
    pub fn concealed_by_trait_item(&self) -> u32 { 29 }
}

pub struct TraitItemTarget(TraitItemHidden);

#[doc(hidden)]
pub struct ImplItemHidden;

impl ImplItemHidden {
    pub fn concealed_by_impl_item(&self) -> u32 { 31 }
}

pub struct ImplItemTarget(ImplItemHidden);

trait PrivateTrait {
    hostile_statement::trait_item_autoderef!();
}

struct Unrelated;

impl Unrelated {
    hostile_statement::impl_item_autoderef!();
}

pub fn install_statement_members() {
    hostile_statement::generate_members!();
}

pub fn allowlisted_expression_macro() {
    let _: Vec<u32> = vec![1];
}

pub fn install_expression_members() {
    let () = hostile_statement::expression_autoderef!();
}

pub fn install_raw_safe_name_members() {
    let () = r#format!();
}

pub fn install_block_import_members() {
    let _: Vec<()> = vec![{
        use hostile_statement::block_import_autoderef as vec;
        vec!()
    }];
}

pub fn proof() -> u32 {
    StatementTarget::concealed_by_statement()
}

pub fn expression_proof(value: &ExpressionTarget) -> u32 {
    value.concealed_by_expression()
}

pub fn raw_expression_proof(value: &RawExpressionTarget) -> u32 {
    value.concealed_by_raw_expression()
}

pub fn block_import_proof(value: &BlockImportTarget) -> u32 {
    value.concealed_by_block_import()
}

pub fn trait_item_proof(value: &TraitItemTarget) -> u32 {
    value.concealed_by_trait_item()
}

pub fn impl_item_proof(value: &ImplItemTarget) -> u32 {
    value.concealed_by_impl_item()
}
",
        )
        .expect("write statement-macro consumer");

        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "hostile_statement",
                    "--crate-type",
                    "rlib",
                    "--edition=2021",
                ])
                .arg(&macro_source)
                .arg("-o")
                .arg(&macro_library),
            "statement macro build",
        );
        assert_command_succeeds(
            pinned_rust_tool("rustc", fixture.path())
                .args([
                    "--crate-name",
                    "statement_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("hostile_statement={}", macro_library.display())),
            "statement consumer build and generated-method call",
        );
        fs::create_dir_all(&docs).expect("create Rustdoc output directory");
        assert_command_succeeds(
            pinned_rust_tool("rustdoc", fixture.path())
                .args([
                    "--crate-name",
                    "statement_consumer",
                    "--crate-type",
                    "lib",
                    "--edition=2021",
                ])
                .arg(&consumer)
                .arg("--extern")
                .arg(format!("hostile_statement={}", macro_library.display()))
                .arg("--out-dir")
                .arg(&docs),
            "statement consumer Rustdoc build",
        );
        let html = fs::read_to_string(
            docs.join("statement_consumer")
                .join("struct.StatementTarget.html"),
        )
        .expect("read statement target Rustdoc page");
        assert!(
            !html.contains("method.concealed_by_statement"),
            "normal Rustdoc must omit the generated doc-hidden method"
        );
        for (page, hidden_method) in [
            (
                "struct.ExpressionTarget.html",
                "method.concealed_by_expression",
            ),
            (
                "struct.RawExpressionTarget.html",
                "method.concealed_by_raw_expression",
            ),
            (
                "struct.BlockImportTarget.html",
                "method.concealed_by_block_import",
            ),
            (
                "struct.TraitItemTarget.html",
                "method.concealed_by_trait_item",
            ),
            (
                "struct.ImplItemTarget.html",
                "method.concealed_by_impl_item",
            ),
        ] {
            let html = fs::read_to_string(docs.join("statement_consumer").join(page))
                .expect("read expression target Rustdoc page");
            assert!(
                !html.contains(hidden_method),
                "normal Rustdoc must omit the generated target member"
            );
        }

        let source = fs::read_to_string(&consumer).expect("read statement consumer");
        let file = syn::parse_file(&source).expect("statement consumer must parse");
        let statement_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_statement_members"))
            .expect("statement-position macro function must exist");
        let statement_error = reject_nested_member_items(statement_item, &consumer)
            .expect_err("a function-body statement macro can emit a non-local impl");
        assert!(statement_error.contains("nested item-position macro"));

        let expression_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "allowlisted_expression_macro"))
            .expect("expression-position macro function must exist");
        reject_nested_member_items(expression_item, &consumer)
            .expect("an unshadowed allowlisted standard expression macro stays accepted");

        let hostile_expression_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_expression_members"))
            .expect("hostile expression-position macro function must exist");
        let hostile_error = reject_nested_member_items(hostile_expression_item, &consumer)
            .expect_err("an expression-position macro can emit a non-local impl");
        assert!(
            hostile_error.contains("expression macro `hostile_statement::expression_autoderef`")
        );

        let syntax = crate_syntax_context(&consumer, fixture.path(), &BTreeSet::new())
            .expect("expression-macro origin must parse");
        let raw_expression_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_raw_safe_name_members"))
            .expect("raw-renamed expression macro function must exist");
        let raw_error =
            reject_nested_member_items_with_context(raw_expression_item, &consumer, &syntax, false)
                .expect_err(
                    "a raw-renamed import must not impersonate an allowlisted standard macro",
                );
        assert!(raw_error.contains("expression macro `format`"));

        let block_import_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "install_block_import_members"))
            .expect("block-local macro import function must exist");
        let block_import_error = reject_nested_member_items(block_import_item, &consumer)
            .expect_err("opaque standard-macro arguments must not hide a local macro binding");
        assert!(block_import_error.contains("binding- or member-changing `use` token"));

        let trait_item = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Trait(item) if item.ident == "PrivateTrait"))
            .expect("trait-item macro container must exist");
        let trait_error = reject_nested_member_items(trait_item, &consumer)
            .expect_err("a trait-item macro can emit a method containing a non-local impl");
        assert!(
            trait_error.contains("non-expression macro `hostile_statement::trait_item_autoderef`")
        );

        let unrelated_impl = file
            .items
            .iter()
            .find(|item| {
                matches!(item, Item::Impl(item)
                    if item.trait_.is_none() && impl_type_name(item).as_deref() == Some("Unrelated"))
            })
            .expect("impl-item macro container must exist");
        let impl_error = reject_nested_member_items(unrelated_impl, &consumer)
            .expect_err("an impl-item macro can emit a method containing a non-local impl");
        assert!(
            impl_error.contains("non-expression macro `hostile_statement::impl_item_autoderef`")
        );

        let error = crate_inherent_member_inventories(&consumer, &consumer)
            .expect_err("every compiled API-generating macro position must fail closed");
        assert!(error.contains("macro"));
    }

    #[test]
    fn crate_scan_rejects_api_items_nested_in_item_bodies() {
        let fixture = temp_dir("rust-api-nested-impl-scan").expect("temporary fixture directory");
        let library = fixture.path().join("lib.rs");
        fs::write(
            &library,
            r"
pub struct Example;
const _: () = {
    impl crate::Example {
        /// A callable method hidden from the type's rendered Rustdoc page.
        pub fn visible() {}
    }
};
",
        )
        .expect("write visible nested implementation");

        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("nested inherent implementation must fail closed");
        assert!(error.contains("nested inherent implementation"));

        fs::write(
            &library,
            r"
pub struct Example;
const _: () = {
    generate_members!();
};
",
        )
        .expect("replace nested implementation with a macro invocation");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("const-body macro expansion must fail closed");
        assert!(error.contains("nested item-position macro"));

        fs::write(
            &library,
            r"
pub struct Example;
fn install_members() {
    generate_members!();
}
",
        )
        .expect("replace const body with a function-body macro invocation");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("a function-body macro can expand to a non-local inherent impl");
        assert!(error.contains("nested item-position macro"));

        fs::write(
            &library,
            r"
pub struct Example;
fn unlisted_expression_macro() {
    let () = expression_only!();
}
",
        )
        .expect("force the macro invocation into expression position");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("an unlisted expression macro can expand to a non-local impl");
        assert!(error.contains("expression macro `expression_only`"));

        fs::write(
            &library,
            r"
pub struct Example;
fn allowlisted_outer_macro_cannot_hide_impl_tokens() {
    let _ = vec![{
        impl crate::Example {
            pub fn hidden_in_tokens() {}
        }
        0
    }];
}
",
        )
        .expect("hide an implementation inside allowlisted macro arguments");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("opaque arguments of allowlisted macros must not hide an implementation");
        assert!(error.contains("expression macro `vec`"));

        fs::write(
            &library,
            r"
pub struct Example;
fn allowlisted_outer_macro_cannot_hide_nested_macro() {
    let _ = vec![expression_only!()];
}
",
        )
        .expect("hide an unlisted macro invocation inside allowlisted macro arguments");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("opaque arguments of allowlisted macros must not hide a nested macro");
        assert!(error.contains("expression macro `vec`"));

        fs::write(
            &library,
            r"
pub struct Example;
fn allowlisted_expression_macro() {
    let _: Vec<bool> = vec![!false, 1 != 2];
}
",
        )
        .expect("replace hostile expression macro with allowlisted standard usage");
        crate_inherent_member_inventories(&library, &library)
            .expect("unshadowed standard expression macros remain accepted");

        fs::write(
            &library,
            r"
pub struct Example;
fn install_members() {
    use hostile_api::Conceal as Debug;
    #[derive(Debug)]
    struct LocalTrigger;
}
",
        )
        .expect("replace expression macro with a function-local derive alias");
        let error = crate_inherent_member_inventories(&library, &library)
            .expect_err("a function-local derive alias must fail closed");
        assert!(error.contains("derive macro `Debug` whose bare name may be shadowed"));
    }

    #[test]
    fn documentation_debt_manifest_pins_identities_not_only_totals() {
        let expected = BTreeSet::from(["DisplayKind::Plot".to_string()]);
        let inventory = r"
<!-- rust-api-undocumented-variants-begin -->
```text
DisplayKind::Other
```
<!-- rust-api-undocumented-variants-end -->
";
        let error = check_identity_manifest(
            inventory,
            "rust-api-undocumented-variants",
            &expected,
            "undocumented facade variant",
        )
        .expect_err("same-size identity swap must fail");
        assert!(error.contains("missing [DisplayKind::Plot]"));
        assert!(error.contains("stale [DisplayKind::Other]"));
    }

    #[test]
    fn rendered_member_debt_and_documented_census_must_match() {
        let html = r#"
<h2 id="fields" class="fields">Fields</h2>
<span id="structfield.documented"><code>documented: u32</code></span>
<div class="docblock"><p>Documented.</p></div>
<span id="structfield.missing"><code>missing: u32</code></span>
<h2 id="variants" class="variants">Variants</h2>
<section id="variant.Documented"><h3>Documented</h3></section>
<div class="docblock"><p>Documented.</p></div>
<section id="variant.Missing"><h3>Missing</h3></section>
<div class="sub-variant"><h4>Fields</h4><div class="sub-variant-field">
<span><code>member: u32</code></span>
</div></div>
<h2 id="implementations">Implementations</h2>
<section id="method.documented"><h3>documented</h3></section>
<div class="docblock"><p>Documented.</p></div>
<section id="method.missing"><h3>missing</h3></section>
<section id="associatedconstant.DOCUMENTED"><h3>DOCUMENTED</h3></section>
<div class="docblock"><p>Documented.</p></div>
<h2 id="trait-implementations">Trait Implementations</h2>
"#;
        let source = SourceMemberInventory {
            variants: 2,
            named_enum_fields: 1,
            struct_fields: 2,
            methods: 2,
            associated_constants: 1,
            undocumented_named_enum_fields: 1,
            undocumented_struct_fields: 1,
            debt: MemberDocDebt {
                variants: 1,
                fields: 2,
                methods: 1,
                associated_constants: 0,
            },
            ..SourceMemberInventory::default()
        };
        let debt = member_doc_debt(html, source).expect("source and rendered census agree");
        assert_eq!(
            debt,
            MemberDocDebt {
                variants: 1,
                fields: 2,
                methods: 1,
                associated_constants: 0,
            }
        );

        let inventory =
            "<!-- rust-api-member-doc-debt: undocumented-variants=1 undocumented-fields=2 undocumented-methods=1 undocumented-associated-constants=0 -->";
        check_member_doc_debt_marker(inventory, debt).expect("matching census must pass");
        let error = check_member_doc_debt_marker(
            "<!-- rust-api-member-doc-debt: undocumented-variants=1 undocumented-fields=1 undocumented-methods=1 undocumented-associated-constants=0 -->",
            debt,
        )
        .expect_err("stale documentation debt must fail closed");
        assert!(error.contains("member-documentation debt changed"));

        let tuple_html = r#"
<h2 id="variants" class="variants">Variants</h2>
<section id="variant.Tuple"><h3>Tuple(u32)</h3></section>
<div class="docblock"><p>Documented variant.</p></div>
<div class="sub-variant"><h4>Tuple Fields</h4><div class="sub-variant-field">
<span><code>0: u32</code></span>
</div></div>
<h2 id="trait-implementations">Trait Implementations</h2>
"#;
        let tuple_source = SourceMemberInventory {
            variants: 1,
            tuple_enum_fields: 1,
            undocumented_tuple_enum_fields: 1,
            debt: MemberDocDebt {
                fields: 1,
                ..MemberDocDebt::default()
            },
            ..SourceMemberInventory::default()
        };
        assert_eq!(
            member_doc_debt(tuple_html, tuple_source).expect("tuple payload debt is source-backed"),
            MemberDocDebt {
                fields: 1,
                ..MemberDocDebt::default()
            }
        );
    }

    #[test]
    fn source_census_independently_includes_tuple_variant_payloads() {
        let file = syn::parse_file(
            r"
/// Example public enum.
pub enum Example {
    /// A fully documented tuple variant.
    Documented(
        /// The documented payload.
        u32,
    ),
    /// A tuple variant whose payload lacks documentation.
    Missing(u64),
}
",
        )
        .expect("fixture must parse");
        let definition = file.items.first().expect("fixture has one item");
        let inventory = inventory_source_members(definition, &file.items, "Example")
            .expect("source member inventory must succeed");

        assert_eq!(inventory.variants, 2);
        assert_eq!(inventory.tuple_enum_fields, 2);
        assert_eq!(inventory.undocumented_tuple_enum_fields, 1);
        assert_eq!(
            inventory.member_identities,
            BTreeSet::from([
                "field Documented.0".to_string(),
                "field Missing.0".to_string(),
                "variant Documented".to_string(),
                "variant Missing".to_string(),
            ])
        );
        assert_eq!(
            inventory.debt,
            MemberDocDebt {
                fields: 1,
                ..MemberDocDebt::default()
            }
        );
    }

    #[test]
    fn supported_member_manifest_pins_identities_not_only_totals() {
        let expected = BTreeSet::from([
            "field Example::value".to_string(),
            "method Example::read".to_string(),
        ]);
        let inventory = r"
<!-- rust-api-supported-members-begin -->
```text
field Example::replacement
method Example::read
```
<!-- rust-api-supported-members-end -->
";
        let error = check_identity_manifest(
            inventory,
            "rust-api-supported-members",
            &expected,
            "supported facade member",
        )
        .expect_err("a same-size member identity swap must require review");
        assert!(error.contains("missing [field Example::value]"));
        assert!(error.contains("stale [field Example::replacement]"));
    }

    fn feature_metadata(feature_packages: &[&str]) -> Value {
        let mut packages = vec![json!({
            "id": "workspace#pinewasm_xtask",
            "name": "pinewasm_xtask",
            "features": {}
        })];
        packages.extend(feature_packages.iter().map(|name| {
            json!({
                "id": format!("workspace#{name}"),
                "name": name,
                "features": { "default": [] }
            })
        }));
        json!({
            "workspace_members": packages
                .iter()
                .map(|package| package["id"].clone())
                .collect::<Vec<_>>(),
            "packages": packages
        })
    }

    #[test]
    fn minimal_feature_inventory_matches_every_feature_bearing_member() {
        let metadata = feature_metadata(&MINIMAL_FEATURE_PACKAGES);
        validate_minimal_feature_inventory(&metadata).unwrap();
    }

    #[test]
    fn minimal_feature_inventory_rejects_new_unclassified_member() {
        let mut packages = MINIMAL_FEATURE_PACKAGES.to_vec();
        packages.push("pinewasm_new_feature_crate");
        let error = validate_minimal_feature_inventory(&feature_metadata(&packages)).unwrap_err();
        assert!(error.contains("pinewasm_new_feature_crate"), "{error}");
    }

    fn with_member(mut metadata: Value, name: &str, features: Value) -> Value {
        let id = format!("workspace#{name}");
        metadata["workspace_members"]
            .as_array_mut()
            .unwrap()
            .push(json!(id));
        metadata["packages"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": id, "name": name, "features": features }));
        metadata
    }

    #[test]
    fn minimal_feature_inventory_ignores_extraction_only_features() {
        let metadata = with_member(
            feature_metadata(&MINIMAL_FEATURE_PACKAGES),
            "pinewasm_extracted",
            json!({ "aeneas": [] }),
        );
        validate_minimal_feature_inventory(&metadata).unwrap();
    }

    #[test]
    fn minimal_feature_inventory_classifies_extraction_feature_that_enables_more() {
        let metadata = with_member(
            feature_metadata(&MINIMAL_FEATURE_PACKAGES),
            "pinewasm_extracted",
            json!({ "aeneas": ["dep:serde"] }),
        );
        let error = validate_minimal_feature_inventory(&metadata).unwrap_err();
        assert!(error.contains("pinewasm_extracted"), "{error}");
    }

    #[test]
    fn minimal_feature_inventory_classifies_extraction_feature_beside_others() {
        let metadata = with_member(
            feature_metadata(&MINIMAL_FEATURE_PACKAGES),
            "pinewasm_extracted",
            json!({ "aeneas": [], "std": [] }),
        );
        let error = validate_minimal_feature_inventory(&metadata).unwrap_err();
        assert!(error.contains("pinewasm_extracted"), "{error}");
    }

    #[test]
    fn minimal_feature_inventory_rejects_missing_package_record() {
        let mut metadata = feature_metadata(&MINIMAL_FEATURE_PACKAGES);
        metadata["workspace_members"]
            .as_array_mut()
            .unwrap()
            .push(json!("workspace#missing"));
        let error = validate_minimal_feature_inventory(&metadata).unwrap_err();
        assert!(error.contains("workspace#missing"), "{error}");
    }
}
