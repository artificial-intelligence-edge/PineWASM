#![cfg(unix)]
#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pinewasm-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn initialize_artifact_repository(root: &Path) {
    fs::create_dir_all(root).expect("create fixture repository");
    let initialized = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .status()
        .expect("initialize fixture repository");
    assert!(initialized.success());
    fs::write(
        root.join("rust-toolchain.toml"),
        include_str!("../../../rust-toolchain.toml"),
    )
    .expect("write repository toolchain pin");
}

fn assert_runner_uses_open_directory(directory: &File) {
    let status = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
        .args([
            "run-in-artifact-directory",
            "--",
            "/bin/sh",
            "-c",
            "test \"$(cat \"$CARGO_HOME/build-input\")\" = trusted-cargo && \
             test \"$(cat \"$BINDGEN_ROOT/build-input\")\" = trusted-helper",
        ])
        .env("CARGO_HOME", "cargo-home")
        .env("BINDGEN_ROOT", "wasm-bindgen-cli")
        .stdin(Stdio::from(directory.try_clone().expect("clone directory")))
        .status()
        .expect("run descriptor-anchored helper");
    assert!(status.success());
}

/// Cargo resolves a relative `CARGO_HOME` itself, unlike the shell-read check.
/// Compile a crate that requires the trusted home's rustflags, while a decoy
/// home selects a nonexistent compiler. This detects redirecting Cargo after
/// the namespace replacement without downloading dependencies.
fn assert_cargo_uses_open_directory(directory: File, fixture: &Path) {
    fs::create_dir(fixture).expect("create Cargo fixture");
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"artifact-directory-check\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         [lib]\npath = \"lib.rs\"\n[workspace]\n",
    )
    .expect("write fixture manifest");
    fs::write(
        fixture.join("Cargo.lock"),
        "version = 3\n[[package]]\nname = \"artifact-directory-check\"\nversion = \"0.0.0\"\n",
    )
    .expect("write fixture lockfile");
    fs::write(
        fixture.join("lib.rs"),
        "#![allow(unexpected_cfgs)]\n\
         #[cfg(not(trusted_artifact_home))]\n\
         compile_error!(\"Cargo did not use the handle-anchored home\");\n",
    )
    .expect("write fixture source");

    let result = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
        .args(["run-in-artifact-directory", "--", env!("CARGO")])
        .args(["build", "--offline", "--locked", "--manifest-path"])
        .arg(fixture.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(fixture.join("target"))
        .env("CARGO_HOME", "cargo-home")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .stdin(Stdio::from(directory))
        .output()
        .expect("run Cargo through descriptor-anchored helper");
    assert!(
        result.status.success(),
        "Cargo build failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(fixture
        .join("target/debug/libartifact_directory_check.rlib")
        .is_file());
}

fn seed_workspace(workspace: &Path, cargo: &str, helper: &str) {
    fs::create_dir_all(workspace.join("cargo-home")).expect("create Cargo home");
    fs::create_dir_all(workspace.join("wasm-bindgen-cli")).expect("create helper root");
    fs::write(workspace.join("cargo-home/build-input"), cargo).expect("write Cargo marker");
    fs::write(workspace.join("wasm-bindgen-cli/build-input"), helper).expect("write helper marker");
    let config = if cargo.starts_with("trusted-") {
        "[build]\nrustflags = [\"--cfg\", \"trusted_artifact_home\"]\n"
    } else {
        "[build]\nrustc = \"/nonexistent-hostile-artifact-compiler\"\n"
    };
    fs::write(workspace.join("cargo-home/config.toml"), config).expect("write Cargo config");
}

#[test]
fn build_lookup_stays_anchored_when_namespace_parent_is_replaced() {
    use std::os::unix::fs::symlink;

    let root = TestDirectory::new("artifact-runner-parent-root");
    let external = TestDirectory::new("artifact-runner-parent-external");
    let namespace = root.path().join("artifact-wasm-workspaces");
    let workspace = namespace.join("build-under-test");
    fs::create_dir(&namespace).expect("create namespace");
    fs::create_dir(&workspace).expect("create workspace");
    seed_workspace(&workspace, "trusted-cargo\n", "trusted-helper\n");
    let directory = File::open(&workspace).expect("open workspace handle");

    let detached = root.path().join("artifact-wasm-workspaces-detached");
    fs::rename(&namespace, &detached).expect("detach namespace");
    symlink(external.path(), &namespace).expect("replace namespace with symlink");
    let decoy = external.path().join("build-under-test");
    fs::create_dir(&decoy).expect("create decoy workspace");
    seed_workspace(&decoy, "hostile-cargo\n", "hostile-helper\n");

    assert_runner_uses_open_directory(&directory);
    assert_cargo_uses_open_directory(directory, &root.path().join("fixture"));

    assert_eq!(
        fs::read_to_string(decoy.join("cargo-home/build-input")).expect("read decoy Cargo marker"),
        "hostile-cargo\n"
    );
    fs::remove_file(namespace).expect("remove namespace symlink");
}

#[test]
fn build_lookup_stays_anchored_when_final_entry_is_replaced() {
    use std::os::unix::fs::symlink;

    let root = TestDirectory::new("artifact-runner-entry-root");
    let external = TestDirectory::new("artifact-runner-entry-external");
    let workspace = root.path().join("build-under-test");
    fs::create_dir(&workspace).expect("create workspace");
    seed_workspace(&workspace, "trusted-cargo\n", "trusted-helper\n");
    let directory = File::open(&workspace).expect("open workspace handle");

    let detached = root.path().join("build-under-test-detached");
    fs::rename(&workspace, &detached).expect("detach workspace");
    seed_workspace(external.path(), "hostile-cargo\n", "hostile-helper\n");
    symlink(external.path(), &workspace).expect("replace workspace with symlink");

    assert_runner_uses_open_directory(&directory);
    assert_cargo_uses_open_directory(directory, &root.path().join("fixture"));

    assert_eq!(
        fs::read_to_string(external.path().join("wasm-bindgen-cli/build-input"))
            .expect("read decoy helper marker"),
        "hostile-helper\n"
    );
    fs::remove_file(workspace).expect("remove workspace symlink");
}

#[test]
fn artifact_build_rejects_stale_ancestor_config_before_cargo_startup() {
    for ancestor in [
        "target",
        "target/xtask",
        "target/xtask/artifact-cargo-homes",
    ] {
        let root = TestDirectory::new("artifact-ancestor-config-command");
        initialize_artifact_repository(root.path());

        let configuration_directory = root.path().join(ancestor).join(".cargo");
        fs::create_dir_all(&configuration_directory).expect("create ignored Cargo configuration");
        let configuration = configuration_directory.join("config.toml");
        let poison =
            "[env]\nPINEWASM_SOURCE_REVISION = { value = 'stale-config-revision', force = true }\n";
        fs::write(&configuration, poison).expect("write stale forced environment");

        let result = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
            .arg("aot-runtime-all")
            .current_dir(root.path())
            .env("PINEWASM_SOURCE_REVISION", "trusted-release-revision")
            .output()
            .expect("run actual artifact builder");
        assert!(!result.status.success());
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(
            diagnostic.contains("ignored Cargo configuration"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains(ancestor), "{diagnostic}");
        assert_eq!(fs::read_to_string(configuration).unwrap(), poison);
        assert!(!root.path().join("target/aot-runtime").exists());
    }
}

#[test]
fn artifact_build_rejects_external_parent_release_configuration() {
    for nested in ["repository", "a/b/repository"] {
        for name in ["config", "config.toml"] {
            let outer = TestDirectory::new("artifact-external-config-command");
            let root = outer.path().join(nested);
            initialize_artifact_repository(&root);
            let cargo = outer.path().join(".cargo");
            fs::create_dir(&cargo).expect("create parent Cargo directory");
            let configuration = cargo.join(name);
            let poison = "[profile.release]\nopt-level = 0\ndebug = true\n";
            fs::write(&configuration, poison).expect("write external release configuration");

            let result = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
                .arg("aot-runtime-all")
                .current_dir(&root)
                .output()
                .expect("run actual artifact builder under configured parent");
            assert!(!result.status.success());
            let diagnostic = String::from_utf8_lossy(&result.stderr);
            assert!(
                diagnostic.contains("external Cargo configuration"),
                "{diagnostic}"
            );
            assert!(
                diagnostic.contains(configuration.to_str().unwrap()),
                "{diagnostic}"
            );
            assert_eq!(fs::read_to_string(configuration).unwrap(), poison);
            assert!(!root.join("target").exists());
        }
    }
}

#[test]
fn docs_check_rejects_cargo_home_source_override_before_metadata() {
    let outer = TestDirectory::new("docs-source-config-command");
    let root = outer.path().join("repository");
    initialize_artifact_repository(&root);
    let cargo_home = outer.path().join("cargo-home");
    fs::create_dir(&cargo_home).expect("create hostile Cargo home");
    let configuration = cargo_home.join("config.toml");
    fs::write(
        &configuration,
        "[source.crates-io]\nreplace-with = 'hostile'\n\
         [source.hostile]\ndirectory = 'vendor'\n",
    )
    .expect("write hostile source configuration");

    let result = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
        .arg("docs-check")
        .current_dir(&root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("run actual docs gate");
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(
        diagnostic.contains("dependency source override"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(configuration.to_str().unwrap()),
        "{diagnostic}"
    );
    assert!(
        !root.join("target").exists(),
        "the gate must reject before Cargo metadata or Rustdoc starts"
    );
}

#[test]
fn artifact_build_rejects_legacy_root_config_hidden_from_git() {
    for exclude_kind in ["repository", "external"] {
        let outer = TestDirectory::new("artifact-hidden-root-config-command");
        let root = outer.path().join("repository");
        initialize_artifact_repository(&root);
        fs::create_dir(root.join(".cargo")).expect("create repository Cargo directory");
        let reviewed = "[profile.release]\nopt-level = 3\n";
        fs::write(root.join(".cargo/config.toml"), reviewed).expect("write reviewed configuration");
        let added = Command::new("git")
            .current_dir(&root)
            .args(["add", "--", "rust-toolchain.toml", ".cargo/config.toml"])
            .status()
            .expect("stage fixture files");
        assert!(added.success());
        let committed = Command::new("git")
            .current_dir(&root)
            .args([
                "-c",
                "user.name=Artifact Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--signoff",
                "-m",
                "Add reviewed fixture configuration",
            ])
            .status()
            .expect("commit fixture files");
        assert!(committed.success());
        let exclude = if exclude_kind == "repository" {
            root.join(".git/info/exclude")
        } else {
            let path = outer.path().join("global-excludes");
            let configured = Command::new("git")
                .current_dir(&root)
                .args(["config", "core.excludesFile"])
                .arg(&path)
                .status()
                .expect("configure fixture external excludes file");
            assert!(configured.success());
            path
        };
        fs::write(exclude, ".cargo/config\n").expect("hide legacy configuration from Git");
        let legacy = root.join(".cargo/config");
        let poison = "[profile.release]\nopt-level = 0\ndebug = true\n";
        fs::write(&legacy, poison).expect("write hidden legacy release override");
        let status = Command::new("git")
            .current_dir(&root)
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .output()
            .expect("check fixture worktree");
        assert!(status.status.success());
        assert!(status.stdout.is_empty(), "fixture must appear clean to Git");

        let result = Command::new(env!("CARGO_BIN_EXE_pinewasm_xtask"))
            .arg("aot-runtime-all")
            .current_dir(&root)
            .output()
            .expect("run artifact builder with hidden legacy configuration");
        assert!(!result.status.success());
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(
            diagnostic.contains("legacy repository Cargo configuration"),
            "{diagnostic}"
        );
        assert_eq!(fs::read_to_string(legacy).unwrap(), poison);
        assert_eq!(
            fs::read_to_string(root.join(".cargo/config.toml")).unwrap(),
            reviewed
        );
        assert!(!root.join("target").exists());
    }
}

#[test]
fn builtin_cargo_bootstrap_is_not_shadowed_by_xtask_alias() {
    let outer = TestDirectory::new("artifact-alias-bootstrap-command");
    let root = outer.path().join("repository");
    initialize_artifact_repository(&root);
    fs::create_dir(root.join(".cargo")).expect("create repository Cargo directory");
    let legacy = root.join(".cargo/config");
    let noop = outer.path().join("noop");
    fs::create_dir(&noop).expect("create no-op Cargo fixture");
    fs::write(
        noop.join("Cargo.toml"),
        "[package]\nname = 'artifact-noop'\nversion = '0.0.0'\nedition = '2021'\n\
         [[bin]]\nname = 'artifact-noop'\npath = 'main.rs'\n[workspace]\n",
    )
    .expect("write no-op manifest");
    fs::write(noop.join("main.rs"), "fn main() {}\n").expect("write no-op source");
    let manifest = noop.join("Cargo.toml");
    let alias_args = serde_json::to_string(&[
        "run",
        "--offline",
        "--manifest-path",
        manifest.to_str().unwrap(),
        "--",
    ])
    .expect("serialize portable Cargo alias array");
    let alias = format!("[alias]\nxtask = {alias_args}\n");
    fs::write(&legacy, &alias).expect("write no-op alias override");

    let shadowed = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args(["xtask", "release-check"])
        .output()
        .expect("invoke shadowed Cargo alias");
    assert!(
        shadowed.status.success(),
        "the no-op alias must falsely succeed: {}",
        String::from_utf8_lossy(&shadowed.stderr)
    );
    assert!(!root.join("target").exists());

    // Build and launch the real xtask through Cargo's built-in command, with
    // separate output to avoid locking the parent test build. The small AOT
    // command reaches the same artifact preflight without needing a complete
    // release fixture; Cargo never resolves the overridden `xtask` alias.
    let direct = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args([
            "run",
            "--offline",
            "--locked",
            "-p",
            "pinewasm_xtask",
            "--manifest-path",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .args(["--", "aot-runtime-all"])
        .env("CARGO_TARGET_DIR", outer.path().join("bootstrap-target"))
        .output()
        .expect("invoke built-in Cargo bootstrap");
    assert!(!direct.status.success());
    let diagnostic = String::from_utf8_lossy(&direct.stderr);
    assert!(
        diagnostic.contains("legacy repository Cargo configuration"),
        "{diagnostic}"
    );
    assert_eq!(fs::read_to_string(legacy).unwrap(), alias);
    assert!(!root.join("target").exists());
}
