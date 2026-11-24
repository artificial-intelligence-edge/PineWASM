//! Rust-native repository automation.
//!
//! Keeping policy, packaging, and reproducibility checks here makes them run
//! under the same pinned toolchain as the compiler. The repository's developer
//! workflows are implemented here instead of in shell scripts or task files.

#![forbid(unsafe_code)]

mod aot;
mod artifact_notice;
mod benchmark_run;
mod benchmark_subject;
mod boundary;
mod ci;
mod command;
mod coverage;
mod dco;
mod installers;
mod licenses;
mod package;
mod policy;
mod repository;
mod runtime_safety;
mod secrets;
mod terminology;
mod version;
mod workflow;

use std::env;
use std::process::ExitCode;

type TaskResult<T = ()> = Result<T, String>;

fn usage() -> &'static str {
    r"usage: cargo xtask <command> [args]

repository workflows:
  setup                  Install pinned Wasm tooling and build the package
  clean                  Delete Cargo and generated Wasm build artifacts
  hooks                  Install the pre-commit hook that checks staged files
  build                  Build the native workspace
  terminology-check      Reject disallowed names in source and paths
  wasm [--migrate]       Build target/wasm-pkg with pinned tooling; --migrate
                         adds migrateToV6 (migrates Pine Script® v5 source to v6) to the package
  wasm-test              Exercise generated package boundaries with Node.js
  wasm-watch [--once]    Rebuild Wasm when Rust or Cargo sources change
  aot-runtime-all        Rebuild all four checked-in runtime modules
  aot-runtime-chart      Build the chart runtime, which is linked into the
                         compiler when it is built and is not checked in
  chart-test             Build the chart runtime and run the linked chart
                         module suite against it
  aot-unsafe-inventory   Verify the excluded runtime's generated unsafe-site inventory
  runtime-safety-miri    Run the bounded Miri suite with the dedicated nightly pin
  runtime-safety-asan    Run shared/runtime-native tests under AddressSanitizer
  aot-census             Run the native and standalone WebAssembly fixture censuses
  fuzz-smoke             Run bounded deterministic adversarial campaigns
  test                   Run default-workspace and minimal-feature Rust suites
                         plus the embedding smoke test
  feature-test           Run all-feature and minimal-feature Rust suites
  check                  Run the repository quality bar
  fmt                    Format all Rust sources
  fmt-check              Check Rust formatting without writing
  lint                   Run Clippy with warnings denied
  docs-check             Build API docs with warnings denied
  feature-check          Check supported minimal-feature configurations
  coverage-check         Validate the Pine Script coverage matrix and its repository links
  benchmark-subject-check
                         Validate the subject-tier manifest and workloads
  benchmark <command>    Prepare, probe, run, or summarize local comparisons
  benchmark-subject <command>
                         Check, probe, run, or summarize PineWASM's own
                         timing boundaries against expected series computed locally
  public-boundary        Validate the publishable repository contents
  workflow-pin-check     Reject mutable GitHub Actions references
  dependency-policy      Audit dependencies and generated license notices
  artifact-notice --out DIR
                         Write the generated-artifact notice bundle
  bump-version major|minor|patch
                         Step the compiler version; every compiler change
                         needs one
  check-compiler-version [--git-range BASE HEAD | --first-parent BASE HEAD]
                         Require a canonical compiler version and, for a
                         range that changes compiler inputs, one SemVer step
                         forward;
                         --first-parent holds each first-parent step of the range
                         to that rule
  policy-check           Verify public licensing terms and artifact notices
  leak-scan              Install pinned Gitleaks and scan publishable files
  ci-plan                Select affected CI tasks from immutable revisions
  ci-range-base          Find the base of the commits a scheduled or
                         dispatched run checks: the head of the last such
                         run that passed, or the pinned floor
  ci-run --stage STAGE   Execute selected Rust validation
  ci-reuse --lane LANE   Look up a prior successful result for a CI job
  ci-record --lane LANE  Record the result of a completed CI job
  ci-verify              Require every selected job to succeed
  ci                     Run the complete local backend sweep
  release-preflight      Require a clean worktree before release validation
  release-check          Run every local generator and release gate
  language-server        Run the language server over stdio
  check-file FILE        Check one Pine Script source file

low-level commands:
  build-wasm-package
  check-coverage
  check-dco [--self-test | BASE_SHA HEAD_SHA]
  check-public-boundary [--self-test | --staged | --repo DIR | --git-range BASE HEAD]
  check-secrets [--self-test | --staged | --git-range BASE HEAD]
  check-workflow-pins
  aot-unsafe-inventory [--write]
  finalize-wasm-package [PACKAGE_DIR]
  generate-licenses [--check] [--repo DIR]
  install-actionlint VERSION PREFIX
  install-binaryen VERSION PREFIX
  install-gitleaks VERSION PREFIX
  install-hook

Run `cargo xtask help` to print this list."
}

fn dispatch(mut args: Vec<String>) -> TaskResult {
    if args.first().map(String::as_str) == Some("run-in-artifact-directory") {
        args.remove(0);
        return command::run_in_artifact_directory(&args);
    }
    if env::var_os("PINEWASM_XTASK_RUSTC_WRAPPER").is_some() {
        return aot::rustc_wrapper(args);
    }
    if args.is_empty()
        && env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(|name| name == "pre-commit"))
            .unwrap_or(false)
    {
        return boundary::check(&["--staged".to_string()]);
    }
    if args.is_empty() {
        println!("{}", usage());
        return Ok(());
    }
    let command = args.remove(0);
    match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        "setup" => repository::setup(&args),
        "clean" => repository::clean(&args),
        "hooks" => repository::hooks(&args),
        "build" | "b" => repository::build(&args),
        "wasm" | "build-wasm-package" => repository::wasm(&args),
        "wasm-test" => repository::wasm_test(&args),
        "wasm-watch" => repository::wasm_watch(&args),
        "aot-runtime-all"
        | "aot-runtime"
        | "aot-runtime-ta"
        | "aot-runtime-standalone-wasm"
        | "aot-runtime-broker" => repository::aot_runtime_all(&args),
        "aot-runtime-chart" => aot::chart_runtime(&args),
        "chart-test" => aot::chart_test(&args),
        "aot-census" => repository::aot_census(&args),
        "terminology-check" => terminology::check(&args),
        "fuzz-smoke" => repository::fuzz_smoke(&args),
        "aot-unsafe-inventory" => runtime_safety::check(&args),
        "runtime-safety-miri" => runtime_safety::miri(&args),
        "runtime-safety-asan" => runtime_safety::address_sanitizer(&args),
        "test" | "test-rust" | "t" => repository::test(&args),
        "feature-test" => repository::feature_test(&args),
        "check" | "c" => repository::check(&args),
        "fmt" => repository::format(&args),
        "fmt-check" => repository::format_check(&args),
        "lint" => repository::lint(&args),
        "docs-check" => repository::docs_check(&args),
        "feature-check" => repository::feature_check(&args),
        "coverage-check" => repository::coverage_check(&args),
        "public-boundary" => repository::public_boundary(&args),
        "workflow-pin-check" => repository::workflow_pin_check(&args),
        "dependency-policy" => repository::dependency_policy(&args),
        "artifact-notice" => artifact_notice::run(&args),
        "policy-check" => policy::public_check(&args),
        "bump-version" => version::bump(&args),
        "check-compiler-version" => version::check(&args),
        "leak-scan" => repository::leak_scan(&args),
        "ci" => repository::ci(&args),
        "ci-plan" => ci::plan(&args),
        "ci-range-base" => ci::range_base(&args),
        "ci-run" => ci::execute(&args),
        "ci-reuse" => ci::reuse(&args),
        "ci-record" => ci::record(&args),
        "ci-verify" => ci::verify(&args),
        "release-preflight" => repository::release_preflight(&args),
        "release-check" => repository::release_check(&args),
        "language-server" => repository::language_server(&args),
        "check-file" => repository::check_file(&args),
        "benchmark" => benchmark_run::run(&args),
        "benchmark-subject" => benchmark_subject::run(&args),
        "benchmark-subject-check" => benchmark_subject::check(&args),
        "check-coverage" => coverage::check(),
        "check-dco" => dco::check(&args),
        "check-public-boundary" => boundary::check(&args),
        "check-secrets" => secrets::check(&args),
        "check-workflow-pins" => workflow::check(),
        "finalize-wasm-package" => package::finalize(&args),
        "generate-licenses" => licenses::run(&args),
        "install-actionlint" => installers::install_actionlint(&args),
        "install-binaryen" => installers::install_binaryen(&args),
        "install-gitleaks" => installers::install_gitleaks(&args),
        "install-hook" => boundary::install_hook(),
        _ => Err(format!("unknown xtask command: {command}\n{}", usage())),
    }
}

fn main() -> ExitCode {
    match dispatch(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}
