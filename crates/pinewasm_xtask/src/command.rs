use crate::TaskResult;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(unix)]
use rustix::fs::{
    fstat, mkdirat, open, openat, statat, unlinkat, AtFlags, Dir, FileType, Mode, OFlags,
};
#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

pub(crate) fn repo_root() -> TaskResult<PathBuf> {
    let output = output(Command::new("git").args(["rev-parse", "--show-toplevel"]))?;
    let root = String::from_utf8(output.stdout)
        .map_err(|error| format!("repository path is not UTF-8: {error}"))?;
    Ok(PathBuf::from(root.trim()))
}

pub(crate) fn output(command: &mut Command) -> TaskResult<Output> {
    let rendered = render(command);
    let result = command
        .output()
        .map_err(|error| format!("could not run {rendered}: {error}"))?;
    if result.status.success() {
        Ok(result)
    } else {
        Err(format!(
            "command failed ({rendered}):\n{}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
    }
}

pub(crate) fn output_allow_failure(command: &mut Command) -> TaskResult<Output> {
    let rendered = render(command);
    command
        .output()
        .map_err(|error| format!("could not run {rendered}: {error}"))
}

pub(crate) fn run(command: &mut Command) -> TaskResult {
    let rendered = render(command);
    let status = command
        .status()
        .map_err(|error| format!("could not run {rendered}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command failed ({rendered}) with {status}"))
    }
}

pub(crate) fn render(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    parts.join(" ")
}

pub(crate) fn text(command: &mut Command) -> TaskResult<String> {
    let result = output(command)?;
    String::from_utf8(result.stdout)
        .map_err(|error| format!("command output is not UTF-8: {error}"))
}

pub(crate) fn git(root: &Path, args: &[&str]) -> TaskResult<String> {
    text(Command::new("git").arg("-C").arg(root).args(args))
}

pub(crate) fn git_bytes(root: &Path, args: &[&str]) -> TaskResult<Vec<u8>> {
    Ok(output(Command::new("git").arg("-C").arg(root).args(args))?.stdout)
}

/// Whether `ancestor` is `descendant` or one of its ancestors. An unknown
/// commit is an error, never a "no".
pub(crate) fn git_is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> TaskResult<bool> {
    let result = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "merge-base",
        "--is-ancestor",
        ancestor,
        descendant,
    ]))?;
    match result.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "cannot compare {ancestor} with {descendant}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )),
    }
}

pub(crate) fn pinned_toolchain(root: &Path) -> TaskResult<String> {
    let source = fs::read_to_string(root.join("rust-toolchain.toml"))
        .map_err(|error| format!("cannot read rust-toolchain.toml: {error}"))?;
    let channel = source
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "channel").then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "unable to read the pinned Rust toolchain".to_string())?;
    if let Ok(expected) = env::var("PINEWASM_RUST_VERSION") {
        if expected != channel {
            return Err(format!(
                "PINEWASM_RUST_VERSION={expected} disagrees with rust-toolchain.toml ({channel})"
            ));
        }
    }
    Ok(channel)
}

/// Resolve the repository's pinned Cargo binary and the environment every
/// child tool needs in order to keep using that same toolchain.
///
/// Calling `rustup run <toolchain> cargo` is not sufficient on its own:
/// Cargo discovers helpers such as `rustc` and `clippy-driver` through
/// `PATH`. A system toolchain earlier on `PATH` can therefore pair its helper
/// with the pinned compiler and fail (or, worse, make local and CI checks use
/// different components). Keep the resolved toolchain directory first and
/// also set `RUSTUP_TOOLCHAIN` for repository-owned artifact helpers.
pub(crate) struct PinnedToolchain {
    pub(crate) channel: String,
    pub(crate) cargo: PathBuf,
    pub(crate) path: OsString,
}

/// Environment variables that can make Cargo (or one of its standard
/// subcommands) ignore the compiler tools selected by the repository pin.
///
/// Cargo configuration files are a separate, user-controlled boundary. These
/// variables are removed because inheriting them accidentally is common in
/// compiler-development shells and CI images, and because they would defeat
/// the exact binary plus `PATH` selection performed below.
const RUST_TOOL_OVERRIDES: [&str; 10] = [
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTDOC",
    "CLIPPY_DRIVER_PATH",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTDOC",
    "RUSTFMT",
];

/// Cargo environment variables that can override a repository-owned build
/// profile, target, target directory, or incremental setting.
///
/// Release artifacts must derive those inputs from checked-in manifests and
/// explicit command arguments. Keep this separate from
/// [`remove_rust_tool_overrides`]: ordinary developer commands may reasonably
/// inherit profile settings, while reproducible artifact builders may not.
pub(crate) fn remove_cargo_build_overrides(command: &mut Command) {
    for (name, _) in env::vars_os() {
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if is_cargo_build_override(name_text) {
            command.env_remove(name);
        }
    }
}

/// Prevent registry dependency build scripts from discovering the enclosing
/// project's Git identity. A dependency's producer version must describe that
/// dependency, not the checkout in which Cargo happened to unpack it.
pub(crate) fn isolate_dependency_git(command: &mut Command, root: &Path) {
    let inherited = env::vars_os().map(|(name, _)| name);
    let explicit = command.get_envs().map(|(name, _)| name.to_os_string());
    let names: Vec<_> = inherited
        .chain(explicit)
        .filter(|name| name.to_str().is_some_and(|name| name.starts_with("GIT_")))
        .collect();
    for name in names {
        command.env_remove(name);
    }
    command.env("GIT_CEILING_DIRECTORIES", root);
}

/// Create the fresh Cargo home used by one reproducible artifact build.
///
/// Cargo reads configuration, credentials, registry sources, and Git checkouts
/// from `$CARGO_HOME`. Reusing either a developer's Cargo home or an earlier
/// build's ignored cache would therefore let untracked state change release
/// bytes. Create this invocation's directory atomically beneath real (not
/// symlinked) worktree directories and remove only that directory on drop.
pub(crate) fn artifact_cargo_home(root: &Path) -> TaskResult<FreshArtifactDirectory> {
    fresh_artifact_directory(root, "artifact-cargo-homes", "artifact Cargo home")
}

/// Create the fresh workspace used by one JavaScript/Wasm package build.
///
/// The workspace becomes the build process's handle-anchored current
/// directory. Its Cargo homes and freshly built `wasm-bindgen-cli` are separate
/// relative children, so startup does not resolve the allocation's original
/// worktree pathname again.
pub(crate) fn artifact_wasm_workspace(root: &Path) -> TaskResult<FreshArtifactDirectory> {
    fresh_artifact_directory(root, "artifact-wasm-workspaces", "artifact Wasm workspace")
}

#[cfg(unix)]
fn fresh_artifact_directory(
    root: &Path,
    leaf: &str,
    label: &str,
) -> TaskResult<FreshArtifactDirectory> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize repository: {error}"))?;
    let parent = root.join("target").join("xtask").join(leaf);

    let parent_handle = open_artifact_parent(&root, leaf, label)?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
        .as_nanos();
    for attempt in 0..100_u32 {
        let entry_name = OsString::from(format!("build-{}-{nonce}-{attempt}", std::process::id()));

        let created =
            mkdirat(&parent_handle, &entry_name, Mode::RWXU).map_err(std::io::Error::from);

        match created {
            Ok(()) => {
                let directory_flags =
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let directory_handle =
                    openat(&parent_handle, &entry_name, directory_flags, Mode::empty()).map_err(
                        |error| {
                            format!(
                                "cannot open fresh {label} {}: {error}",
                                parent.join(&entry_name).display()
                            )
                        },
                    )?;
                return Ok(FreshArtifactDirectory {
                    path: parent.join(&entry_name),
                    parent_handle,
                    directory_handle,
                    entry_name,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot create fresh {label} {}: {error}",
                    parent.join(entry_name).display()
                ));
            }
        }
    }
    Err(format!(
        "cannot allocate a unique {label} after 100 attempts"
    ))
}

#[cfg(not(unix))]
fn fresh_artifact_directory(
    _root: &Path,
    _leaf: &str,
    label: &str,
) -> TaskResult<FreshArtifactDirectory> {
    Err(format!(
        "secure {label} isolation requires Unix directory-handle semantics"
    ))
}

#[cfg(unix)]
fn open_artifact_parent(root: &Path, leaf: &str, label: &str) -> TaskResult<OwnedFd> {
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = open(root, directory_flags, Mode::empty()).map_err(|error| {
        format!(
            "cannot open repository directory {}: {error}",
            root.display()
        )
    })?;
    reject_legacy_root_cargo_configuration(&parent, root, label)?;
    reject_external_cargo_configuration(&parent, root, label)?;
    let mut rendered = root.to_path_buf();

    for component in ["target", "xtask", leaf] {
        rendered.push(component);
        match mkdirat(&parent, component, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(format!(
                    "cannot create {label} directory {}: {error}",
                    rendered.display()
                ));
            }
        }
        parent = openat(&parent, component, directory_flags, Mode::empty()).map_err(|error| {
            format!(
                "{label} path component must not be a symlink and must be a directory: {}: {error}",
                rendered.display()
            )
        })?;
        // Cargo discovers configuration from its working directory upward,
        // independently of CARGO_HOME and --manifest-path. The tracked root
        // config is allowed, but no ignored ancestor introduced by this
        // workspace may contribute persistent configuration to a fresh build.
        match statat(&parent, ".cargo", AtFlags::SYMLINK_NOFOLLOW) {
            Err(rustix::io::Errno::NOENT) => {}
            Ok(_) => {
                return Err(format!(
                    "{label} ancestor must not contain ignored Cargo configuration: {}",
                    rendered.join(".cargo").display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect {label} ancestor configuration {}: {error}",
                    rendered.join(".cargo").display()
                ));
            }
        }
    }
    Ok(parent)
}

/// Cargo gives the legacy filename precedence over the tracked config.toml.
/// Git excludes can hide an untracked legacy file from clean-tree checks.
#[cfg(unix)]
fn reject_legacy_root_cargo_configuration(
    root_handle: &OwnedFd,
    root: &Path,
    label: &str,
) -> TaskResult {
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let cargo = match openat(root_handle, ".cargo", directory_flags, Mode::empty()) {
        Ok(cargo) => cargo,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot open repository Cargo directory {}: {error}",
                root.join(".cargo").display()
            ));
        }
    };
    let configuration = root.join(".cargo/config");
    match statat(&cargo, "config", AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Ok(_) => Err(format!(
            "{label} must not inherit legacy repository Cargo configuration: {}",
            configuration.display()
        )),
        Err(error) => Err(format!(
            "cannot inspect legacy repository Cargo configuration {}: {error}",
            configuration.display()
        )),
    }
}

/// Largest Cargo configuration file compared byte-for-byte against the
/// repository's tracked configuration. The tracked file is a few dozen bytes.
#[cfg(unix)]
const DUPLICATE_CARGO_CONFIGURATION_LIMIT: u64 = 64 * 1024;

/// Read a regular Cargo configuration file relative to an open `.cargo`
/// directory without following a final symlink.
///
/// Returns `Ok(None)` when the entry is absent, is not a regular file
/// (symlink, directory, FIFO, device, ...), is replaced between inspection
/// and open, or exceeds [`DUPLICATE_CARGO_CONFIGURATION_LIMIT`]. Callers treat
/// `None` as "not exempt", so every such case keeps failing closed.
#[cfg(unix)]
fn read_regular_cargo_configuration(
    cargo: &OwnedFd,
    name: &str,
    rendered: &Path,
) -> TaskResult<Option<Vec<u8>>> {
    use std::io::Read;

    let inspected = match statat(cargo, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot inspect Cargo configuration {}: {error}",
                rendered.display()
            ));
        }
    };
    if FileType::from_raw_mode(inspected.st_mode) != FileType::RegularFile {
        return Ok(None);
    }
    // NONBLOCK keeps a FIFO swapped in after statat from stalling the open;
    // the fstat identity check below then rejects it.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let handle = match openat(cargo, name, flags, Mode::empty()) {
        Ok(handle) => handle,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot open Cargo configuration {}: {error}",
                rendered.display()
            ));
        }
    };
    let opened = fstat(&handle).map_err(|error| {
        format!(
            "cannot inspect Cargo configuration {}: {error}",
            rendered.display()
        )
    })?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
        || opened.st_dev != inspected.st_dev
        || opened.st_ino != inspected.st_ino
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    fs::File::from(handle)
        .take(DUPLICATE_CARGO_CONFIGURATION_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "cannot read Cargo configuration {}: {error}",
                rendered.display()
            )
        })?;
    if bytes.len() as u64 > DUPLICATE_CARGO_CONFIGURATION_LIMIT {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Return the repository's own `.cargo/config.toml` bytes when an ancestor
/// copy of exactly those bytes cannot change a build by being loaded twice.
///
/// A Git worktree nested inside another checkout of this repository (for
/// example one in a subdirectory of it) sees the outer checkout's tracked
/// `.cargo/config.toml` as an ancestor configuration. Cargo already loads the
/// root copy, so a byte-identical ancestor adds no new keys. It is not
/// automatically a no-op, though: Cargo keeps the deeper scalar value but
/// concatenates arrays across files, so a duplicated `rustflags` array (or an
/// array-form alias) would apply twice. The exemption is therefore limited to
/// a root configuration that parses as TOML containing only an `[alias]` table
/// whose values are all strings, where a duplicate merge is exactly the same
/// value. Anything else returns `None`, and ancestors are rejected as before.
#[cfg(unix)]
fn duplicable_root_cargo_configuration(
    root_handle: &OwnedFd,
    root: &Path,
) -> TaskResult<Option<Vec<u8>>> {
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let cargo = match openat(root_handle, ".cargo", directory_flags, Mode::empty()) {
        Ok(cargo) => cargo,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "cannot open repository Cargo directory {}: {error}",
                root.join(".cargo").display()
            ));
        }
    };
    let Some(bytes) =
        read_regular_cargo_configuration(&cargo, "config.toml", &root.join(".cargo/config.toml"))?
    else {
        return Ok(None);
    };
    Ok(is_alias_only_cargo_configuration(&bytes).then_some(bytes))
}

/// True when `bytes` is UTF-8 TOML whose only top-level key is an `alias`
/// table of string values. Loading such a file twice merges to the same
/// aliases, because Cargo resolves duplicate string values to one value.
#[cfg(unix)]
fn is_alias_only_cargo_configuration(bytes: &[u8]) -> bool {
    let Ok(source) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Ok(parsed) = source.parse::<toml::Table>() else {
        return false;
    };
    if parsed.len() != 1 {
        return false;
    }
    parsed
        .get("alias")
        .and_then(toml::Value::as_table)
        .is_some_and(|aliases| aliases.values().all(toml::Value::is_str))
}

/// Cargo also searches above the tracked repository root. A normal Cargo
/// installation directory is harmless there, but either configuration filename
/// (including a symlink) can override the isolated artifact build.
///
/// The one exception is an ancestor `config.toml` that is a regular file with
/// exactly the bytes of the repository's own alias-only `config.toml` (see
/// [`duplicable_root_cargo_configuration`]); it arises when a worktree is
/// nested inside another checkout and cannot alter the build.
#[cfg(unix)]
fn reject_external_cargo_configuration(
    root_handle: &OwnedFd,
    root: &Path,
    label: &str,
) -> TaskResult {
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let duplicable = duplicable_root_cargo_configuration(root_handle, root)?;
    let mut current = root_handle
        .try_clone()
        .map_err(|error| format!("cannot retain repository directory handle: {error}"))?;
    let mut rendered = root.to_path_buf();
    loop {
        let ancestor = openat(&current, "..", directory_flags, Mode::empty())
            .map_err(|error| format!("cannot open repository ancestor: {error}"))?;
        let current_stat = fstat(&current)
            .map_err(|error| format!("cannot inspect repository ancestor: {error}"))?;
        let ancestor_stat = fstat(&ancestor)
            .map_err(|error| format!("cannot inspect repository ancestor: {error}"))?;
        if current_stat.st_dev == ancestor_stat.st_dev
            && current_stat.st_ino == ancestor_stat.st_ino
        {
            return Ok(());
        }
        rendered.pop();
        let cargo_path = rendered.join(".cargo");
        match statat(&ancestor, ".cargo", AtFlags::SYMLINK_NOFOLLOW) {
            Err(rustix::io::Errno::NOENT) => {}
            Ok(metadata) if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory => {
                let cargo = openat(&ancestor, ".cargo", directory_flags, Mode::empty()).map_err(
                    |error| {
                        format!(
                            "cannot open external Cargo directory {}: {error}",
                            cargo_path.display()
                        )
                    },
                )?;
                for name in ["config", "config.toml"] {
                    match statat(&cargo, name, AtFlags::SYMLINK_NOFOLLOW) {
                        Err(rustix::io::Errno::NOENT) => {}
                        // A read failure is not an exemption: it falls
                        // through to the rejection below.
                        Ok(_)
                            if name == "config.toml"
                                && duplicable.as_deref().is_some_and(|tracked| {
                                    read_regular_cargo_configuration(
                                        &cargo,
                                        name,
                                        &cargo_path.join(name),
                                    )
                                    .ok()
                                    .flatten()
                                    .as_deref()
                                        == Some(tracked)
                                }) => {}
                        Ok(_) => {
                            return Err(format!(
                                "{label} must not inherit external Cargo configuration: {}",
                                cargo_path.join(name).display()
                            ));
                        }
                        Err(error) => {
                            return Err(format!(
                                "cannot inspect external Cargo configuration {}: {error}",
                                cargo_path.join(name).display()
                            ));
                        }
                    }
                }
            }
            Ok(_) => {
                return Err(format!(
                    "external Cargo directory must be a real directory, not a symlink or file: {}",
                    cargo_path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect external Cargo directory {}: {error}",
                    cargo_path.display()
                ));
            }
        }
        current = ancestor;
    }
}

#[derive(Debug)]
pub(crate) struct FreshArtifactDirectory {
    path: PathBuf,
    #[cfg(unix)]
    parent_handle: OwnedFd,
    #[cfg(unix)]
    directory_handle: OwnedFd,
    #[cfg(unix)]
    entry_name: OsString,
}

impl FreshArtifactDirectory {
    /// Return the worktree pathname for diagnostics and path remapping only.
    ///
    /// A child process must use [`Self::run`] and relative paths so it cannot
    /// be redirected by replacing this mutable namespace pathname.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Create an empty child for a tool-specific home/cache and return the
    /// relative path that is safe to use from the anchored build directory.
    #[cfg(unix)]
    pub(crate) fn create_subdirectory(&self, name: &str) -> TaskResult<PathBuf> {
        let path = Path::new(name);
        if path.components().count() != 1
            || !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(format!(
                "artifact workspace child must be one normal path component: {name}"
            ));
        }
        mkdirat(&self.directory_handle, name, Mode::RWXU)
            .map_err(|error| format!("cannot create artifact workspace child {name}: {error}"))?;
        Ok(PathBuf::from(name))
    }

    #[cfg(not(unix))]
    pub(crate) fn create_subdirectory(&self, _name: &str) -> TaskResult<PathBuf> {
        Err("secure artifact workspace isolation is unavailable on this platform".into())
    }

    /// Execute a command through the xtask's safe directory-handle wrapper.
    ///
    /// The open directory is transferred as the wrapper's standard input. The
    /// wrapper changes directory through that handle, replaces standard input
    /// with `/dev/null` for the actual build tool, and never resolves the
    /// original worktree pathname. Relative home/cache paths are resolved from
    /// that directory even if its visible pathname was replaced before startup.
    #[cfg(unix)]
    pub(crate) fn run(&self, command: &mut Command) -> TaskResult {
        let directory = self.directory_handle.try_clone().map_err(|error| {
            format!("cannot duplicate artifact workspace directory handle: {error}")
        })?;
        let executable = env::current_exe()
            .map_err(|error| format!("cannot locate xtask executable: {error}"))?;
        let mut wrapper = Command::new(executable);
        wrapper
            .args(["run-in-artifact-directory", "--"])
            .arg(command.get_program())
            .args(command.get_args())
            .stdin(Stdio::from(directory));
        for (name, value) in command.get_envs() {
            if let Some(value) = value {
                wrapper.env(name, value);
            } else {
                wrapper.env_remove(name);
            }
        }
        run(&mut wrapper)
    }

    #[cfg(not(unix))]
    pub(crate) fn run(&self, _command: &mut Command) -> TaskResult {
        Err("secure artifact workspace isolation is unavailable on this platform".into())
    }
}

/// Enter the directory supplied as standard input and run one artifact tool.
///
/// This is an internal subprocess boundary used by [`FreshArtifactDirectory`].
/// It avoids `pre_exec` (and therefore unsafe code) while retaining the same
/// descriptor-anchored current directory on every supported Unix platform.
#[cfg(unix)]
pub(crate) fn run_in_artifact_directory(args: &[String]) -> TaskResult {
    if args.len() < 2 || args[0] != "--" {
        return Err("internal artifact runner requires `-- PROGRAM [ARGS...]`".into());
    }
    rustix::process::fchdir(std::io::stdin())
        .map_err(|error| format!("cannot enter handle-anchored artifact directory: {error}"))?;
    let mut command = Command::new(&args[1]);
    command.args(&args[2..]).stdin(Stdio::null());
    run(&mut command)
}

#[cfg(not(unix))]
pub(crate) fn run_in_artifact_directory(_args: &[String]) -> TaskResult {
    Err("secure artifact workspace isolation is unavailable on this platform".into())
}

impl Drop for FreshArtifactDirectory {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(entry_name) = CString::new(self.entry_name.as_bytes()) {
            let opened = fstat(&self.directory_handle);
            let visible = statat(&self.parent_handle, &entry_name, AtFlags::SYMLINK_NOFOLLOW);
            match (opened, visible) {
                (Ok(opened), Ok(visible))
                    if opened.st_dev == visible.st_dev && opened.st_ino == visible.st_ino =>
                {
                    // Traverse only through the retained handle. If the
                    // namespace entry is swapped after the identity check,
                    // unlinkat may fail (or remove an empty replacement), but
                    // cleanup can never recurse into the replacement tree.
                    let _ = remove_directory_contents_at(self.directory_handle.as_fd());
                    let _ = unlinkat(&self.parent_handle, &entry_name, AtFlags::REMOVEDIR);
                }
                (_, Ok(visible))
                    if FileType::from_raw_mode(visible.st_mode) == FileType::Symlink =>
                {
                    // A replacement symlink occupies only the private,
                    // unpredictable build entry. Unlink it without following
                    // its external target.
                    let _ = unlinkat(&self.parent_handle, &entry_name, AtFlags::empty());
                }
                _ => {
                    // A different real object now owns the visible name.
                    // Leaving both objects behind is safer than deleting data
                    // that this process did not allocate.
                }
            }
        }

        // Artifact allocation fails closed on platforms without the required
        // directory-handle semantics, so there is no portable cleanup branch.
    }
}

/// Remove the contents reached through an already-open directory handle.
#[cfg(unix)]
fn remove_directory_contents_at(directory: BorrowedFd<'_>) -> rustix::io::Result<()> {
    let mut entries = Dir::read_from(directory)?;
    for entry in &mut entries {
        let entry = entry?;
        let child = entry.file_name();
        if child.to_bytes() == b"." || child.to_bytes() == b".." {
            continue;
        }
        remove_entry_at(directory, child)?;
    }
    Ok(())
}

/// Recursively remove an entry without resolving any mutable path above the
/// directory handle retained at allocation time.
#[cfg(unix)]
fn remove_entry_at(parent: BorrowedFd<'_>, name: &CStr) -> rustix::io::Result<()> {
    let metadata = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error),
    };

    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return unlinkat(parent, name, AtFlags::empty());
    }

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let directory = openat(parent, name, flags, Mode::empty())?;
    remove_directory_contents_at(directory.as_fd())?;
    unlinkat(parent, name, AtFlags::REMOVEDIR)
}

pub(crate) fn is_cargo_build_override(name: &str) -> bool {
    name == "CARGO_INCREMENTAL"
        || name.starts_with("CARGO_BUILD_")
        || name.starts_with("CARGO_PROFILE_")
        || name.starts_with("CARGO_TARGET_")
}

pub(crate) fn resolve_pinned_toolchain(
    root: &Path,
    extra_paths: &[&Path],
) -> TaskResult<PinnedToolchain> {
    let channel = pinned_toolchain(root)?;
    let cargo = pinned_tool(&channel, "cargo")?;
    let path = pinned_path(&cargo, extra_paths, env::var_os("PATH"))?;
    Ok(PinnedToolchain {
        channel,
        cargo,
        path,
    })
}

pub(crate) fn pinned_cargo(root: &Path) -> TaskResult<Command> {
    let toolchain = resolve_pinned_toolchain(root, &[])?;
    Ok(pinned_cargo_command(root, &toolchain))
}

/// Reject Cargo inputs that can preserve a crates.io package identity while
/// substituting different source bytes.
///
/// Cargo metadata deliberately reports the logical source being replaced, not
/// a content-authenticated origin for the files Cargo actually loaded. A Rust
/// API analysis step may therefore use registry identity to recognize
/// macro-generated items only when the invoking builder has no source
/// replacement. Keep this guard intentionally narrower than a Cargo
/// configuration reimplementation:
/// ordinary aliases, network settings, and the crates.io transport protocol
/// remain valid, while dependency/source overrides, includes, injected source
/// environment, and index overrides fail closed.
pub(crate) fn reject_untrusted_cargo_sources(root: &Path) -> TaskResult {
    reject_untrusted_cargo_source_environment()?;
    let cargo_home = effective_cargo_home(root)?;
    reject_untrusted_cargo_configuration_files(
        root,
        &cargo_home,
        CargoConfigurationPolicy::SourceIdentity,
    )
}

/// Apply the Cargo source/configuration preflight to an explicit command
/// working directory and Cargo home.
///
/// The Rust API documentation check runs Cargo from the filesystem root with a
/// fresh home, so repository and ancestor configuration are outside Cargo's
/// discovery walk. Keeping this lower-level entry point explicit lets the
/// caller check the root and home Cargo will actually inspect instead of
/// accidentally re-checking only the invoking developer's configuration. It
/// also rejects build, target, profile, host, and environment controls at
/// those discovered locations so configuration cannot restore a compiler or
/// lint override removed from the child process environment.
pub(crate) fn reject_untrusted_cargo_sources_at(
    cargo_working_directory: &Path,
    cargo_home: &Path,
) -> TaskResult {
    reject_untrusted_cargo_source_environment()?;
    reject_untrusted_cargo_configuration_files(
        cargo_working_directory,
        cargo_home,
        CargoConfigurationPolicy::IsolatedBuild,
    )
}

fn reject_untrusted_cargo_source_environment() -> TaskResult {
    for (name, _) in env::vars_os() {
        if is_untrusted_cargo_source_environment(&name) {
            return Err(format!(
                "Cargo source-identity check cannot inherit Cargo source override environment variable `{}`",
                name.to_string_lossy()
            ));
        }
    }
    Ok(())
}

fn effective_cargo_home(root: &Path) -> TaskResult<PathBuf> {
    let configured = env::var_os("CARGO_HOME");
    #[cfg(windows)]
    let user_home = env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let user_home = env::var_os("HOME");
    resolve_cargo_home(root, configured.as_deref(), user_home.as_deref())
}

fn resolve_cargo_home(
    root: &Path,
    configured: Option<&OsStr>,
    user_home: Option<&OsStr>,
) -> TaskResult<PathBuf> {
    if let Some(configured) = configured {
        if configured.is_empty() {
            return Err("Cargo source-identity check cannot use an empty CARGO_HOME".into());
        }
        let configured = PathBuf::from(configured);
        return Ok(if configured.is_absolute() {
            configured
        } else {
            root.join(configured)
        });
    }
    let user_home = user_home
        .filter(|home| !home.is_empty())
        .ok_or_else(|| "cannot resolve the user home used by Cargo".to_string())?;
    Ok(PathBuf::from(user_home).join(".cargo"))
}

fn is_untrusted_cargo_source_environment(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let name = name.to_ascii_uppercase();
    name.starts_with("CARGO_SOURCE_")
        || name.starts_with("CARGO_PATCH_")
        || name.starts_with("CARGO_REPLACE_")
        || matches!(
            name.as_str(),
            "CARGO_PATHS" | "CARGO_REGISTRIES_CRATES_IO_INDEX" | "CARGO_REGISTRY_INDEX"
        )
}

fn is_untrusted_cargo_source_injection(name: &str) -> bool {
    is_untrusted_cargo_source_environment(OsStr::new(name))
        || matches!(
            name.to_ascii_uppercase().as_str(),
            "CARGO_HOME" | "HOME" | "USERPROFILE"
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CargoConfigurationPolicy {
    SourceIdentity,
    IsolatedBuild,
}

fn reject_untrusted_cargo_configuration_files(
    root: &Path,
    cargo_home: &Path,
    policy: CargoConfigurationPolicy,
) -> TaskResult {
    let mut configurations = BTreeSet::new();
    for directory in root.ancestors() {
        let cargo = directory.join(".cargo");
        configurations.insert(cargo.join("config"));
        configurations.insert(cargo.join("config.toml"));
    }
    configurations.insert(cargo_home.join("config"));
    configurations.insert(cargo_home.join("config.toml"));

    for configuration in configurations {
        let metadata = match fs::symlink_metadata(&configuration) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect Cargo configuration {}: {error}",
                    configuration.display()
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "Cargo source-identity check requires a regular Cargo configuration file: {}",
                configuration.display()
            ));
        }
        let source = fs::read_to_string(&configuration).map_err(|error| {
            format!(
                "cannot read Cargo configuration {}: {error}",
                configuration.display()
            )
        })?;
        let parsed = source.parse::<toml::Table>().map_err(|error| {
            format!(
                "cannot parse Cargo configuration {}: {error}",
                configuration.display()
            )
        })?;
        if parsed.contains_key("include") {
            return Err(format!(
                "Cargo source-identity check does not accept included Cargo configuration: {}",
                configuration.display()
            ));
        }
        if ["source", "paths", "patch", "replace"]
            .iter()
            .any(|key| parsed.contains_key(*key))
        {
            return Err(format!(
                "Cargo source-identity check does not accept Cargo dependency source override configuration: {}",
                configuration.display()
            ));
        }
        if policy == CargoConfigurationPolicy::IsolatedBuild
            && ["build", "target", "profile", "host"]
                .iter()
                .any(|key| parsed.contains_key(*key))
        {
            return Err(format!(
                "Cargo source-identity check does not accept Cargo build/compiler override configuration: {}",
                configuration.display()
            ));
        }
        if parsed
            .get("env")
            .and_then(toml::Value::as_table)
            .is_some_and(|variables| {
                variables
                    .keys()
                    .any(|name| is_untrusted_cargo_source_injection(name))
            })
        {
            return Err(format!(
                "Cargo source-identity check does not accept Cargo configuration that injects source override environment variables: {}",
                configuration.display()
            ));
        }
        if policy == CargoConfigurationPolicy::IsolatedBuild && parsed.contains_key("env") {
            return Err(format!(
                "Cargo source-identity check does not accept Cargo environment injection at its build configuration root: {}",
                configuration.display()
            ));
        }
        if parsed
            .get("registries")
            .and_then(toml::Value::as_table)
            .and_then(|registries| registries.get("crates-io"))
            .and_then(toml::Value::as_table)
            .is_some_and(|crates_io| crates_io.contains_key("index"))
        {
            return Err(format!(
                "Cargo source-identity check does not accept a crates.io index override: {}",
                configuration.display()
            ));
        }
    }
    Ok(())
}

/// Build the same hermetic Cargo command for an explicit fixture toolchain.
///
/// Compiler-level boundary tests run once on the repository pin and once on
/// the declared MSRV. Reusing the production command constructor keeps those
/// child Cargo invocations from accidentally finding a different `rustc` on
/// the ambient `PATH`.
#[cfg(test)]
pub(crate) fn pinned_fixture_cargo(root: &Path, channel: &str) -> TaskResult<Command> {
    let cargo = pinned_tool(channel, "cargo")?;
    let toolchain = PinnedToolchain {
        channel: channel.to_string(),
        path: pinned_path(&cargo, &[], env::var_os("PATH"))?,
        cargo,
    };
    Ok(pinned_cargo_command(root, &toolchain))
}

pub(crate) fn pinned_tool(channel: &str, tool: &str) -> TaskResult<PathBuf> {
    let result = output(Command::new("rustup").args(["which", "--toolchain", channel, tool]))?;
    let rendered = String::from_utf8(result.stdout)
        .map_err(|error| format!("pinned {tool} path is not UTF-8: {error}"))?;
    let path = PathBuf::from(rendered.trim());
    if !path.is_file() {
        return Err(format!(
            "rustup returned an invalid {tool} path for {channel}: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn pinned_cargo_command(root: &Path, toolchain: &PinnedToolchain) -> Command {
    let mut command = Command::new(&toolchain.cargo);
    command
        .current_dir(root)
        .env("PATH", &toolchain.path)
        .env("RUSTUP_TOOLCHAIN", &toolchain.channel)
        .env("CARGO", &toolchain.cargo);
    remove_rust_tool_overrides(&mut command);
    command
}

pub(crate) fn remove_rust_tool_overrides(command: &mut Command) {
    for variable in RUST_TOOL_OVERRIDES {
        command.env_remove(variable);
    }
}

fn pinned_path(
    cargo: &Path,
    extra_paths: &[&Path],
    inherited: Option<OsString>,
) -> TaskResult<OsString> {
    let toolchain_bin = cargo
        .parent()
        .ok_or_else(|| format!("pinned Cargo path has no parent: {}", cargo.display()))?;
    let mut entries = vec![toolchain_bin.to_path_buf()];
    entries.extend(extra_paths.iter().map(|path| (*path).to_path_buf()));
    if let Some(inherited) = inherited {
        entries.extend(env::split_paths(&inherited));
    }
    env::join_paths(entries)
        .map_err(|error| format!("cannot construct pinned toolchain PATH: {error}"))
}

pub(crate) fn temp_dir(label: &str) -> TaskResult<TempDir> {
    temp_dir_in(&env::temp_dir(), label)
}

/// Create a unique temporary directory under a caller-selected parent.
///
/// This is useful when a test must execute a fixture: some CI temporary
/// directories are mounted `noexec`, while the Cargo target directory is
/// already required to host executable test binaries.
pub(crate) fn temp_dir_in(parent: &Path, label: &str) -> TaskResult<TempDir> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
        .as_nanos();
    create_temp_dir(
        parent,
        &format!("pinewasm-{label}-{}-{nonce}", std::process::id()),
    )
}

fn create_temp_dir(parent: &Path, stem: &str) -> TaskResult<TempDir> {
    // The clock may return the same nonce to concurrent test threads. Claim
    // ownership atomically: create_dir_all would accept an existing fixture,
    // letting either owner overwrite its files or remove it during Drop.
    for attempt in 0..100_u32 {
        let directory = parent.join(format!("{stem}-{attempt}"));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(TempDir(directory)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create {}: {error}", directory.display())),
        }
    }
    Err(format!(
        "cannot claim a fresh temporary directory for {stem}"
    ))
}

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn sha256_file(file: &Path) -> TaskResult<String> {
    let bytes =
        fs::read(file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    Ok(sha256(&bytes))
}

pub(crate) fn copy_file(source: &Path, destination: &Path) -> TaskResult {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "cannot copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

pub(crate) fn command_exists(program: impl AsRef<OsStr>) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub(crate) fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn colliding_temp_names_keep_independent_ownership() {
        let root = temp_dir("temp-name-collision").unwrap();
        let first = create_temp_dir(root.path(), "same-clock-tick").unwrap();
        fs::write(first.path().join("owned"), "first").unwrap();
        let second = create_temp_dir(root.path(), "same-clock-tick").unwrap();
        fs::write(second.path().join("owned"), "second").unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(
            fs::read_to_string(first.path().join("owned")).unwrap(),
            "first"
        );
        drop(first);
        assert_eq!(
            fs::read_to_string(second.path().join("owned")).unwrap(),
            "second"
        );
    }

    #[test]
    fn exhausted_temp_names_leave_existing_paths_untouched() {
        let root = temp_dir("temp-name-exhaustion").unwrap();
        for attempt in 0..100 {
            fs::write(root.path().join(format!("occupied-{attempt}")), "owned").unwrap();
        }
        assert!(create_temp_dir(root.path(), "occupied").is_err());
        for attempt in 0..100 {
            assert_eq!(
                fs::read_to_string(root.path().join(format!("occupied-{attempt}"))).unwrap(),
                "owned"
            );
        }
    }

    #[test]
    fn dependency_build_cannot_discover_enclosing_git_identity() {
        let root = temp_dir("dependency-git-isolation").unwrap();
        output(Command::new("git").args(["init", "-q"]).arg(root.path())).unwrap();
        let dependency = root.path().join("target/cargo-home/registry/example");
        fs::create_dir_all(&dependency).unwrap();
        assert!(Command::new("git")
            .current_dir(&dependency)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap()
            .status
            .success());
        let mut isolated = Command::new("git");
        isolated
            .current_dir(&dependency)
            .args(["rev-parse", "--show-toplevel"])
            .env("GIT_DIR", root.path().join(".git"))
            .env("GIT_WORK_TREE", root.path())
            .env("GIT_CEILING_DIRECTORIES", "/");
        isolate_dependency_git(&mut isolated, root.path());
        assert!(!isolated.output().unwrap().status.success());
        // The repository itself remains usable; isolation is command-scoped.
        assert!(Command::new("git")
            .current_dir(root.path())
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap()
            .status
            .success());
    }

    #[test]
    fn pinned_cargo_command_defeats_a_hostile_inherited_path() {
        let root = Path::new("repository");
        let toolchain_bin = PathBuf::from("pinned-toolchain-bin");
        let helper_bin = PathBuf::from("pinned-helper-bin");
        let hostile_bin = PathBuf::from("hostile-system-bin");
        let inherited = env::join_paths([&hostile_bin]).expect("portable test PATH");
        let cargo = toolchain_bin.join("cargo");
        let path = pinned_path(&cargo, &[&helper_bin], Some(inherited)).expect("pinned PATH");
        let toolchain = PinnedToolchain {
            channel: "test-toolchain".into(),
            cargo: cargo.clone(),
            path,
        };

        let command = pinned_cargo_command(root, &toolchain);
        assert_eq!(command.get_program(), cargo.as_os_str());
        assert_eq!(command.get_current_dir(), Some(root));

        let variables = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            variables.get(OsStr::new("RUSTUP_TOOLCHAIN")),
            Some(&Some(OsString::from("test-toolchain")))
        );
        assert_eq!(
            variables.get(OsStr::new("CARGO")),
            Some(&Some(cargo.into_os_string()))
        );
        for variable in RUST_TOOL_OVERRIDES {
            assert_eq!(
                variables.get(OsStr::new(variable)),
                Some(&None),
                "{variable} must not override the pinned toolchain"
            );
        }
        let path = variables
            .get(OsStr::new("PATH"))
            .and_then(Option::as_ref)
            .expect("PATH override");
        assert_eq!(
            env::split_paths(path).collect::<Vec<_>>(),
            vec![toolchain_bin, helper_bin, hostile_bin]
        );
    }

    #[test]
    fn reproducible_builds_identify_inherited_cargo_overrides() {
        for name in [
            "CARGO_INCREMENTAL",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_PROFILE_RELEASE_OPT_LEVEL",
            "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS",
            "CARGO_TARGET_DIR",
        ] {
            assert!(is_cargo_build_override(name), "{name}");
        }
        for name in ["CARGO", "CARGO_HOME", "CARGO_PKG_NAME", "RUSTUP_HOME"] {
            assert!(!is_cargo_build_override(name), "{name}");
        }
    }

    #[test]
    fn cargo_source_identity_allows_benign_configuration() {
        let outer = temp_dir("cargo-source-benign-config").unwrap();
        let root = outer.path().join("nested/repository");
        let cargo_home = outer.path().join("cargo-home");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::create_dir_all(outer.path().join(".cargo")).unwrap();
        fs::create_dir_all(&cargo_home).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[alias]\nxtask = 'run --locked -p pinewasm_xtask --'\n",
        )
        .unwrap();
        fs::write(
            outer.path().join(".cargo/config.toml"),
            "[net]\ngit-fetch-with-cli = true\n",
        )
        .unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            "[registries.crates-io]\nprotocol = 'sparse'\n[env]\nORDINARY = 'value'\n",
        )
        .unwrap();

        reject_untrusted_cargo_configuration_files(
            &root,
            &cargo_home,
            CargoConfigurationPolicy::SourceIdentity,
        )
        .expect("benign Cargo settings must remain usable");
    }

    #[test]
    fn cargo_source_identity_rejects_every_configuration_layer() {
        let cases = [
            (
                "root",
                "config.toml",
                "[\"source\".\"crates-io\"]\nreplace-with = 'vendor'\n",
                "dependency source override",
            ),
            (
                "parent",
                "config",
                "paths = ['vendor/serde']\n",
                "dependency source override",
            ),
            (
                "home",
                "config.toml",
                "[patch.crates-io]\nserde = { path = 'vendor/serde' }\n",
                "dependency source override",
            ),
            (
                "root",
                "config.toml",
                "[replace]\n\"serde:1.0.0\" = { path = 'vendor/serde' }\n",
                "dependency source override",
            ),
            (
                "parent",
                "config.toml",
                "include = ['unlisted.toml']\n",
                "included Cargo configuration",
            ),
            (
                "home",
                "config.toml",
                "[registries.crates-io]\nindex = 'https://example.invalid/index'\n",
                "crates.io index override",
            ),
            (
                "root",
                "config.toml",
                "[env]\nCARGO_REGISTRIES_CRATES_IO_INDEX = { value = 'https://example.invalid/index', force = true }\n",
                "injects source override environment",
            ),
            (
                "parent",
                "config.toml",
                "[env]\nCARGO_HOME = { value = 'unlisted-home', force = true, relative = true }\n",
                "injects source override environment",
            ),
        ];

        for (location, filename, contents, expected) in cases {
            let outer = temp_dir("cargo-source-hostile-config").unwrap();
            let root = outer.path().join("nested/repository");
            let cargo_home = outer.path().join("cargo-home");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&cargo_home).unwrap();
            let directory = match location {
                "root" => root.join(".cargo"),
                "parent" => outer.path().join(".cargo"),
                "home" => cargo_home.clone(),
                _ => unreachable!("fixture locations are exhaustive"),
            };
            fs::create_dir_all(&directory).unwrap();
            let configuration = directory.join(filename);
            fs::write(&configuration, contents).unwrap();

            let error = reject_untrusted_cargo_configuration_files(
                &root,
                &cargo_home,
                CargoConfigurationPolicy::SourceIdentity,
            )
            .expect_err("source-identity override must fail closed");
            assert!(error.contains(expected), "{error}");
            assert!(error.contains(configuration.to_str().unwrap()), "{error}");
        }
    }

    #[test]
    fn isolated_build_rejects_discovered_compiler_overrides() {
        let cases = [
            ("[build]\nrustc-wrapper = '/hostile/wrapper'\n", "build/compiler"),
            (
                "[target.x86_64-unknown-linux-gnu]\nrustflags = ['--cap-lints', 'allow']\n",
                "build/compiler",
            ),
            (
                "[profile.dev]\ndebug-assertions = false\n",
                "build/compiler",
            ),
            (
                "[host]\nrustflags = ['--cap-lints', 'allow']\n",
                "build/compiler",
            ),
            (
                "[env]\nCARGO_ENCODED_RUSTDOCFLAGS = { value = '--cap-lints allow', force = true }\n",
                "environment injection",
            ),
        ];

        for (contents, expected) in cases {
            let fixture = temp_dir("cargo-identity-build-config").unwrap();
            let root = fixture.path().join("build-root");
            let cargo_home = fixture.path().join("cargo-home");
            fs::create_dir_all(root.join(".cargo")).unwrap();
            fs::create_dir_all(&cargo_home).unwrap();
            // Join component by component so the expected spelling matches
            // the scanner's native-separator rendering on every platform.
            let configuration = root.join(".cargo").join("config.toml");
            fs::write(&configuration, contents).unwrap();

            let error = reject_untrusted_cargo_configuration_files(
                &root,
                &cargo_home,
                CargoConfigurationPolicy::IsolatedBuild,
            )
            .expect_err("discovered build override must fail closed");
            assert!(error.contains(expected), "{error}");
            assert!(error.contains(configuration.to_str().unwrap()), "{error}");
        }
    }

    #[test]
    fn cargo_source_identity_identifies_environment_overrides() {
        for name in [
            "CARGO_SOURCE_CRATES_IO_REPLACE_WITH",
            "CARGO_SOURCE_VENDOR_DIRECTORY",
            "CARGO_PATHS",
            "CARGO_PATCH_CRATES_IO_SERDE_PATH",
            "CARGO_REPLACE_SERDE_PATH",
            "CARGO_REGISTRIES_CRATES_IO_INDEX",
            "CARGO_REGISTRY_INDEX",
            "cargo_source_crates_io_replace_with",
        ] {
            assert!(is_untrusted_cargo_source_environment(OsStr::new(name)));
        }
        for name in [
            "CARGO_HOME",
            "CARGO_REGISTRIES_CRATES_IO_PROTOCOL",
            "CARGO_REGISTRY_TOKEN",
        ] {
            assert!(!is_untrusted_cargo_source_environment(OsStr::new(name)));
        }
        for name in ["CARGO_HOME", "HOME", "USERPROFILE", "cargo_home"] {
            assert!(is_untrusted_cargo_source_injection(name));
        }

        let root = temp_dir("cargo-home-resolution").unwrap();
        assert_eq!(
            resolve_cargo_home(root.path(), Some(OsStr::new("relative-home")), None).unwrap(),
            root.path().join("relative-home")
        );
        let absolute = root.path().join("absolute-home");
        assert_eq!(
            resolve_cargo_home(root.path(), Some(absolute.as_os_str()), None).unwrap(),
            absolute
        );
        let user = root.path().join("user");
        assert_eq!(
            resolve_cargo_home(root.path(), None, Some(user.as_os_str())).unwrap(),
            user.join(".cargo")
        );
        assert!(resolve_cargo_home(root.path(), Some(OsStr::new("")), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cargo_source_identity_rejects_symlinked_configuration() {
        use std::os::unix::fs::symlink;

        let outer = temp_dir("cargo-source-symlink-config").unwrap();
        let root = outer.path().join("repository");
        let cargo_home = outer.path().join("cargo-home");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::create_dir_all(&cargo_home).unwrap();
        let target = outer.path().join("target.toml");
        fs::write(&target, "[net]\noffline = true\n").unwrap();
        let configuration = root.join(".cargo/config.toml");
        symlink(&target, &configuration).unwrap();

        let error = reject_untrusted_cargo_configuration_files(
            &root,
            &cargo_home,
            CargoConfigurationPolicy::SourceIdentity,
        )
        .expect_err("symlinked configuration must fail closed");
        assert!(error.contains("regular Cargo configuration"), "{error}");
        assert!(error.contains(configuration.to_str().unwrap()), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn artifact_cargo_home_is_fresh_and_does_not_reuse_ignored_sources() {
        let root = temp_dir("artifact-cargo-home-test").unwrap();
        let stale = root.path().join("target/xtask/cargo-home");
        let poisoned = stale.join("registry/src/example-1.0.0/src/lib.rs");
        fs::create_dir_all(poisoned.parent().unwrap()).unwrap();
        fs::write(&poisoned, "compile_error!(\"poisoned cache\");\n").unwrap();

        let first = artifact_cargo_home(root.path()).unwrap();
        let first_path = first.path().to_path_buf();
        assert_ne!(first.path(), stale);
        assert!(!first.path().join("registry").exists());
        let first_poison = first.path().join("registry/src/example-1.0.0/src/lib.rs");
        fs::create_dir_all(first_poison.parent().unwrap()).unwrap();
        fs::write(&first_poison, "compile_error!(\"first build poison\");\n").unwrap();
        assert_eq!(
            fs::read_to_string(&poisoned).unwrap(),
            "compile_error!(\"poisoned cache\");\n"
        );
        drop(first);
        assert!(!first_path.exists());

        let second = artifact_cargo_home(root.path()).unwrap();
        assert_ne!(second.path(), first_path);
        assert!(!second.path().join("registry").exists());
    }

    #[cfg(unix)]
    #[test]
    fn wasm_bindgen_helper_is_fresh_and_does_not_reuse_mutable_tools() {
        let root = temp_dir("wasm-bindgen-helper-test").unwrap();
        let stale = root.path().join("target/xtask/wasm-bindgen-cli");
        let helper = Path::new("bin/wasm-bindgen");
        let stale_helper = stale.join(helper);
        fs::create_dir_all(stale_helper.parent().unwrap()).unwrap();
        fs::write(&stale_helper, "hostile helper\n").unwrap();

        let first = artifact_wasm_workspace(root.path()).unwrap();
        let first_cache = first.create_subdirectory("wasm-bindgen-cli").unwrap();
        let first_path = first.path().to_path_buf();
        assert_ne!(first.path(), stale);
        assert!(!first.path().join(&first_cache).join(helper).exists());
        let first_helper = first.path().join(&first_cache).join(helper);
        fs::create_dir_all(first_helper.parent().unwrap()).unwrap();
        fs::write(&first_helper, "first build poison\n").unwrap();
        assert_eq!(
            fs::read_to_string(&stale_helper).unwrap(),
            "hostile helper\n"
        );
        drop(first);
        assert!(!first_path.exists());

        let second = artifact_wasm_workspace(root.path()).unwrap();
        let second_cache = second.create_subdirectory("wasm-bindgen-cli").unwrap();
        assert_ne!(second.path(), first_path);
        assert!(!second.path().join(second_cache).join(helper).exists());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_symlinked_components_without_deleting_external_files() {
        use std::os::unix::fs::symlink;

        for (leaf, allocate) in [
            (
                "artifact-cargo-homes",
                artifact_cargo_home as fn(&Path) -> TaskResult<FreshArtifactDirectory>,
            ),
            ("artifact-wasm-workspaces", artifact_wasm_workspace),
        ] {
            for real_components in [0, 1, 2] {
                let root = temp_dir("artifact-directory-symlink-root").unwrap();
                let external = temp_dir("artifact-directory-symlink-external").unwrap();
                let sentinel = external.path().join("credentials.toml");
                fs::write(&sentinel, "do not delete\n").unwrap();

                let components = ["target", "xtask", leaf];
                let mut path = root.path().to_path_buf();
                for component in &components[..real_components] {
                    path.push(component);
                    fs::create_dir(&path).unwrap();
                }
                path.push(components[real_components]);
                symlink(external.path(), &path).unwrap();

                let error = allocate(root.path()).unwrap_err();
                assert!(error.contains("must not be a symlink"), "{error}");
                assert_eq!(fs::read_to_string(&sentinel).unwrap(), "do not delete\n");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_all_ignored_cargo_configuration_ancestors() {
        use std::os::unix::fs::symlink;

        for (leaf, allocate) in [
            (
                "artifact-cargo-homes",
                artifact_cargo_home as fn(&Path) -> TaskResult<FreshArtifactDirectory>,
            ),
            ("artifact-wasm-workspaces", artifact_wasm_workspace),
        ] {
            for depth in 1..=3 {
                for kind in ["directory", "file", "dangling-symlink"] {
                    let root = temp_dir("artifact-ancestor-config-root").unwrap();
                    let mut ancestor = root.path().to_path_buf();
                    for component in &["target", "xtask", leaf][..depth] {
                        ancestor.push(component);
                    }
                    fs::create_dir_all(&ancestor).unwrap();
                    let config = ancestor.join(".cargo");
                    match kind {
                        "directory" => {
                            fs::create_dir(&config).unwrap();
                            fs::write(config.join("config.toml"), "[env]\nSTALE = 'yes'\n")
                                .unwrap();
                        }
                        "file" => fs::write(&config, "do not delete\n").unwrap(),
                        "dangling-symlink" => symlink("missing-external-config", &config).unwrap(),
                        _ => unreachable!(),
                    }

                    let error = allocate(root.path()).unwrap_err();
                    assert!(error.contains("ignored Cargo configuration"), "{error}");
                    assert!(fs::symlink_metadata(&config).is_ok());
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_allow_config_free_external_cargo_and_tracked_root_config() {
        let outer = temp_dir("artifact-config-free-ancestor").unwrap();
        fs::create_dir_all(outer.path().join(".cargo/bin")).unwrap();
        fs::create_dir_all(outer.path().join(".cargo/registry")).unwrap();
        let credentials = outer.path().join(".cargo/credentials.toml");
        fs::write(&credentials, "must not be read or modified\n").unwrap();
        let root = outer.path().join("nested/repository");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[alias]\ntracked = 'check'\n",
        )
        .unwrap();

        let cargo = artifact_cargo_home(&root).unwrap();
        let wasm = artifact_wasm_workspace(&root).unwrap();
        assert!(cargo.path().is_dir());
        assert!(wasm.path().is_dir());
        assert_eq!(
            fs::read_to_string(credentials).unwrap(),
            "must not be read or modified\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_legacy_root_cargo_configuration() {
        use std::os::unix::fs::symlink;

        for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
            for kind in ["file", "directory", "dangling-symlink"] {
                let root = temp_dir("artifact-root-legacy-config").unwrap();
                fs::create_dir(root.path().join(".cargo")).unwrap();
                let tracked = root.path().join(".cargo/config.toml");
                fs::write(&tracked, "[alias]\ntracked = 'check'\n").unwrap();
                let legacy = root.path().join(".cargo/config");
                match kind {
                    "file" => fs::write(&legacy, "[profile.release]\nopt-level = 0\n").unwrap(),
                    "directory" => fs::create_dir(&legacy).unwrap(),
                    "dangling-symlink" => symlink("missing-legacy-config", &legacy).unwrap(),
                    _ => unreachable!(),
                }
                let error = allocate(root.path()).unwrap_err();
                assert!(
                    error.contains("legacy repository Cargo configuration"),
                    "{error}"
                );
                assert!(fs::symlink_metadata(&legacy).is_ok());
                assert_eq!(
                    fs::read_to_string(tracked).unwrap(),
                    "[alias]\ntracked = 'check'\n"
                );
                assert!(!root.path().join("target").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_untrusted_external_cargo_entries() {
        use std::os::unix::fs::symlink;

        for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
            for kind in ["cargo-symlink", "cargo-file", "config-symlink"] {
                let outer = temp_dir("artifact-external-cargo-entry").unwrap();
                let root = outer.path().join("nested/repository");
                fs::create_dir_all(&root).unwrap();
                let cargo = outer.path().join(".cargo");
                let entry = match kind {
                    "cargo-symlink" => {
                        symlink("missing-external-cargo", &cargo).unwrap();
                        cargo
                    }
                    "cargo-file" => {
                        fs::write(&cargo, "must not be deleted\n").unwrap();
                        cargo
                    }
                    "config-symlink" => {
                        fs::create_dir(&cargo).unwrap();
                        let config = cargo.join("config.toml");
                        symlink("missing-external-config", &config).unwrap();
                        config
                    }
                    _ => unreachable!(),
                };
                let error = allocate(&root).unwrap_err();
                assert!(error.contains("external Cargo"), "{error}");
                assert!(fs::symlink_metadata(entry).is_ok());
                assert!(!root.join("target").exists());
            }
        }
    }

    /// Lay out `outer/.cargo/<outer_name>` above a nested checkout whose own
    /// `.cargo/config.toml` holds `root_config`, mirroring a Git worktree
    /// created inside another checkout.
    #[cfg(unix)]
    fn nested_checkout(root_config: &str) -> (TempDir, PathBuf) {
        let outer = temp_dir("artifact-nested-worktree").unwrap();
        fs::create_dir_all(outer.path().join(".cargo")).unwrap();
        let root = outer.path().join("worktrees/nested");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(root.join(".cargo/config.toml"), root_config).unwrap();
        (outer, root)
    }

    const TRACKED_ALIAS_CONFIG: &str = "[alias]\nxtask = \"run --locked -p pinewasm_xtask --\"\n";

    #[cfg(unix)]
    #[test]
    fn artifact_directories_allow_identical_alias_only_ancestor_config() {
        for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
            let (outer, root) = nested_checkout(TRACKED_ALIAS_CONFIG);
            let ancestor = outer.path().join(".cargo/config.toml");
            fs::write(&ancestor, TRACKED_ALIAS_CONFIG).unwrap();

            let artifact = allocate(&root).unwrap();
            assert!(artifact.path().is_dir());
            assert_eq!(fs::read_to_string(ancestor).unwrap(), TRACKED_ALIAS_CONFIG);
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_non_duplicate_ancestor_config() {
        use std::os::unix::fs::symlink;

        for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
            for kind in [
                "different-bytes",
                "trailing-newline",
                "symlink-to-identical",
                "legacy-name-identical",
                "fifo",
                "directory",
            ] {
                let (outer, root) = nested_checkout(TRACKED_ALIAS_CONFIG);
                let cargo = outer.path().join(".cargo");
                let entry = match kind {
                    "different-bytes" => {
                        let entry = cargo.join("config.toml");
                        fs::write(&entry, "[alias]\nxtask = \"run -p other --\"\n").unwrap();
                        entry
                    }
                    "trailing-newline" => {
                        let entry = cargo.join("config.toml");
                        fs::write(&entry, format!("{TRACKED_ALIAS_CONFIG}\n")).unwrap();
                        entry
                    }
                    "symlink-to-identical" => {
                        let entry = cargo.join("config.toml");
                        symlink(root.join(".cargo/config.toml"), &entry).unwrap();
                        entry
                    }
                    "legacy-name-identical" => {
                        let entry = cargo.join("config");
                        fs::write(&entry, TRACKED_ALIAS_CONFIG).unwrap();
                        entry
                    }
                    "fifo" => {
                        let entry = cargo.join("config.toml");
                        let status = Command::new("mkfifo").arg(&entry).status().unwrap();
                        assert!(status.success());
                        entry
                    }
                    "directory" => {
                        let entry = cargo.join("config.toml");
                        fs::create_dir(&entry).unwrap();
                        entry
                    }
                    _ => unreachable!(),
                };

                let error = allocate(&root).unwrap_err();
                assert!(
                    error.contains("must not inherit external Cargo configuration"),
                    "{kind}: {error}"
                );
                assert!(fs::symlink_metadata(entry).is_ok());
                assert!(!root.join("target").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_identical_ancestor_config_that_is_not_alias_only() {
        for root_config in [
            // Cargo concatenates arrays across files, so a duplicate applies twice.
            "[build]\nrustflags = [\"-Cdebuginfo=0\"]\n",
            "[alias]\nxtask = [\"run\", \"--locked\", \"-p\", \"pinewasm_xtask\", \"--\"]\n",
            "[alias]\nxtask = \"run\"\n[env]\nSTALE = \"yes\"\n",
            "include = \"extra.toml\"\n[alias]\nxtask = \"run\"\n",
            "not valid toml = = =\n",
        ] {
            for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
                let (outer, root) = nested_checkout(root_config);
                let ancestor = outer.path().join(".cargo/config.toml");
                fs::write(&ancestor, root_config).unwrap();

                let error = allocate(&root).unwrap_err();
                assert!(
                    error.contains("must not inherit external Cargo configuration"),
                    "{root_config}: {error}"
                );
                assert!(!root.join("target").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_reject_identical_ancestor_when_root_config_is_a_symlink() {
        use std::os::unix::fs::symlink;

        for allocate in [artifact_cargo_home, artifact_wasm_workspace] {
            let (outer, root) = nested_checkout(TRACKED_ALIAS_CONFIG);
            let ancestor = outer.path().join(".cargo/config.toml");
            fs::write(&ancestor, TRACKED_ALIAS_CONFIG).unwrap();
            let root_config = root.join(".cargo/config.toml");
            fs::remove_file(&root_config).unwrap();
            symlink(&ancestor, &root_config).unwrap();

            let error = allocate(&root).unwrap_err();
            assert!(
                error.contains("must not inherit external Cargo configuration"),
                "{error}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn alias_only_cargo_configuration_matches_the_tracked_root_config() {
        assert!(is_alias_only_cargo_configuration(
            TRACKED_ALIAS_CONFIG.as_bytes()
        ));
        assert!(is_alias_only_cargo_configuration(include_bytes!(
            "../../../.cargo/config.toml"
        )));
        assert!(!is_alias_only_cargo_configuration(b""));
        assert!(!is_alias_only_cargo_configuration(b"\xff[alias]\n"));
        assert!(!is_alias_only_cargo_configuration(b"alias = \"x\"\n"));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_stays_anchored_when_the_namespace_parent_is_replaced() {
        use std::os::unix::fs::symlink;

        for (leaf, allocate) in [
            (
                "artifact-cargo-homes",
                artifact_cargo_home as fn(&Path) -> TaskResult<FreshArtifactDirectory>,
            ),
            ("artifact-wasm-workspaces", artifact_wasm_workspace),
        ] {
            let root = temp_dir("artifact-directory-parent-swap-root").unwrap();
            let external = temp_dir("artifact-directory-parent-swap-external").unwrap();
            let artifact = allocate(root.path()).unwrap();
            let entry_name = artifact.path().file_name().unwrap().to_owned();
            let nested_external = external.path().join("nested-sentinel.txt");
            fs::write(&nested_external, "do not delete\n").unwrap();
            symlink(external.path(), artifact.path().join("nested-link")).unwrap();
            fs::write(artifact.path().join("ordinary-file"), "delete me\n").unwrap();

            let namespace = root.path().join("target/xtask").join(leaf);
            let detached_namespace = root
                .path()
                .join("target/xtask")
                .join(format!("{leaf}-detached"));
            fs::rename(&namespace, &detached_namespace).unwrap();
            symlink(external.path(), &namespace).unwrap();

            let decoy = external.path().join(&entry_name);
            fs::create_dir(&decoy).unwrap();
            let decoy_sentinel = decoy.join("external-sentinel.txt");
            fs::write(&decoy_sentinel, "do not delete\n").unwrap();

            drop(artifact);

            assert_eq!(
                fs::read_to_string(&decoy_sentinel).unwrap(),
                "do not delete\n"
            );
            assert_eq!(
                fs::read_to_string(&nested_external).unwrap(),
                "do not delete\n"
            );
            assert!(!detached_namespace.join(entry_name).exists());

            // Do not leave a symlink inside the disposable test root for its
            // portable best-effort cleanup to encounter.
            fs::remove_file(namespace).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_unlinks_a_replacement_final_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("artifact-directory-entry-swap-root").unwrap();
        let external = temp_dir("artifact-directory-entry-swap-external").unwrap();
        let sentinel = external.path().join("external-sentinel.txt");
        fs::write(&sentinel, "do not delete\n").unwrap();

        let artifact = artifact_cargo_home(root.path()).unwrap();
        let original_path = artifact.path().to_path_buf();
        let moved_path = original_path.with_extension("moved-aside");
        fs::rename(&original_path, &moved_path).unwrap();
        symlink(external.path(), &original_path).unwrap();

        drop(artifact);

        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "do not delete\n");
        assert!(!original_path.exists());
        assert!(moved_path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_leaves_a_replacement_final_directory_untouched() {
        let root = temp_dir("artifact-directory-entry-replacement-root").unwrap();
        let artifact = artifact_cargo_home(root.path()).unwrap();
        let original_path = artifact.path().to_path_buf();
        fs::write(original_path.join("owned-file"), "original\n").unwrap();

        let moved_path = original_path.with_extension("moved-aside");
        fs::rename(&original_path, &moved_path).unwrap();
        fs::create_dir(&original_path).unwrap();
        let sentinel = original_path.join("replacement-sentinel.txt");
        fs::write(&sentinel, "do not delete\n").unwrap();

        drop(artifact);

        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "do not delete\n");
        assert_eq!(
            fs::read_to_string(moved_path.join("owned-file")).unwrap(),
            "original\n"
        );
    }
}
