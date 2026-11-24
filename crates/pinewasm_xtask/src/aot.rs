use crate::command::{
    artifact_cargo_home, copy_file, output, pinned_cargo, pinned_tool, pinned_toolchain,
    remove_cargo_build_overrides, repo_root, run, sha256_file,
};
use crate::TaskResult;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RUNTIME_VARIANTS: [(Option<&str>, &str); 4] = [
    (None, "pinewasm_aot_runtime.wasm"),
    (Some("ta"), "pinewasm_aot_runtime_ta.wasm"),
    (
        Some("standalone_wasm"),
        "pinewasm_aot_runtime_standalone_wasm.wasm",
    ),
    (Some("broker"), "pinewasm_aot_runtime_broker.wasm"),
];

pub(crate) fn rebuild_all() -> TaskResult {
    rebuild_selected(
        &["base", "ta", "standalone_wasm", "broker"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
}

pub(crate) fn rebuild_selected(selected: &std::collections::BTreeSet<String>) -> TaskResult {
    if selected.is_empty()
        || selected
            .iter()
            .any(|name| !["base", "ta", "standalone_wasm", "broker"].contains(&name.as_str()))
    {
        return Err("invalid runtime variant selection".into());
    }
    let root = repo_root()?;
    let runtime = root.join("crates/pinewasm_aot/runtime");
    for_each_selected_runtime_variant(&root, selected, |features, output_name, cargo_home| {
        let built = build_runtime(&root, features, cargo_home)?;
        install_runtime(&built, &runtime.join(output_name))
    })
}

/// The chart runtime (the runtime crate's `chart` feature) relative to the
/// repository: where [`build_chart_runtime`] leaves it.
const CHART_RUNTIME: &str = "target/aot-runtime-chart/pinewasm_aot_runtime_chart.wasm";

/// The variable that names the chart runtime to `pinewasm_aot`'s build
/// script, which links it into the compiler.
pub(crate) const CHART_RUNTIME_ENV: &str = "PINEWASM_AOT_CHART_RUNTIME";

/// Build the chart runtime from this checkout and return its path.
///
/// The four runtimes above are checked in, and CI holds each to a
/// byte-for-byte rebuild. This one holds the interpreter VM, so almost every
/// compiler change would change a checked-in copy. It is built with the
/// compiler instead, by the same recipe, and stays under `target/`.
pub(crate) fn build_chart_runtime() -> TaskResult<PathBuf> {
    let root = repo_root()?;
    let read = |path: &str| {
        fs::read_to_string(root.join(path)).map_err(|error| format!("cannot read {path}: {error}"))
    };
    let stray = lock_versions_outside(
        &read("crates/pinewasm_aot/runtime/Cargo.lock")?,
        &read("Cargo.lock")?,
    );
    if !stray.is_empty() {
        return Err(format!(
            "crates/pinewasm_aot/runtime/Cargo.lock builds the linked VM from versions the \
             workspace does not build the interpreter VM from: {}. Pin each with `cargo update \
             --manifest-path crates/pinewasm_aot/runtime/Cargo.toml -p NAME --precise VERSION`.",
            stray.join(", ")
        ));
    }
    let cargo_home = artifact_cargo_home(&root)?;
    let built = build_runtime(&root, Some("chart"), &cargo_home)?;
    let destination = root.join(CHART_RUNTIME);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    install_runtime(&built, &destination)?;
    Ok(destination)
}

/// The `name version` pairs of every package in a `Cargo.lock`.
fn lock_versions(lock: &str) -> std::collections::BTreeSet<(String, String)> {
    let mut versions = std::collections::BTreeSet::new();
    let mut name = None;
    for line in lock.lines() {
        let value = |key: &str| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(" = \""))
                .and_then(|rest| rest.strip_suffix('"'))
        };
        if let Some(found) = value("name") {
            name = Some(found.to_string());
        } else if let (Some(version), Some(package)) = (value("version"), name.take()) {
            versions.insert((package, version.to_string()));
        }
    }
    versions
}

/// The packages `runtime` locks at a version `workspace` does not, among
/// the packages both name. The chart runtime is the interpreter VM built a
/// second time; it is the same VM only if its dependencies are the same.
fn lock_versions_outside(runtime: &str, workspace: &str) -> Vec<String> {
    let workspace = lock_versions(workspace);
    lock_versions(runtime)
        .into_iter()
        .filter(|(name, version)| {
            workspace.iter().any(|(other, _)| other == name)
                && !workspace.contains(&(name.clone(), version.clone()))
        })
        .map(|(name, version)| format!("{name} {version}"))
        .collect()
}

/// `aot-runtime-chart`: build the chart runtime and print how to link it.
pub(crate) fn chart_runtime(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("aot-runtime-chart takes no arguments".into());
    }
    let built = build_chart_runtime()?;
    println!(
        "Built {}. Set {CHART_RUNTIME_ENV} to that path to link it into pinewasm_aot.",
        built.display()
    );
    Ok(())
}

/// `chart-test`: the linked chart module suite, against a chart runtime
/// built from this checkout. The suite, and the code that links the runtime,
/// exist only in a compiler built with it, so this is also where they are
/// linted, and a run that discovers no test fails.
pub(crate) fn chart_test(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("chart-test takes no arguments".into());
    }
    let root = repo_root()?;
    let built = build_chart_runtime()?;
    let mut lint = pinned_cargo(&root)?;
    run(lint.env(CHART_RUNTIME_ENV, &built).args([
        "clippy",
        "--locked",
        "-p",
        "pinewasm_aot",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    let command = || -> TaskResult<Command> {
        let mut cargo = pinned_cargo(&root)?;
        cargo.env(CHART_RUNTIME_ENV, &built).args([
            "test",
            "--locked",
            "-p",
            "pinewasm_aot",
            "--test",
            "chart_wasm",
        ]);
        Ok(cargo)
    };
    let listing = output(command()?.args(["--", "--list"]))?;
    if !String::from_utf8_lossy(&listing.stdout)
        .lines()
        .any(|line| line.ends_with(": test"))
    {
        return Err("chart-test discovered no linked chart module test".into());
    }
    run(&mut command()?)
}

/// Build the runtime crate with `features` in a fresh Cargo home and return
/// the module Cargo produced. Every runtime, checked in or not, is built by
/// this one recipe: the pinned toolchain, a fixed memory layout, remapped
/// paths, and a per-crate metadata salt instead of Cargo's path-derived one.
fn build_runtime(
    root: &Path,
    features: Option<&str>,
    cargo_home: &crate::command::FreshArtifactDirectory,
) -> TaskResult<PathBuf> {
    let runtime = root.join("crates/pinewasm_aot/runtime");
    let target_dir = root.join("target/aot-runtime");
    let target = "wasm32-unknown-unknown";
    let toolchain = pinned_toolchain(root)?;
    let rustc = pinned_tool(&toolchain, "rustc")?;
    let wrapper =
        env::current_exe().map_err(|error| format!("cannot locate xtask binary: {error}"))?;
    let wrapper_hash = sha256_file(&wrapper)?;
    // The chart runtime links `std` and registry crates, whose panic
    // messages name their source files: in the fresh Cargo home, under a
    // path that carries a nonce, and in the toolchain's own sources when it
    // has them. Its further remaps follow the repository's so that they are
    // the ones rustc applies to those files, as the package build's do. The
    // checked-in runtimes embed no such path and keep their exact flags.
    let chart = features == Some("chart");
    let chart_remaps = if chart {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME must be set".to_string())?;
        let rustup_home =
            crate::package::canonical_env_path("RUSTUP_HOME", home.join(".rustup"), "rustup home")?;
        format!(
            "--remap-path-prefix={}=/cargo-home --remap-path-prefix={}=/rustup-home ",
            cargo_home.path().display(),
            rustup_home.display()
        )
    } else {
        String::new()
    };
    let rustflags = format!(
        "--remap-path-prefix={}=/workspace \
{chart_remaps}--cfg=pinewasm_aot_wrapper_fingerprint_{wrapper_hash} \
-C link-arg=--global-base=4096 \
-C link-arg=-zstack-size=4096 \
-C link-arg=--export=__heap_base \
-C link-arg=--export=__stack_pointer",
        root.display()
    );
    // The chart runtime has its own profile in the runtime manifest, which
    // favors speed where the checked-in runtimes favor size.
    let profile = if chart { "chart" } else { "release" };
    let mut command = pinned_cargo(root)?;
    remove_cargo_build_overrides(&mut command);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(runtime.join("Cargo.toml"));
    if chart {
        command.args(["--profile", profile]);
    } else {
        command.arg("--release");
    }
    command
        .args(["--target", target, "--target-dir"])
        .arg(&target_dir)
        .args(["--locked", "--no-default-features"])
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CARGO_HOME", ".")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("RUSTFLAGS", &rustflags)
        .env("CARGO_INCREMENTAL", "0")
        .env("RUSTC", &rustc)
        .env("RUSTC_WRAPPER", &wrapper)
        .env("PINEWASM_XTASK_RUSTC_WRAPPER", "1");
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    cargo_home.run(&mut command)?;
    Ok(target_dir
        .join(target)
        .join(profile)
        .join("pinewasm_aot_runtime.wasm"))
}

/// Copy a built runtime to `destination`, readable and not executable.
fn install_runtime(built: &Path, destination: &Path) -> TaskResult {
    copy_file(built, destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o644)).map_err(|error| {
            format!(
                "cannot set permissions on {}: {error}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

/// Run one action per checked-in runtime with a newly allocated Cargo home.
///
/// Keeping allocation in this loop is a release invariant: a dependency or
/// build script may mutate Cargo's extracted registry/Git sources, and no such
/// mutation may cross from one feature variant into the next.
#[cfg(all(test, unix))]
fn for_each_runtime_variant(
    root: &Path,
    action: impl FnMut(
        Option<&'static str>,
        &'static str,
        &crate::command::FreshArtifactDirectory,
    ) -> TaskResult,
) -> TaskResult {
    let selected = ["base", "ta", "standalone_wasm", "broker"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for_each_selected_runtime_variant(root, &selected, action)
}

fn for_each_selected_runtime_variant(
    root: &Path,
    selected: &std::collections::BTreeSet<String>,
    mut action: impl FnMut(
        Option<&'static str>,
        &'static str,
        &crate::command::FreshArtifactDirectory,
    ) -> TaskResult,
) -> TaskResult {
    for (features, output_name) in RUNTIME_VARIANTS {
        if !selected.contains(features.unwrap_or("base")) {
            continue;
        }
        let cargo_home = artifact_cargo_home(root)?;
        action(features, output_name, &cargo_home)?;
    }
    Ok(())
}

pub(crate) fn rustc_wrapper(mut args: Vec<String>) -> TaskResult {
    if args.is_empty() {
        return Err("rustc wrapper requires RUSTC [RUSTC_ARGS...]".into());
    }
    let rustc = args.remove(0);
    let Some(crate_name) = crate_name(&args) else {
        return run(Command::new(rustc).args(args));
    };
    let package = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "package".into());
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0".into());
    let salt = sanitize(&format!(
        "pinewasm_aot_runtime_v1_{package}_{version}_{crate_name}"
    ));
    let filtered = filter_rustc_args(args);
    run(Command::new(rustc)
        .args(filtered)
        .args(["-C", &format!("metadata={salt}")]))
}

fn crate_name(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .map(|pair| pair[1].as_str())
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn filter_rustc_args(args: Vec<String>) -> Vec<OsString> {
    let mut filtered = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index].starts_with("--cfg=pinewasm_aot_wrapper_fingerprint_") {
            index += 1;
            continue;
        }
        if args[index] == "-C" && index + 1 < args.len() && args[index + 1].starts_with("metadata=")
        {
            index += 2;
            continue;
        }
        if args[index].starts_with("-Cmetadata=") {
            index += 1;
            continue;
        }
        filtered.push(OsString::from(&args[index]));
        index += 1;
    }
    filtered
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::command::temp_dir;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    #[test]
    fn a_runtime_lock_version_the_workspace_lacks_is_reported() {
        let workspace = "[[package]]\nname = \"serde_json\"\nversion = \"1.0.151\"\n\n\
                         [[package]]\nname = \"syn\"\nversion = \"2.0.119\"\n\n\
                         [[package]]\nname = \"syn\"\nversion = \"3.0.3\"\n";
        let agrees = "[[package]]\nname = \"serde_json\"\nversion = \"1.0.151\"\n\n\
                      [[package]]\nname = \"syn\"\nversion = \"3.0.3\"\n\n\
                      [[package]]\nname = \"ryu\"\nversion = \"1.0.23\"\n";
        assert!(lock_versions_outside(agrees, workspace).is_empty());
        let drifted = agrees.replace("1.0.151", "1.0.154");
        assert_eq!(
            lock_versions_outside(&drifted, workspace),
            ["serde_json 1.0.154"]
        );
    }

    /// The committed lockfiles agree, so the chart runtime builds.
    #[test]
    fn the_runtime_lock_matches_the_workspace_lock() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |path: &str| fs::read_to_string(root.join(path)).unwrap();
        assert_eq!(
            lock_versions_outside(
                &read("crates/pinewasm_aot/runtime/Cargo.lock"),
                &read("Cargo.lock")
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn selected_runtime_rebuild_does_not_visit_other_variants() {
        let root = temp_dir("aot-selected-cargo-home").unwrap();
        let mut visited = Vec::new();
        for_each_selected_runtime_variant(
            root.path(),
            &BTreeSet::from(["broker".into()]),
            |feature, output, home| {
                assert!(home.path().exists());
                visited.push((feature, output));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            visited,
            vec![(Some("broker"), "pinewasm_aot_runtime_broker.wasm")]
        );
    }

    #[test]
    fn every_runtime_variant_gets_an_empty_fresh_cargo_home() {
        let root = temp_dir("aot-variant-cargo-homes").unwrap();
        let mut paths = BTreeSet::new();
        #[cfg(unix)]
        let mut previous: Option<PathBuf> = None;
        let mut variants = Vec::new();

        for_each_runtime_variant(root.path(), |features, output_name, cargo_home| {
            let cargo_home_path = cargo_home.path();
            #[cfg(unix)]
            if let Some(previous) = previous.take() {
                assert!(
                    !previous.exists(),
                    "the previous variant's Cargo home must be removed before the next build"
                );
            }
            assert!(
                !cargo_home_path.join("registry").exists(),
                "a variant must not inherit extracted registry sources"
            );
            assert!(paths.insert(cargo_home_path.to_path_buf()));

            let poison = cargo_home_path.join("registry/src/hostile-1.0.0/src/lib.rs");
            fs::create_dir_all(poison.parent().unwrap()).unwrap();
            fs::write(&poison, "compile_error!(\"inter-variant poison\");\n").unwrap();
            #[cfg(unix)]
            {
                previous = Some(cargo_home_path.to_path_buf());
            }
            variants.push((features, output_name));
            Ok(())
        })
        .unwrap();

        assert_eq!(variants, RUNTIME_VARIANTS);
        assert_eq!(paths.len(), RUNTIME_VARIANTS.len());
        #[cfg(unix)]
        assert!(
            previous.is_some_and(|path| !path.exists()),
            "the final variant's Cargo home must be removed"
        );
    }
}
