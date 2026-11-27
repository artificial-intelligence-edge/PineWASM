use crate::command::{
    artifact_wasm_workspace, copy_file, isolate_dependency_git, output_allow_failure, pinned_tool,
    remove_cargo_build_overrides, remove_rust_tool_overrides, repo_root, resolve_pinned_toolchain,
};
use crate::TaskResult;
use regex::bytes::Regex;
use serde::ser::{Error as _, SerializeMap};
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GENERATED_FILES: [&str; 3] = ["pinewasm_bg.wasm", "pinewasm.js", "pinewasm.d.ts"];
pub(crate) const WASM_BINDGEN_CLI_VERSION: &str = "0.2.127";
const PACKAGE_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const ROOT_EXPORT_CONDITIONS: [&str; 3] = ["types", "import", "default"];
const PACKAGE_FILES: [(&str, &str); 4] = [
    ("README.md", "crates/pinewasm_bindings/README.md"),
    ("LICENSE", "LICENSE"),
    ("NOTICE", "NOTICE"),
    ("THIRD_PARTY_LICENSES.txt", "THIRD_PARTY_LICENSES.txt"),
];
const INIT_OUTPUT_DECLARATION: &str = "export interface InitOutput";

/// The Binaryen arguments that optimize the package's compiler module.
///
/// `-O` alone inlines every function that has a single caller, whatever its
/// size, fusing whole compiler pipelines into functions of up to about
/// 100 KiB. An engine compiles each Wasm function in full the first time it is
/// called, so a cold `compile()` or `compileStandaloneWasmProgram` compiled
/// every branch of those functions, run or not. The hottest of them then
/// became long optimizing-tier jobs that held up the host's next
/// `WebAssembly.compile`. Inlining only single-caller functions of at most 50
/// Binaryen expressions keeps the small helpers inlined and the cold paths
/// out of line. The AOT runtimes an artifact links never pass through here,
/// so emitted modules are unaffected. See
/// `docs/browser.md#cold-start-of-the-compiler-package`.
const WASM_OPT_ARGS: [&str; 2] = ["-O", "--one-caller-inline-max-function-size=50"];

/// Exact high-level signatures whose arguments or return values cross the
/// Serde/`JsValue` boundary. Keeping this list in the package finalizer makes a
/// silently reintroduced `any` or a stale custom declaration a build failure,
/// rather than something first discovered by a downstream TypeScript user.
const REQUIRED_TYPESCRIPT_SIGNATURES: &[&str] = &[
    "constructor(program: Program, market_data: MarketData, run_options?: RunOptions | null, bars?: number | null);",
    "abiMismatch(): string | null;",
    "drainAlerts(): ReadonlyArray<AlertEvent>;",
    "drainDrawings(): Drawings | null;",
    "resolveDisplayStyles(cells: BigInt64Array, visible_bars: number): NativeDisplayStyles;",
    "drainLogs(): ReadonlyArray<LogEntry>;",
    "takeError(): RuntimeError | null;",
    "advance(ticks: ReadonlyArray<Tick>): RunResult;",
    "constructor(program: Program, market_data: MarketData, run_options?: RunOptions | null);",
    "result(): RunResult;",
    "export function compile(source: string, options?: CompileOptions | null): CompileResult;",
    "export function compileStandaloneWasmProgram(program: Program, options: StandaloneWasmOptions): StandaloneWasmCompileResult;",
    "export function compileAndRun(source: string, market_data: MarketData, compile_options?: CompileOptions | null, run_options?: RunOptions | null): RunResult;",
    "export function compileAndRunWithLibraries(source: string, libraries: Libraries | null | undefined, market_data: MarketData, compile_options?: CompileOptions | null, run_options?: RunOptions | null): RunResult;",
    "export function compileProgramToWasm(program: Program): CsawCompileResult;",
    "export function compileWithLibraries(source: string, libraries: Libraries | null | undefined, options?: CompileOptions | null): CompileResult;",
    "export function compileResident(source: string, libraries: Libraries | null | undefined, options?: CompileOptions | null): ResidentCompileResult;",
    "exportProgram(): Program;",
    "prepareVm(): PreparedProgram;",
    "compileStandaloneWasm(options: StandaloneWasmOptions): StandaloneWasmCompileResult;",
    "readonly inputs: ReadonlyArray<InputDeclaration>;\n    readonly metadata: ProgramMetadata;\n    readonly plots: ReadonlyArray<CsilPlot>;",
    "export function outline(source: string): ReadonlyArray<OutlineNode>;",
    "export function languageQuery(request: LanguageQueryRequest): LanguageQueryResponse;",
    "export function languageCapabilities(): LanguageCapabilities;",
    "export function runProgram(program: Program, market_data: MarketData, run_options?: RunOptions | null): RunResult;",
    "export function searchFiles(files: ReadonlyArray<SearchFile>, query: string, options: SearchOptions): SearchResults;",
    "export function symbols(source: string): ReadonlyArray<DocumentSymbol>;",
    "export function version(): VersionInfo;",
    "export function renderingContract(): RenderingContract;",
    "constructor(program: Program);",
    "runColumns(columns: Float64Array, metadata: MarketData, options: RunOptions | null | undefined, numeric_plots: boolean): RunResult | NumericRunResult;",
];

const REQUIRED_TYPESCRIPT_CONTRACT_SNIPPETS: &[&str] = &[
    "export {};",
    "declare const pineWasmProgram: unique symbol;",
    "export type CompileOptions =",
    "readonly disableDistanceScanOptimization?: boolean;",
    "readonly distanceScanKernels: number;",
    "readonly emitIl?: boolean; readonly emitCsil?: never",
    "readonly emitIl?: never; readonly emitCsil?: boolean",
    "export interface Program {",
    "export interface ResidentCompileResult extends Omit<CompileResult, \"program\">",
    "readonly [pineWasmProgram]: never;",
    "type TaggedInputValue<Tag extends InputValueTag, Value> =",
    "readonly [Other in Exclude<InputValueTag, Tag>]?: never",
    "export type InputValue =",
    "export type DeclaredInputValue =",
    "readonly defval: DeclaredInputValue;",
    "readonly renderingContractVersion: number;",
    "readonly astDebug: string | null;",
    "readonly inputs?: Map<number, InputValue>;",
    "readonly externalSources?: Map<number, ExternalSourceLane>;",
    "export interface MarketData",
    "export interface RunOptions",
    "export interface RunResult",
    "readonly lineRefs?: readonly [number | null, number | null];",
    "readonly sourceOrder?: number;",
    "readonly merge?: readonly [bigint, bigint, bigint, bigint];",
    "readonly strategy?: StrategyOutcome;",
    "readonly signal_plot?: IntegerInput | null;",
    "readonly signal_semantics?: \"position-events\" | \"posture-events\" | null;",
    "readonly max_collection_elements?: number | null;",
    "export type CsawCompileResult = CsawModule | UnsupportedCsawModule;",
    "export type StandaloneWasmCompileResult = StandaloneWasmModule | RefusedStandaloneWasmModule;",
    "export type NumericPlotSeries =",
    "export type NumericStrategyOutcome =",
    "export type NumericRunResult =",
    "readonly plots: ReadonlyArray<NumericPlotSeries>;",
    "readonly strategy?: NumericStrategyOutcome;",
];

/// Build `target/wasm-pkg`. `migrate` builds the bindings with the `migrate`
/// feature, which adds `migrateToV6` to the JavaScript API.
pub(crate) fn build_wasm_package_with_path(path: &Path, migrate: bool) -> TaskResult {
    build_wasm_package_with_optimizer(
        &path.join(format!("wasm-opt{}", env::consts::EXE_SUFFIX)),
        migrate,
    )
}

fn build_wasm_package_with_optimizer(wasm_opt: &Path, migrate: bool) -> TaskResult {
    let root = repo_root()?;
    require_identical(
        &root.join("LICENSE"),
        &root.join("crates/pinewasm_bindings/LICENSE"),
        "the bindings crate license must match the repository license",
    )?;
    if let Ok(expected) = env::var("PINEWASM_WASM_BINDGEN_CLI_VERSION") {
        if expected != WASM_BINDGEN_CLI_VERSION {
            return Err(format!(
                "PINEWASM_WASM_BINDGEN_CLI_VERSION={expected} disagrees with the repository pin ({WASM_BINDGEN_CLI_VERSION})"
            ));
        }
    }
    if env::var_os("RUSTFLAGS").is_some() || env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some() {
        return Err("external Rust flags are not allowed for release builds".into());
    }

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME must be set".to_string())?;
    let build_workspace = artifact_wasm_workspace(&root)?;
    let cargo_home = build_workspace.create_subdirectory("cargo-home")?;
    let bindgen_cargo_home = build_workspace.create_subdirectory("bindgen-cargo-home")?;
    let bindgen_root = build_workspace.create_subdirectory("wasm-bindgen-cli")?;
    let rustup_home = canonical_env_path("RUSTUP_HOME", home.join(".rustup"), "rustup home")?;
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize repository: {error}"))?;
    // Only the Rust toolchain belongs on the compiler's PATH. Binaryen is
    // checksum-verified and invoked by absolute path below; prepending its
    // ignored directory would also trust any unrelated stale executables.
    let toolchain = resolve_pinned_toolchain(&root, &[])?;
    let rustc = pinned_tool(&toolchain.channel, "rustc")?;
    let separator = '\u{1f}';
    let encoded_flags = format!(
        "--remap-path-prefix={}=/workspace{separator}--remap-path-prefix={}=/cargo-home{separator}--remap-path-prefix={}=/rustup-home",
        root.display(),
        build_workspace.path().join(&cargo_home).display(),
        rustup_home.display()
    );

    let clean = output_allow_failure(Command::new("git").current_dir(&root).args([
        "status",
        "--porcelain=v1",
        "--untracked-files=normal",
        "--ignore-submodules",
    ]))?;
    let source_revision = if env::var_os("PINEWASM_SOURCE_REVISION").is_none()
        && clean.status.success()
        && clean.stdout.is_empty()
    {
        let revision = crate::command::git(&root, &["rev-parse", "HEAD"])?;
        Some(revision.trim().to_string())
    } else {
        None
    };

    // The package's compiler links the chart runtime of this same checkout
    // (`compileChartWasmProgram`), so it is built first.
    let chart_runtime = crate::aot::build_chart_runtime()?;

    let package_dir = root.join("target/wasm-pkg");
    if package_dir.exists() {
        fs::remove_dir_all(&package_dir)
            .map_err(|error| format!("cannot remove {}: {error}", package_dir.display()))?;
    }
    fs::create_dir(&package_dir)
        .map_err(|error| format!("cannot create {}: {error}", package_dir.display()))?;

    let mut build = Command::new(&toolchain.cargo);
    build
        .env("PATH", &toolchain.path)
        .env("RUSTUP_TOOLCHAIN", &toolchain.channel)
        .env("CARGO", &toolchain.cargo)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTC", &rustc)
        .args(["build", "--lib", "--release", "--target"])
        .arg("wasm32-unknown-unknown")
        .arg("--manifest-path")
        .arg(root.join("crates/pinewasm_bindings/Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target"))
        .arg("--locked");
    if migrate {
        build.args(["--features", "migrate"]);
    }
    remove_rust_tool_overrides(&mut build);
    remove_cargo_build_overrides(&mut build);
    isolate_dependency_git(&mut build, &root);
    // The release build intentionally sets its own reproducibility flags after
    // inherited compiler selectors and Cargo build settings have been removed.
    build
        .env("CARGO_ENCODED_RUSTFLAGS", encoded_flags)
        .env("CARGO_INCREMENTAL", "0")
        .env(crate::aot::CHART_RUNTIME_ENV, &chart_runtime);
    if let Some(revision) = source_revision {
        build.env("PINEWASM_SOURCE_REVISION", revision);
    }
    build_workspace.run(&mut build)?;

    let mut install_bindgen = Command::new(&toolchain.cargo);
    install_bindgen
        .env("PATH", &toolchain.path)
        .env("RUSTUP_TOOLCHAIN", &toolchain.channel)
        .env("CARGO", &toolchain.cargo)
        .env("CARGO_HOME", &bindgen_cargo_home)
        .env("RUSTC", &rustc)
        .args(["install", "wasm-bindgen-cli", "--version"])
        .arg(format!("={WASM_BINDGEN_CLI_VERSION}"))
        .args(["--locked", "--force", "--root"])
        .arg(&bindgen_root);
    remove_rust_tool_overrides(&mut install_bindgen);
    remove_cargo_build_overrides(&mut install_bindgen);
    isolate_dependency_git(&mut install_bindgen, &root);
    build_workspace.run(&mut install_bindgen)?;

    let wasm_input = root.join("target/wasm32-unknown-unknown/release/pinewasm_bindings.wasm");
    let bindgen = bindgen_root
        .join("bin")
        .join(format!("wasm-bindgen{}", env::consts::EXE_SUFFIX));
    let mut generate = Command::new(&bindgen);
    generate
        .arg(&wasm_input)
        .arg("--out-dir")
        .arg(&package_dir)
        .args(["--typescript", "--target", "web", "--out-name", "pinewasm"]);
    build_workspace.run(&mut generate)?;

    let generated_wasm = package_dir.join("pinewasm_bg.wasm");
    let optimized_wasm = package_dir.join("pinewasm_bg.wasm-opt.wasm");
    let mut optimize = Command::new(wasm_opt);
    optimize
        .arg(&generated_wasm)
        .arg("-o")
        .arg(&optimized_wasm)
        .args(WASM_OPT_ARGS);
    build_workspace.run(&mut optimize)?;
    fs::rename(&optimized_wasm, &generated_wasm).map_err(|error| {
        format!(
            "cannot replace {} with optimized Wasm: {error}",
            generated_wasm.display()
        )
    })?;
    write_package_manifest(&package_dir)?;
    finalize_directory(&root, &package_dir)
}

fn write_package_manifest(package_dir: &Path) -> TaskResult {
    let manifest = json!({
        "description": "WebAssembly bindings for the PineWASM compiler, runtime, language services, and AOT APIs",
        "files": GENERATED_FILES,
        "license": "AGPL-3.0-only",
        "main": "pinewasm.js",
        "name": "pinewasm",
        "sideEffects": ["./snippets/*"],
        "types": "pinewasm.d.ts",
        "version": crate::version::COMPILER_VERSION,
    });
    let mut rendered = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("cannot render npm package manifest: {error}"))?;
    rendered.push('\n');
    let path = package_dir.join("package.json");
    fs::write(&path, rendered).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

pub(crate) fn canonical_env_path(name: &str, default: PathBuf, label: &str) -> TaskResult<PathBuf> {
    let value = env::var_os(name).map_or(default, PathBuf::from);
    value
        .canonicalize()
        .map_err(|error| format!("{label} is unavailable ({}): {error}", value.display()))
}

pub(crate) fn finalize(args: &[String]) -> TaskResult {
    if args.len() > 1 {
        return Err("usage: finalize-wasm-package [PACKAGE_DIR]".into());
    }
    let root = repo_root()?;
    let package_dir = args
        .first()
        .map_or_else(|| root.join("target/wasm-pkg"), PathBuf::from);
    let package_dir = package_dir.canonicalize().map_err(|error| {
        format!(
            "cannot resolve package directory {}: {error}",
            package_dir.display()
        )
    })?;
    finalize_directory(&root, &package_dir)
}

fn finalize_directory(root: &Path, package_dir: &Path) -> TaskResult {
    let manifest_path = package_dir.join("package.json");
    if !package_dir.is_dir() || !manifest_path.is_file() {
        return Err(format!(
            "Wasm package output is missing package.json: {}",
            package_dir.display()
        ));
    }
    let manifest_source = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let mut manifest: Value = serde_json::from_str(&manifest_source)
        .map_err(|error| format!("cannot parse {}: {error}", manifest_path.display()))?;
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| "package.json must contain a JSON object".to_string())?;
    if object.get("license").and_then(Value::as_str) != Some("AGPL-3.0-only") {
        return Err("package.json must declare license AGPL-3.0-only".into());
    }
    object.insert("name".into(), json!("pinewasm"));
    object.insert("publishConfig".into(), json!({"access": "public"}));
    object.insert(
        "repository".into(),
        json!({
            "type": "git",
            "url": format!("git+{PACKAGE_REPOSITORY}.git")
        }),
    );
    // Keep the legacy `main`/`types` fields alongside explicit conditional
    // exports so Node and bundlers always have a resolvable root entry point.
    object.insert("type".into(), json!("module"));
    object.insert("main".into(), json!("pinewasm.js"));
    object.insert("types".into(), json!("pinewasm.d.ts"));
    object.insert(
        "exports".into(),
        json!({
            ".": {
                "types": "./pinewasm.d.ts",
                "import": "./pinewasm.js",
                "default": "./pinewasm.js"
            },
            "./pinewasm_bg.wasm": "./pinewasm_bg.wasm",
            "./package.json": "./package.json"
        }),
    );
    validate_root_export_conditions(object)?;

    let files = object
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "package.json files must be an array".to_string())?;
    if !files.iter().all(Value::is_string) {
        return Err("package.json files must be an array of strings".into());
    }

    for generated in GENERATED_FILES {
        let generated_path = package_dir.join(generated);
        require_nonempty(&generated_path, "required generated file")?;
        if !files.iter().any(|entry| entry.as_str() == Some(generated)) {
            return Err(format!(
                "package.json does not include generated file: {generated}"
            ));
        }
        reject_builder_paths(root, &generated_path)?;
    }
    validate_generated_api_docs(&package_dir.join("pinewasm.js"))?;
    let declarations = package_dir.join("pinewasm.d.ts");
    validate_generated_api_docs(&declarations)?;
    validate_typescript_contract(&declarations)?;
    for (destination, source) in PACKAGE_FILES {
        let source_path = root.join(source);
        require_nonempty(&source_path, "required source file")?;
        copy_file(&source_path, &package_dir.join(destination))?;
        if !files
            .iter()
            .any(|entry| entry.as_str() == Some(destination))
        {
            files.push(json!(destination));
        }
    }

    // `serde_json::Map` sorts keys unless its workspace-global
    // `preserve_order` feature is enabled. Conditional exports are the one
    // place where insertion order is semantic: Node selects the first
    // matching condition. Keep the rest of the manifest deterministic while
    // serializing the root conditions in their deliberate order.
    let rendered = serde_json::to_string_pretty(&PackageManifest(object))
        .map_err(|error| format!("cannot serialize package.json: {error}"))?;
    fs::write(&manifest_path, format!("{rendered}\n"))
        .map_err(|error| format!("cannot write {}: {error}", manifest_path.display()))?;
    for (name, _) in PACKAGE_FILES {
        require_nonempty(&package_dir.join(name), "generated package file")?;
    }
    validate_package_readme(&package_dir.join("README.md"))?;
    println!("Finalized Wasm package: {}", package_dir.display());
    Ok(())
}

fn validate_root_export_conditions(manifest: &serde_json::Map<String, Value>) -> TaskResult {
    let conditions = manifest
        .get("exports")
        .and_then(|exports| exports.get("."))
        .and_then(Value::as_object)
        .ok_or_else(|| "package.json must define an object export for `.`".to_string())?;
    let actual = conditions.keys().map(String::as_str).collect::<Vec<_>>();
    if conditions.len() != ROOT_EXPORT_CONDITIONS.len()
        || !ROOT_EXPORT_CONDITIONS
            .iter()
            .all(|condition| conditions.contains_key(*condition))
    {
        return Err(format!(
            "package.json root export conditions must be exactly {ROOT_EXPORT_CONDITIONS:?}, found {actual:?}"
        ));
    }
    Ok(())
}

struct PackageManifest<'a>(&'a serde_json::Map<String, Value>);

impl Serialize for PackageManifest<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            if key == "exports" {
                map.serialize_entry(key, &PackageExports(value))?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct PackageExports<'a>(&'a Value);

impl Serialize for PackageExports<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let exports = self
            .0
            .as_object()
            .ok_or_else(|| S::Error::custom("package.json exports must be an object"))?;
        let mut map = serializer.serialize_map(Some(exports.len()))?;
        for (key, value) in exports {
            if key == "." {
                map.serialize_entry(key, &RootExportConditions(value))?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct RootExportConditions<'a>(&'a Value);

impl Serialize for RootExportConditions<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let conditions = self
            .0
            .as_object()
            .ok_or_else(|| S::Error::custom("package.json root export must be an object"))?;
        if conditions.len() != ROOT_EXPORT_CONDITIONS.len() {
            return Err(S::Error::custom(format!(
                "package.json root export must contain exactly {} conditions",
                ROOT_EXPORT_CONDITIONS.len()
            )));
        }
        let mut map = serializer.serialize_map(Some(ROOT_EXPORT_CONDITIONS.len()))?;
        for condition in ROOT_EXPORT_CONDITIONS {
            let value = conditions.get(condition).ok_or_else(|| {
                S::Error::custom(format!("package.json root export is missing {condition}"))
            })?;
            map.serialize_entry(condition, value)?;
        }
        map.end()
    }
}

fn validate_package_readme(file: &Path) -> TaskResult {
    let source = fs::read_to_string(file).map_err(|error| {
        format!(
            "cannot read generated package README {}: {error}",
            file.display()
        )
    })?;
    for required in [
        "npm install pinewasm",
        "npm pack ./target/wasm-pkg",
        "from 'pinewasm'",
    ] {
        if !source.contains(required) {
            return Err(format!(
                "generated package README is missing required package guidance {required:?}: {}",
                file.display()
            ));
        }
    }
    for forbidden in [
        "No npm release has been published",
        "from './target/wasm-pkg",
        "from \"./target/wasm-pkg",
    ] {
        if source.contains(forbidden) {
            return Err(format!(
                "generated package README contains source-checkout-only guidance {forbidden:?}: {}",
                file.display()
            ));
        }
    }
    Ok(())
}

fn validate_generated_api_docs(file: &Path) -> TaskResult {
    let source = fs::read_to_string(file).map_err(|error| {
        format!(
            "cannot read generated API documentation {}: {error}",
            file.display()
        )
    })?;
    if source.contains("[`") {
        return Err(format!(
            "generated API documentation contains an unresolved Rustdoc link: {}",
            file.display()
        ));
    }
    Ok(())
}

fn validate_typescript_contract(file: &Path) -> TaskResult {
    let source = fs::read_to_string(file).map_err(|error| {
        format!(
            "cannot read generated TypeScript declarations {}: {error}",
            file.display()
        )
    })?;
    validate_typescript_contract_source(&source)
        .map_err(|error| format!("{error}: {}", file.display()))
}

fn validate_typescript_contract_source(source: &str) -> TaskResult {
    // Keep two position-preserving projections: declarations retain literal
    // contents so exact enum/wire spellings remain checked, while syntax also
    // blanks strings so text inside a literal cannot impersonate a declaration.
    let mut declarations = source.as_bytes().to_vec();
    let mut syntax = source.as_bytes().to_vec();
    let non_code =
        Regex::new(r#"(?s:\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|/\*.*?\*/)|(?m://[^\r\n]*)"#)
            .expect("valid TypeScript comment/string matcher");
    for matched in non_code.find_iter(source.as_bytes()) {
        blank_typescript_region(&mut syntax[matched.start()..matched.end()]);
        if source.as_bytes()[matched.start()] == b'/' {
            blank_typescript_region(&mut declarations[matched.start()..matched.end()]);
        }
    }
    if syntax.contains(&b'`') {
        return Err(
            "generated TypeScript contains unsupported template-literal syntax; update the declaration validator before accepting it".into(),
        );
    }
    let syntax_source =
        std::str::from_utf8(&syntax).expect("blanking TypeScript regions preserves valid UTF-8");
    let marker_count = syntax_source.matches(INIT_OUTPUT_DECLARATION).count();
    if marker_count != 1 {
        return Err(format!(
            "generated TypeScript must contain exactly one {INIT_OUTPUT_DECLARATION:?}; found {marker_count}"
        ));
    }
    let boundary = syntax_source
        .find(INIT_OUTPUT_DECLARATION)
        .expect("the count check proved that the marker exists");
    let declaration_source = std::str::from_utf8(&declarations)
        .expect("blanking TypeScript regions preserves valid UTF-8");

    let open = syntax[boundary..]
        .iter()
        .position(|byte| *byte == b'{')
        .map(|offset| boundary + offset)
        .ok_or_else(|| "generated InitOutput declaration has no interface body".to_string())?;
    let mut depth = 0_usize;
    let mut close = None;
    for (offset, byte) in syntax[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    "generated InitOutput interface has unbalanced braces".to_string()
                })?;
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let close =
        close.ok_or_else(|| "generated InitOutput interface has no closing brace".to_string())?;
    syntax[open..=close].fill(b' ');

    let any = Regex::new(r"\bany\b").expect("valid TypeScript any matcher");
    if any.is_match(&syntax) {
        return Err(
            "generated TypeScript exposes `any` outside the low-level InitOutput ABI body".into(),
        );
    }
    let ergonomic_declarations = &declaration_source[..boundary];
    let ergonomic_syntax = &syntax[..boundary];

    for required in REQUIRED_TYPESCRIPT_CONTRACT_SNIPPETS {
        let count = count_typescript_code_occurrences(
            ergonomic_declarations,
            ergonomic_syntax,
            required,
            &non_code,
        );
        if count != 1 {
            return Err(format!(
                "generated TypeScript must contain exactly one ergonomic contract {required:?}; found {count}"
            ));
        }
    }
    for required in REQUIRED_TYPESCRIPT_SIGNATURES {
        let count = count_typescript_code_occurrences(
            ergonomic_declarations,
            ergonomic_syntax,
            required,
            &non_code,
        );
        if count != 1 {
            return Err(format!(
                "generated TypeScript must contain exactly one ergonomic signature {required:?}; found {count}"
            ));
        }
    }
    Ok(())
}

fn blank_typescript_region(region: &mut [u8]) {
    for byte in region {
        if !matches!(*byte, b'\r' | b'\n') {
            *byte = b' ';
        }
    }
}

fn count_typescript_code_occurrences(
    declarations: &str,
    syntax: &[u8],
    required: &str,
    non_code: &Regex,
) -> usize {
    let mut required_syntax = required.as_bytes().to_vec();
    for matched in non_code.find_iter(required.as_bytes()) {
        blank_typescript_region(&mut required_syntax[matched.start()..matched.end()]);
    }
    declarations
        .match_indices(required)
        .filter(|(start, _)| {
            syntax.get(*start..*start + required.len()) == Some(required_syntax.as_slice())
        })
        .count()
}

fn require_nonempty(file: &Path, label: &str) -> TaskResult {
    let metadata = fs::metadata(file)
        .map_err(|error| format!("{label} is missing ({}): {error}", file.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{label} is missing or empty: {}", file.display()));
    }
    Ok(())
}

fn require_identical(left: &Path, right: &Path, label: &str) -> TaskResult {
    let left_bytes =
        fs::read(left).map_err(|error| format!("cannot read {}: {error}", left.display()))?;
    let right_bytes =
        fs::read(right).map_err(|error| format!("cannot read {}: {error}", right.display()))?;
    if left_bytes != right_bytes {
        return Err(format!(
            "{label}: {} differs from {}",
            right.display(),
            left.display()
        ));
    }
    Ok(())
}

fn reject_builder_paths(root: &Path, file: &Path) -> TaskResult {
    let data =
        fs::read(file).map_err(|error| format!("cannot inspect {}: {error}", file.display()))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut configured = vec![root.to_path_buf()];
    if let Some(home) = home {
        configured.push(home.clone());
        configured
            .push(env::var_os("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from));
        configured
            .push(env::var_os("RUSTUP_HOME").map_or_else(|| home.join(".rustup"), PathBuf::from));
    }
    for directory in configured {
        if let Ok(canonical) = directory.canonicalize() {
            let needle = canonical.to_string_lossy();
            if needle.len() > 1
                && data
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            {
                return Err(format!(
                    "generated file contains a builder-local path: {}",
                    file.display()
                ));
            }
        }
    }
    let generic = [
        (
            Regex::new(r"/Users/[^/\x00]+/").expect("valid regex"),
            "a macOS home directory",
        ),
        (
            Regex::new(r"/home/[^/\x00]+/").expect("valid regex"),
            "a Unix home directory",
        ),
        (
            Regex::new(r"(?i:[A-Z]:\\Users\\[^\\\x00]+\\)").expect("valid regex"),
            "a Windows home directory",
        ),
        (
            Regex::new(r"/(?:__w|_work|github/workspace)/").expect("valid regex"),
            "a hosted-runner workspace",
        ),
    ];
    for (pattern, description) in generic {
        if pattern.is_match(&data) {
            return Err(format!(
                "generated file contains {description}: {}",
                file.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_export_conditions_keep_types_first_and_default_last() {
        let valid = json!({
            "exports": {
                ".": {
                    "types": "./pinewasm.d.ts",
                    "import": "./pinewasm.js",
                    "default": "./pinewasm.js"
                }
            }
        });
        let object = valid.as_object().unwrap();
        validate_root_export_conditions(object).unwrap();
        let rendered = serde_json::to_string_pretty(&PackageManifest(object)).unwrap();
        let types = rendered.find("\"types\":").unwrap();
        let import = rendered.find("\"import\":").unwrap();
        let default = rendered.find("\"default\":").unwrap();
        assert!(types < import && import < default, "{rendered}");

        let invalid: Value = serde_json::from_str(
            r#"{"exports":{".":{"default":"./pinewasm.js","import":"./pinewasm.js"}}}"#,
        )
        .unwrap();
        let error = validate_root_export_conditions(invalid.as_object().unwrap()).unwrap_err();
        assert!(error.contains("types"));

        let unknown = json!({
            "types": "./pinewasm.d.ts",
            "import": "./pinewasm.js",
            "default": "./pinewasm.js",
            "browser": "./pinewasm.js"
        });
        let error = serde_json::to_string(&RootExportConditions(&unknown)).unwrap_err();
        assert!(error.to_string().contains("exactly 3 conditions"));
    }

    fn valid_typescript_contract() -> String {
        let mut source = REQUIRED_TYPESCRIPT_CONTRACT_SNIPPETS.join("\n");
        source.push('\n');
        source.push_str(&REQUIRED_TYPESCRIPT_SIGNATURES.join("\n"));
        source.push('\n');
        source.push_str(INIT_OUTPUT_DECLARATION);
        source.push_str(" { raw(value: any): any; }\n");
        source
    }

    #[test]
    fn typescript_contract_accepts_any_only_in_low_level_abi() {
        validate_typescript_contract_source(&valid_typescript_contract()).unwrap();
    }

    #[test]
    fn typescript_contract_distinguishes_program_metadata_from_program() {
        let source = valid_typescript_contract().replace(
            "export interface Program {",
            "export interface ProgramMetadata { title: string | null; }\nexport interface Program {",
        );
        validate_typescript_contract_source(&source).unwrap();
    }

    #[test]
    fn typescript_contract_rejects_any_in_ergonomic_api() {
        let source = valid_typescript_contract().replacen(
            REQUIRED_TYPESCRIPT_SIGNATURES[0],
            &format!(
                "{}\ntype Regression = any;",
                REQUIRED_TYPESCRIPT_SIGNATURES[0]
            ),
            1,
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exposes `any` outside"));
    }

    #[test]
    fn typescript_contract_ignores_any_in_documentation_and_string_literals() {
        let source = valid_typescript_contract().replacen(
            REQUIRED_TYPESCRIPT_SIGNATURES[0],
            &format!(
                "/** This can describe any supported value without declaring the any type. */\n\
                 type LiteralRegression = \"any\";\n{}",
                REQUIRED_TYPESCRIPT_SIGNATURES[0]
            ),
            1,
        );
        validate_typescript_contract_source(&source).unwrap();
    }

    #[test]
    fn typescript_contract_rejects_any_after_the_low_level_abi() {
        let source = format!(
            "{}\nexport type PostAbiRegression = any;\n",
            valid_typescript_contract()
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exposes `any` outside"));
    }

    #[test]
    fn typescript_contract_rejects_object_input_map() {
        let source = valid_typescript_contract().replace(
            "readonly inputs?: Map<number, InputValue>;",
            "readonly inputs?: Record<number, InputValue>;",
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("Map<number, InputValue>"));
    }

    #[test]
    fn typescript_contract_preserves_exactly_one_input_tag() {
        let source = valid_typescript_contract().replace(
            "readonly [Other in Exclude<InputValueTag, Tag>]?: never",
            "readonly [Other in Exclude<InputValueTag, Tag>]?: unknown",
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("Exclude<InputValueTag, Tag>]?: never"));
    }

    #[test]
    fn typescript_contract_preserves_null_and_omitted_fields() {
        let retained_null = valid_typescript_contract().replace(
            "readonly astDebug: string | null;",
            "readonly astDebug?: string;",
        );
        let error = validate_typescript_contract_source(&retained_null).unwrap_err();
        assert!(error.contains("readonly astDebug: string | null;"));

        let omitted = valid_typescript_contract().replace(
            "readonly strategy?: StrategyOutcome;",
            "readonly strategy: StrategyOutcome | null;",
        );
        let error = validate_typescript_contract_source(&omitted).unwrap_err();
        assert!(error.contains("readonly strategy?: StrategyOutcome;"));
    }

    #[test]
    fn typescript_contract_preserves_nullable_standalone_wasm_options() {
        for (property, nullable, non_nullable) in [
            (
                "signal_plot",
                "readonly signal_plot?: IntegerInput | null;",
                "readonly signal_plot?: IntegerInput;",
            ),
            (
                "signal_semantics",
                "readonly signal_semantics?: \"position-events\" | \"posture-events\" | null;",
                "readonly signal_semantics?: \"position-events\" | \"posture-events\";",
            ),
            (
                "max_collection_elements",
                "readonly max_collection_elements?: number | null;",
                "readonly max_collection_elements?: number;",
            ),
        ] {
            let source = valid_typescript_contract().replace(nullable, non_nullable);
            let error = validate_typescript_contract_source(&source).unwrap_err();
            assert!(error.contains(property));
        }
    }

    #[test]
    fn typescript_contract_rejects_missing_exact_signature() {
        let source = valid_typescript_contract().replacen(REQUIRED_TYPESCRIPT_SIGNATURES[0], "", 1);
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exactly one ergonomic signature"));
        assert!(error.contains("found 0"));
    }

    #[test]
    fn typescript_contract_rejects_duplicate_exact_signature() {
        let signature = REQUIRED_TYPESCRIPT_SIGNATURES[0];
        let source = valid_typescript_contract().replacen(
            signature,
            &format!("{signature}\n{signature}"),
            1,
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exactly one ergonomic signature"));
        assert!(error.contains("found 2"));
    }

    #[test]
    fn typescript_contract_rejects_duplicate_exact_contract() {
        let contract = REQUIRED_TYPESCRIPT_CONTRACT_SNIPPETS[0];
        let source =
            valid_typescript_contract().replacen(contract, &format!("{contract}\n{contract}"), 1);
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exactly one ergonomic contract"));
        assert!(error.contains("found 2"));
    }

    #[test]
    fn typescript_contract_rejects_signature_shadowed_by_comment_or_string() {
        let signature = REQUIRED_TYPESCRIPT_SIGNATURES[0];
        for shadow in [
            format!("// {signature}"),
            format!("type Shadow = '{signature}';"),
        ] {
            let source = valid_typescript_contract().replacen(signature, &shadow, 1);
            let error = validate_typescript_contract_source(&source).unwrap_err();
            assert!(error.contains("exactly one ergonomic signature"), "{error}");
            assert!(error.contains("found 0"), "{error}");
        }
    }

    #[test]
    fn typescript_contract_rejects_contract_shadowed_by_comment() {
        let contract = REQUIRED_TYPESCRIPT_CONTRACT_SNIPPETS[1];
        let source = valid_typescript_contract().replacen(contract, &format!("// {contract}"), 1);
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exactly one ergonomic contract"), "{error}");
        assert!(error.contains("found 0"), "{error}");
    }

    #[test]
    fn typescript_contract_rejects_boundary_marker_shadowed_by_non_code() {
        for shadow in [
            format!("// {INIT_OUTPUT_DECLARATION}"),
            format!("type Shadow = '{INIT_OUTPUT_DECLARATION}';"),
        ] {
            let source = valid_typescript_contract().replacen(INIT_OUTPUT_DECLARATION, &shadow, 1);
            let error = validate_typescript_contract_source(&source).unwrap_err();
            assert!(error.contains("exactly one"), "{error}");
            assert!(error.contains("found 0"), "{error}");
        }
    }

    #[test]
    fn typescript_contract_fails_closed_on_template_literals() {
        let source = format!(
            "type TemplateRegression = `it's hidden`;\n{}",
            valid_typescript_contract()
        );
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("template-literal syntax"), "{error}");
    }

    #[test]
    fn typescript_contract_rejects_ambiguous_low_level_boundary() {
        let source = format!("{}\n{INIT_OUTPUT_DECLARATION}", valid_typescript_contract());
        let error = validate_typescript_contract_source(&source).unwrap_err();
        assert!(error.contains("exactly one"));
    }
}
