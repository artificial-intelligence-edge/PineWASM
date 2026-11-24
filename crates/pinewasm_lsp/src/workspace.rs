//! Workspace boundaries and local Pine library discovery.

use crate::ToolResult;
use pinewasm_compiler::{
    MAX_LIBRARY_COUNT, MAX_LIBRARY_PATH_LEN, MAX_LIBRARY_SOURCE_BYTES, MAX_SOURCE_LEN,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_DISCOVERED_FILES: usize = 10_000;
const MAX_SCAN_DEPTH: usize = 12;
/// Hard ceiling on directories visited in a single discovery walk. Any real
/// Pine project sits far below this; the cap only exists so that pointing the
/// workspace at a huge tree (a home directory, `/`) can never walk forever.
const MAX_SCAN_DIRS: usize = 50_000;
/// Soft wall-clock budget for a discovery walk. Normal projects finish in a
/// few milliseconds; this only trips on a pathologically large root, keeping
/// startup snappy instead of blocking the language-server startup behind the scan.
const SCAN_TIME_BUDGET: Duration = Duration::from_millis(750);
/// Directory names never descended into: build artifacts, dependency caches,
/// and the large, often permission-protected macOS home subtrees. Discovering
/// Pine sources never needs to enter any of these, and skipping them keeps a
/// home-directory workspace both fast and clear of `Operation not permitted`.
const SKIPPED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "Library",
    "Applications",
    "Music",
    "Movies",
    "Pictures",
];

#[derive(Debug, Clone)]
pub struct LibraryFile {
    pub import_path: String,
    pub file: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    libraries: Vec<LibraryFile>,
    pine_files: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct WorkspaceConfig {
    libraries: BTreeMap<String, String>,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> ToolResult<Self> {
        let root = root.into();
        let root = if root.exists() {
            root.canonicalize()?
        } else {
            return Err(format!("workspace does not exist: {}", root.display()).into());
        };
        let mut workspace = Self {
            root,
            libraries: Vec::new(),
            pine_files: Vec::new(),
        };
        workspace.refresh()?;
        Ok(workspace)
    }

    pub fn current_dir() -> ToolResult<Self> {
        Self::new(std::env::current_dir()?)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn pine_files(&self) -> &[PathBuf] {
        &self.pine_files
    }

    /// Rebuild the on-disk library catalog after watched-file changes.
    ///
    /// Discovery is deliberately best effort: an unreadable directory, a broken
    /// symlink, a library file that fails to read, or a malformed config is
    /// surfaced on stderr and skipped, never propagated. The catalog is a
    /// convenience layer over whatever Pine happens to sit under the root, so a
    /// hostile or oversized root must degrade to a partial catalog rather than
    /// fail construction — a hard error here would take the whole language-server startup
    /// (or LSP session) down with it.
    pub fn refresh(&mut self) -> ToolResult<()> {
        let mut files = Vec::new();
        scan_files(
            &self.root,
            &self.root,
            0,
            &mut files,
            &mut ScanBudget::new(),
        );
        files.sort();
        files.dedup();
        self.pine_files = files.clone();

        let mut catalog = BTreeMap::<String, LibraryFile>::new();
        let mut source_bytes = 0usize;

        // Explicit mappings take priority, including when the compiler's table
        // limit leaves room for only part of a large discovery catalog.
        let config_path = self.root.join(".pinewasm-lsp.json");
        if config_path.is_file() {
            match self.config_libraries(&config_path) {
                Ok(entries) => {
                    for (import_path, library) in entries {
                        insert_library(&mut catalog, &mut source_bytes, import_path, library);
                    }
                }
                Err(error) => {
                    eprintln!("pinewasm: ignoring {}: {error}", config_path.display());
                }
            }
        }

        for file in files {
            let Some(import_path) = infer_import_path(&self.root, &file) else {
                continue;
            };
            if catalog.contains_key(&import_path) {
                continue;
            }
            if catalog.len() >= MAX_LIBRARY_COUNT {
                break;
            }
            if let Err(error) = validate_import_path(&import_path) {
                eprintln!("pinewasm: skipping library {}: {error}", file.display());
                continue;
            }
            let source = match read_bounded_text(&file, MAX_SOURCE_LEN) {
                Ok(source) => source,
                Err(error) => {
                    eprintln!(
                        "pinewasm: skipping unreadable library {}: {error}",
                        file.display()
                    );
                    continue;
                }
            };
            insert_library(
                &mut catalog,
                &mut source_bytes,
                import_path.clone(),
                LibraryFile {
                    import_path,
                    file,
                    source,
                },
            );
        }

        self.libraries = catalog.into_values().collect();
        Ok(())
    }

    /// Parse the explicit `.pinewasm-lsp.json` library mappings. Only an
    /// unreadable, oversized, or unparseable file rejects the whole config (and
    /// is caught and reported by [`refresh`](Self::refresh)); an individual mapping that
    /// fails to resolve is reported and skipped so the remaining valid mappings
    /// still stand.
    fn config_libraries(&self, config_path: &Path) -> ToolResult<Vec<(String, LibraryFile)>> {
        let config_path = self.resolve_file(config_path)?;
        let config: WorkspaceConfig =
            serde_json::from_str(&read_bounded_text(&config_path, MAX_SOURCE_LEN)?)?;
        let mut entries = Vec::new();
        let mut source_bytes = 0usize;
        for (import_path, configured) in config.libraries {
            if entries.len() >= MAX_LIBRARY_COUNT {
                eprintln!(
                    "pinewasm: configured library catalog exceeds {MAX_LIBRARY_COUNT} entries"
                );
                break;
            }
            match self.resolve_config_library(&import_path, &configured) {
                Ok(library) => {
                    if source_bytes.saturating_add(library.source.len()) > MAX_LIBRARY_SOURCE_BYTES
                    {
                        eprintln!("pinewasm: skipping library `{import_path}`: library catalog limit reached");
                        continue;
                    }
                    source_bytes += library.source.len();
                    entries.push((import_path, library));
                }
                Err(error) => eprintln!(
                    "pinewasm: skipping library `{import_path}` in {}: {error}",
                    config_path.display()
                ),
            }
        }
        Ok(entries)
    }

    /// Resolve one explicit mapping to its on-disk source, confined to the
    /// workspace root.
    fn resolve_config_library(
        &self,
        import_path: &str,
        configured: &str,
    ) -> ToolResult<LibraryFile> {
        validate_import_path(import_path)?;
        let file = self.resolve_file(Path::new(configured))?;
        let source = read_bounded_text(&file, MAX_SOURCE_LEN)?;
        Ok(LibraryFile {
            import_path: import_path.to_string(),
            file,
            source,
        })
    }

    /// Compiler-ready `(import path, source)` pairs, with unsaved LSP buffers
    /// replacing their on-disk library revisions.
    pub fn libraries(&self, open_buffers: &HashMap<PathBuf, String>) -> Vec<(String, String)> {
        self.libraries
            .iter()
            .map(|library| {
                let source = open_buffers
                    .get(&library.file)
                    .cloned()
                    .unwrap_or_else(|| library.source.clone());
                (library.import_path.clone(), source)
            })
            .collect()
    }

    pub fn library_file(&self, source_id: u32) -> Option<&LibraryFile> {
        source_id
            .checked_sub(1)
            .and_then(|index| self.libraries.get(index as usize))
    }

    pub fn library_by_import_path(&self, import_path: &str) -> Option<&LibraryFile> {
        self.libraries
            .iter()
            .find(|library| library.import_path == import_path)
    }

    /// Resolve and authorize a user-supplied file. Language-server file tools are confined
    /// to the configured root; callers can always pass source text explicitly.
    pub fn resolve_file(&self, path: &Path) -> ToolResult<PathBuf> {
        let resolved = self.resolve_path(path)?;
        if !resolved.is_file() {
            return Err(format!("not a file: {}", resolved.display()).into());
        }
        Ok(resolved)
    }

    fn resolve_path(&self, path: &Path) -> ToolResult<PathBuf> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canonical = candidate.canonicalize()?;
        if !canonical.starts_with(&self.root) {
            return Err(format!(
                "path is outside workspace {}: {}",
                self.root.display(),
                canonical.display()
            )
            .into());
        }
        Ok(canonical)
    }
}

/// Read at most the admitted bytes plus one sentinel byte. Metadata is only
/// an early rejection: the bounded reader also protects against a growing file.
pub(crate) fn read_bounded_text(path: &Path, limit: usize) -> ToolResult<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(format!("not a regular file: {}", path.display()).into());
    }
    if metadata.len() > limit as u64 {
        return Err(format!("file exceeds {limit} bytes: {}", path.display()).into());
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(format!("file exceeds {limit} bytes: {}", path.display()).into());
    }
    Ok(String::from_utf8(bytes)?)
}

fn insert_library(
    catalog: &mut BTreeMap<String, LibraryFile>,
    source_bytes: &mut usize,
    import_path: String,
    library: LibraryFile,
) {
    if catalog.len() >= MAX_LIBRARY_COUNT
        || source_bytes.saturating_add(library.source.len()) > MAX_LIBRARY_SOURCE_BYTES
    {
        eprintln!("pinewasm: skipping library `{import_path}`: library catalog limit reached");
        return;
    }
    *source_bytes += library.source.len();
    catalog.insert(import_path, library);
}

/// Bounds a single discovery walk so a huge or hostile root can neither run
/// forever nor block startup. Both limits are generous relative to any real
/// Pine project, so a normal workspace scans to completion untouched.
struct ScanBudget {
    deadline: Instant,
    dirs_remaining: usize,
}

impl ScanBudget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + SCAN_TIME_BUDGET,
            dirs_remaining: MAX_SCAN_DIRS,
        }
    }

    /// Whether any limit has been reached. Checked once per directory entry so
    /// a single enormous directory can't outrun the wall-clock budget mid-scan.
    fn is_exhausted(&self) -> bool {
        self.dirs_remaining == 0 || Instant::now() >= self.deadline
    }

    /// Charge one directory read against the budget. Returns `false` once the
    /// directory cap or the wall-clock deadline is reached, ending the walk.
    fn take_dir(&mut self) -> bool {
        if self.is_exhausted() {
            return false;
        }
        self.dirs_remaining -= 1;
        true
    }
}

/// Collect `.pine` / `.pine.local` files under `directory`, best effort.
///
/// Unreadable directories (permission denied, mid-scan races), entries whose
/// type can't be determined, and files that fail to canonicalize (broken
/// symlinks) are skipped rather than aborting the walk. The walk also stops
/// once any of the depth, file-count, directory-count, or time limits is hit.
fn scan_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
    budget: &mut ScanBudget,
) {
    if depth > MAX_SCAN_DEPTH || output.len() >= MAX_DISCOVERED_FILES || !budget.take_dir() {
        return;
    }
    let Ok(read_dir) = fs::read_dir(directory) else {
        return;
    };
    // Stream entries rather than materializing and sorting the whole directory
    // first: a single pathologically large flat directory must not force a big
    // allocation or O(n log n) sort before the budget is consulted again. The
    // final `pine_files` list is sorted by `refresh` and the catalog is
    // import-path ordered, so traversal order here is not observable.
    for entry in read_dir {
        if output.len() >= MAX_DISCOVERED_FILES || budget.is_exhausted() {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            scan_files(root, &path, depth + 1, output, budget);
        } else if name.ends_with(".pine") || name.ends_with(".pine.local") {
            if let Ok(canonical) = path.canonicalize() {
                // Discovery is subject to the same workspace boundary as
                // explicitly configured files. A valid symlink must not turn
                // an in-workspace directory entry into an out-of-workspace
                // source read.
                if canonical.starts_with(root) && canonical.is_file() {
                    output.push(canonical);
                }
            }
        }
    }
}

// Not `Path::extension`: the two are not equivalent here. A file named exactly
// `.pine` has no extension by Rust's rules (the leading dot makes it the stem),
// so the rewrite would silently stop resolving it, and it would also start
// matching `.PINE`. Changing either behavior is a decision, not a lint fix.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn infer_import_path(root: &Path, file: &Path) -> Option<String> {
    // Keep the inference helper safe when called independently of discovery.
    // In particular, the flat `.pine.local` convention does not otherwise
    // inspect any parent components.
    file.strip_prefix(root).ok()?;
    let name = file.file_name()?.to_str()?;
    if let Some(stem) = name.strip_suffix(".pine.local") {
        let mut parts: Vec<&str> = stem.split('_').collect();
        if parts.len() >= 3 {
            let publisher = parts.remove(0);
            let version = parts.pop()?;
            return Some(format!("{publisher}/{}/{version}", parts.join("_")));
        }
    }

    if name.ends_with(".pine") {
        let relative = file.strip_prefix(root).ok()?;
        let components: Vec<_> = relative.components().collect();
        // Both `libraries/` and `library/<publisher>/<library>/<version>.pine`
        // project layouts are recognized.
        let under_libraries = components.iter().any(|component| {
            let name = component.as_os_str();
            name == "libraries" || name == "library"
        });
        if under_libraries && components.len() >= 4 {
            let publisher = components[components.len() - 3].as_os_str().to_str()?;
            let library = components[components.len() - 2].as_os_str().to_str()?;
            let version = Path::new(components.last()?.as_os_str())
                .file_stem()?
                .to_str()?;
            return Some(format!("{publisher}/{library}/{version}"));
        }
    }
    None
}

fn validate_import_path(path: &str) -> ToolResult<()> {
    if path.len() > MAX_LIBRARY_PATH_LEN {
        return Err(format!("library key exceeds {MAX_LIBRARY_PATH_LEN} bytes").into());
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(format!("library key must be Publisher/Library/Version, got `{path}`").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_reader_enforces_utf8_byte_limit() {
        let dir = scratch_dir("bounded-reader");
        let file = dir.join("source.pine");
        fs::write(&file, "éé").unwrap();
        assert_eq!(read_bounded_text(&file, 4).unwrap(), "éé");
        assert!(read_bounded_text(&file, 3)
            .unwrap_err()
            .to_string()
            .contains("exceeds 3 bytes"));
        fs::write(&file, [0xff]).unwrap();
        assert!(read_bounded_text(&file, 4).is_err());
        assert!(read_bounded_text(&dir, 4).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_library_source_and_config_are_skipped() {
        let dir = scratch_dir("oversized-library");
        fs::write(dir.join("p_L_1.pine.local"), "x".repeat(MAX_SOURCE_LEN + 1)).unwrap();
        fs::write(
            dir.join(".pinewasm-lsp.json"),
            " ".repeat(MAX_SOURCE_LEN + 1),
        )
        .unwrap();
        let workspace = Workspace::new(&dir).unwrap();
        assert!(workspace.libraries(&HashMap::new()).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovered_catalog_obeys_compiler_count_and_explicit_priority() {
        let dir = scratch_dir("catalog-count");
        for index in 0..=MAX_LIBRARY_COUNT {
            fs::write(
                dir.join(format!("p_L{index}_1.pine.local")),
                "//@version=6\n",
            )
            .unwrap();
        }
        fs::write(
            dir.join("explicit.pine"),
            "//@version=6\nlibrary(\"Explicit\")\n",
        )
        .unwrap();
        fs::write(
            dir.join(".pinewasm-lsp.json"),
            r#"{"libraries":{"p/Explicit/1":"explicit.pine"}}"#,
        )
        .unwrap();
        let workspace = Workspace::new(&dir).unwrap();
        let libraries = workspace.libraries(&HashMap::new());
        assert_eq!(libraries.len(), MAX_LIBRARY_COUNT);
        assert!(workspace.library_by_import_path("p/Explicit/1").is_some());
        let compiled = pinewasm_compiler::compile_with_libraries(
            "//@version=6\nindicator(\"Catalog\")\nplot(close)\n",
            &libraries,
            pinewasm_compiler::CompileOptions::default(),
        );
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovered_catalog_obeys_compiler_aggregate_bytes() {
        let dir = scratch_dir("catalog-bytes");
        for index in 0..10 {
            fs::write(
                dir.join(format!("p_L{index}_1.pine.local")),
                " ".repeat(MAX_SOURCE_LEN),
            )
            .unwrap();
        }
        let workspace = Workspace::new(&dir).unwrap();
        let libraries = workspace.libraries(&HashMap::new());
        assert_eq!(libraries.len(), MAX_LIBRARY_SOURCE_BYTES / MAX_SOURCE_LEN);
        assert_eq!(
            libraries
                .iter()
                .map(|(_, source)| source.len())
                .sum::<usize>(),
            MAX_LIBRARY_SOURCE_BYTES
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_special_files_before_library_reads() {
        use std::os::unix::net::UnixListener;
        let dir = scratch_dir("special-file");
        let socket = dir.join("p_L_1.pine.local");
        let listener = UnixListener::bind(&socket).unwrap();
        let workspace = Workspace::new(&dir).unwrap();
        assert!(workspace.pine_files().is_empty());
        assert!(workspace.libraries(&HashMap::new()).is_empty());
        drop(listener);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_symlink_cannot_read_outside_workspace() {
        let dir = scratch_dir("config-boundary");
        let outside = scratch_dir("outside-config");
        fs::write(dir.join("valid.pine"), "//@version=6\n").unwrap();
        let config = outside.join("config.json");
        fs::write(&config, r#"{"libraries":{"p/Outside/1":"valid.pine"}}"#).unwrap();
        std::os::unix::fs::symlink(&config, dir.join(".pinewasm-lsp.json")).unwrap();
        let workspace = Workspace::new(&dir).unwrap();
        assert!(workspace.library_by_import_path("p/Outside/1").is_none());
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn infers_flat_library_fixture_names() {
        let root = Path::new("/repo");
        assert_eq!(
            infer_import_path(
                root,
                Path::new("/repo/tests/libraries/example_Signals_2.pine.local")
            )
            .as_deref(),
            Some("example/Signals/2")
        );
    }

    #[test]
    fn import_path_inference_rejects_files_outside_the_workspace() {
        let root = Path::new("/repo");
        assert_eq!(
            infer_import_path(root, Path::new("/elsewhere/example_Signals_2.pine.local")),
            None
        );
        assert_eq!(
            infer_import_path(
                root,
                Path::new("/elsewhere/libraries/example/Signals/2.pine")
            ),
            None
        );
    }

    #[test]
    fn infers_hierarchical_library_paths() {
        let root = Path::new("/repo");
        assert_eq!(
            infer_import_path(root, Path::new("/repo/libraries/acme/Signals/6.pine")).as_deref(),
            Some("acme/Signals/6")
        );
        assert_eq!(
            infer_import_path(root, Path::new("/repo/library/acme/Signals/6.pine")).as_deref(),
            Some("acme/Signals/6")
        );
    }

    #[test]
    fn file_resolution_cannot_escape_the_workspace() {
        let workspace = Workspace::new(env!("CARGO_MANIFEST_DIR")).unwrap();
        let outside = Path::new("../pinewasm_span/Cargo.toml");
        assert!(workspace.resolve_file(outside).is_err());
    }

    /// A distinct scratch directory per test. Tests run as threads in one
    /// process, so the caller-supplied tag (not the pid) is what keeps names
    /// from colliding; leftovers from a prior run are cleared first.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pinewasm-ws-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn discovered_names(workspace: &Workspace) -> Vec<String> {
        workspace
            .pine_files()
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_malformed_config_does_not_fail_construction() {
        let dir = scratch_dir("bad-config");
        fs::write(
            dir.join("strategy.pine"),
            "//@version=6\nindicator(\"x\")\n",
        )
        .unwrap();
        fs::write(dir.join(".pinewasm-lsp.json"), "{ not valid json ").unwrap();

        let workspace = Workspace::new(dir.clone()).expect("a broken config must not be fatal");
        assert_eq!(discovered_names(&workspace), ["strategy.pine"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_skipped_not_fatal() {
        let dir = scratch_dir("dangling-symlink");
        fs::write(dir.join("real.pine"), "//@version=6\n").unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("dangling.pine")).unwrap();

        let workspace = Workspace::new(dir.clone()).expect("a dangling .pine must not be fatal");
        assert_eq!(discovered_names(&workspace), ["real.pine"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn valid_file_symlinks_cannot_escape_automatic_discovery() {
        let dir = scratch_dir("outside-file-symlink");
        let outside = scratch_dir("outside-file-symlink-target");
        let outside_pine = outside.join("secret.pine");
        let outside_library = outside.join("example_Secret_1.pine.local");
        fs::write(&outside_pine, "//@version=6\nindicator(\"outside\")\n").unwrap();
        fs::write(
            &outside_library,
            "//@version=6\nlibrary(\"outside library\")\n",
        )
        .unwrap();
        fs::write(
            dir.join("inside.pine"),
            "//@version=6\nindicator(\"inside\")\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside_pine, dir.join("linked.pine")).unwrap();
        std::os::unix::fs::symlink(&outside_library, dir.join("example_Secret_1.pine.local"))
            .unwrap();

        let workspace = Workspace::new(dir.clone()).expect("outside symlinks must be skipped");
        assert_eq!(discovered_names(&workspace), ["inside.pine"]);
        assert!(workspace
            .library_by_import_path("example/Secret/1")
            .is_none());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("unreadable-subdir");
        fs::write(dir.join("top.pine"), "//@version=6\n").unwrap();
        let locked = dir.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("inner.pine"), "//@version=6\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        // Root ignores permission bits, so only assert the skip when the
        // directory is genuinely unreadable to this process; either way,
        // construction must succeed.
        let unreadable = fs::read_dir(&locked).is_err();
        let workspace =
            Workspace::new(dir.clone()).expect("an unreadable subdir must not be fatal");
        if unreadable {
            assert_eq!(discovered_names(&workspace), ["top.pine"]);
        }

        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_config_mapping_does_not_drop_the_valid_ones() {
        let dir = scratch_dir("partial-config");
        fs::write(dir.join("good.pine"), "//@version=6\n").unwrap();
        fs::write(
            dir.join(".pinewasm-lsp.json"),
            r#"{"libraries":{"acme/Good/6":"good.pine","acme/Missing/6":"nope.pine"}}"#,
        )
        .unwrap();

        let workspace =
            Workspace::new(dir.clone()).expect("a partially broken config must not be fatal");
        assert!(workspace.library_by_import_path("acme/Good/6").is_some());
        assert!(workspace.library_by_import_path("acme/Missing/6").is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
