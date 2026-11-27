//! `artifact-notice`: the mechanical notice bundle for emitted artifacts.
//!
//! An emitted module can carry function bodies and rodata copied from the
//! checked-in runtime blobs, so the bytes a caller receives are not a
//! notice-complete distribution on their own (`docs/artifact-distribution.md`).
//! This command materializes the material a distributor must convey next to
//! such artifacts: the project license, the project notice, the license texts
//! of the runtime's third-party closure, and a manifest that pins the artifact
//! identity (ABI and metadata versions, runtime blob digests, corresponding
//! source) together with the licensing status of the emitted bytes.
//!
//! The notice model is public-source distribution: when an artifact is
//! conveyed, its complete corresponding source must accompany it through the
//! repository/commit recorded in the bundle or another AGPL-compliant method.
//! No separate runtime output exception is granted. The manifest records that
//! model, and `check_notice_text` refuses a notice whose shipped text departs
//! from it.

use crate::command::{
    git, reject_untrusted_cargo_sources, repo_root, sha256, sha256_file, temp_dir,
};
use crate::coverage::{documented_runtime_checksums, AOT_ARTIFACTS};
use crate::licenses::{self, Inventory, InventoryPackage};
use crate::TaskResult;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Every file the bundle contains. `release-check` requires each one.
pub(crate) const BUNDLE_FILES: [&str; 4] = [
    "LICENSE",
    "NOTICE",
    "THIRD_PARTY_LICENSES.txt",
    "artifact-manifest.json",
];
const MANIFEST_FILE: &str = "artifact-manifest.json";
const MANIFEST_SCHEMA_VERSION: u64 = 1;
const LICENSE_EXPRESSION: &str = "AGPL-3.0-only";
/// No runtime output exception is granted. The notice model makes the
/// complete corresponding source public under the same license as the spliced
/// runtime bytes; see `docs/artifact-distribution.md`.
const RUNTIME_EXCEPTION_STATUS: &str = "none";
/// Revision of the notice model rendered into every shipped notice and manifest.
const NOTICE_REVISION: &str = "artifact-notice-v1";
/// The dependency elections below are the project's recorded elections.
const ELECTION_STATUS: &str = "recorded";
const RUNTIME_MANIFEST: &str = "crates/pinewasm_aot/runtime/Cargo.toml";
const RUNTIME_DIRECTORY: &str = "crates/pinewasm_aot/runtime";
const AOT_DOCUMENT: &str = "docs/aot.md";
const AOT_LIBRARY_SOURCE: &str = "crates/pinewasm_aot/src/lib.rs";
const AOT_SCHEMA_SOURCE: &str = "crates/pinewasm_aot/src/schema.rs";
const PACKAGE_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const PACKAGE_VERSION: &str = crate::version::COMPILER_VERSION;

/// The one sentence that states the generated-artifact notice model. It must
/// appear verbatim in every document that describes what an emitted artifact
/// contains, `NOTICE` first because the bundle ships a verbatim copy of it
/// beside the artifact. The sentence defines where the corresponding source
/// for a conveyed artifact is.
pub(crate) const NOTICE_MODEL_SENTENCE: &str = "PineWASM distributes artifacts under AGPL-3.0-only: when an artifact is conveyed, its corresponding source must be made available from the PineWASM repository at the commit recorded in the artifact notice bundle or through another AGPL-compliant method; the repository URL and commit identify that source, and no separate runtime output exception is granted.";

const POLICY_BLOCK_BEGIN: &str = "<!-- BEGIN GENERATED ARTIFACT POLICY -->";
const POLICY_BLOCK_END: &str = "<!-- END GENERATED ARTIFACT POLICY -->";
const NOTICE_INFORMATIONAL_PLACEHOLDER: &str = "{{GENERATED_ARTIFACT_POLICY}}";
// SHA-256 of NOTICE after replacing its one generated policy block with
// NOTICE_INFORMATIONAL_PLACEHOLDER. This makes the complete shipped NOTICE a
// reviewed byte-exact artifact: the generated block comes from the constants
// below, while edits to informational attribution require an explicit digest
// update. It deliberately does not claim to understand arbitrary legal prose.
const NOTICE_INFORMATIONAL_SHA256: &str =
    "fef4ff46c2e6e8d113cde6fc6e0d39f43ffa0c16d155e1128ab2f316c3d3d748";
const LICENSE_SHA256: &str = "0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0";
const AOT_INFORMATIONAL_SHA256: &str =
    "36b03920f2f91dfa16f10bb08ef62e2a42903d201827993f7d6dd5b3195bc881";
const DISTRIBUTION_INFORMATIONAL_SHA256: &str =
    "612224c641613c20ad125a719d4631f378dd0ac0199390a0eeef116332a886f2";

/// Documents that must contain the generated authoritative block byte-for-byte.
pub(crate) const NOTICE_MODEL_DOCUMENTS: [&str; 3] =
    ["NOTICE", "docs/artifact-distribution.md", AOT_DOCUMENT];

/// Anchor that must not appear in any shipped document. Requiring its absence
/// everywhere makes a document that still links to it fail the check here,
/// instead of shipping a notice that points at a missing section.
const RETIRED_NOTICE_ANCHOR: &str = "superseded-notice-anchor";

/// Every document whose text must agree with the notice model. Wider than
/// `NOTICE_MODEL_DOCUMENTS`: these need not state the model, but none of them
/// may link to the anchor in `RETIRED_NOTICE_ANCHOR`.
const NOTICE_DOCUMENTS: [&str; 7] = [
    "NOTICE",
    "README.md",
    "docs/licensing.md",
    "CONTRIBUTING.md",
    AOT_DOCUMENT,
    "docs/artifact-distribution.md",
    "docs/releasing.md",
];

/// The recorded license for every multi-licensed expression in the resolved
/// dependency graphs, keyed by the exact SPDX expression. A multi-licensed
/// runtime dependency whose expression is absent here fails the command, so
/// a new dual-licensed crate cannot enter an artifact without a recorded
/// election. `THIRD_PARTY_LICENSES.txt` still reproduces every text each
/// package ships, so a recipient holds the alternatives too (`NOTICE`).
pub(crate) const LICENSE_ELECTIONS: [(&str, &str, &str); 6] = [
    (
        "Apache-2.0 OR BSL-1.0",
        "Apache-2.0",
        "the election for the Apache-2.0 OR BSL-1.0 family is Apache-2.0",
    ),
    (
        "(MIT OR Apache-2.0) AND Unicode-3.0",
        "MIT AND Unicode-3.0",
        "the election for the (MIT OR Apache-2.0) AND Unicode-3.0 family is MIT AND Unicode-3.0",
    ),
    (
        "Apache-2.0 OR MIT",
        "MIT",
        "the election for the Apache-2.0 OR MIT family is MIT",
    ),
    (
        "MIT OR Apache-2.0",
        "MIT",
        "the election for the MIT OR Apache-2.0 family is MIT",
    ),
    (
        "Unlicense OR MIT",
        "MIT",
        "the election for the Unlicense OR MIT family is MIT",
    ),
    (
        "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
        "Apache-2.0",
        "the election for the MIT OR Apache-2.0 OR LGPL-2.1-or-later family is Apache-2.0",
    ),
];

/// Artifact identity pins read from the AOT crate's source. Parsing the source
/// rather than depending on `pinewasm_aot` keeps the tooling crate's build
/// graph small; the values are asserted against the documentation so a bump
/// in either place is a reviewed change.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArtifactConstants {
    pub(crate) csaw_abi: u64,
    pub(crate) module_metadata_schema: u64,
    pub(crate) standalone_wasm_abi: u64,
}

/// Everything the manifest is rendered from, gathered separately so the
/// rendering is a pure function that tests can drive without Cargo or Git.
pub(crate) struct ManifestInputs<'a> {
    pub(crate) constants: ArtifactConstants,
    pub(crate) runtime_blobs: BTreeMap<String, String>,
    pub(crate) packages: &'a [InventoryPackage],
    pub(crate) repository: &'a str,
    pub(crate) commit: &'a str,
}

/// The complete bundle reconstructed from one trusted source checkout.
///
/// Verification compares emitted bytes with this snapshot rather than with
/// claims made by the emitted manifest itself. Keeping the snapshot private to
/// this module prevents callers from treating an arbitrary parsed bundle as
/// its own authority.
#[derive(Debug)]
struct TrustedBundle {
    license: String,
    notice: String,
    runtime_licenses: String,
    manifest: Value,
}

/// Validate the structural policy artifact: every policy document contains the
/// exact generated block, the complete shipped NOTICE has the reviewed
/// informational shell around that block, and no document links to the
/// anchor in `RETIRED_NOTICE_ANCHOR`. Prose outside generated blocks is informational.
pub(crate) fn check_notice_text(root: &Path) -> TaskResult {
    let mut failures = Vec::new();
    for document in NOTICE_MODEL_DOCUMENTS {
        match fs::read_to_string(root.join(document)) {
            Ok(text) if document == "NOTICE" => {
                if let Err(error) = check_authoritative_notice(&text) {
                    failures.push(error);
                }
            }
            Ok(text) => {
                if let Err(error) = check_authoritative_policy_document(document, &text) {
                    failures.push(error);
                }
            }
            Err(error) => failures.push(format!("cannot read {document}: {error}")),
        }
    }
    for document in NOTICE_DOCUMENTS {
        match fs::read_to_string(root.join(document)) {
            Ok(text) if text.contains(RETIRED_NOTICE_ANCHOR) => failures.push(format!(
                "{document} still refers to the superseded `{RETIRED_NOTICE_ANCHOR}` anchor"
            )),
            Ok(_) => {}
            Err(error) => failures.push(format!("cannot read {document}: {error}")),
        }
    }
    if failures.is_empty() {
        println!(
            "Generated-artifact policy block is byte-exact in {} documents.",
            NOTICE_MODEL_DOCUMENTS.len()
        );
        Ok(())
    } else {
        Err(format!(
            "generated-artifact notice text does not implement the notice model:\n  {}",
            failures.join("\n  ")
        ))
    }
}

fn render_authoritative_policy_block() -> String {
    let mut lines = vec![
        POLICY_BLOCK_BEGIN.to_string(),
        "This block is generated from PineWASM's artifact notice model.".into(),
        format!("Notice revision: {NOTICE_REVISION}"),
        format!("Project license: {LICENSE_EXPRESSION}"),
        format!("Runtime output exception: {RUNTIME_EXCEPTION_STATUS}"),
        format!("Dependency-license status: {ELECTION_STATUS}"),
        String::new(),
        NOTICE_MODEL_SENTENCE.into(),
        String::new(),
        "Dependency license elections:".into(),
    ];
    lines.extend(
        LICENSE_ELECTIONS
            .iter()
            .map(|(expression, elected, _)| format!("- {expression} -> {elected}")),
    );
    lines.push(POLICY_BLOCK_END.into());
    lines.join("\n")
}

fn split_policy_block<'a>(
    document: &str,
    text: &'a str,
) -> TaskResult<(&'a str, &'a str, &'a str)> {
    let (before, remainder) = text
        .split_once(POLICY_BLOCK_BEGIN)
        .ok_or_else(|| format!("{document} lacks the generated artifact-policy block"))?;
    if remainder.contains(POLICY_BLOCK_BEGIN) {
        return Err(format!(
            "{document} contains more than one generated artifact-policy block"
        ));
    }
    let (body, after) = remainder
        .split_once(POLICY_BLOCK_END)
        .ok_or_else(|| format!("{document} has an unterminated generated artifact-policy block"))?;
    if after.contains(POLICY_BLOCK_END) {
        return Err(format!(
            "{document} contains more than one generated artifact-policy block"
        ));
    }
    Ok((before, body, after))
}

fn check_authoritative_policy_block(document: &str, text: &str) -> TaskResult {
    let (before, body, after) = split_policy_block(document, text)?;
    let actual = format!("{POLICY_BLOCK_BEGIN}{body}{POLICY_BLOCK_END}");
    let expected = render_authoritative_policy_block();
    if actual != expected {
        return Err(format!(
            "{document} generated artifact-policy block is not byte-exact; regenerate it from the approved manifest policy"
        ));
    }
    // The split also proves the markers occur exactly once. Outside prose is
    // intentionally informational and is not interpreted as legal semantics.
    let _ = (before, after);
    Ok(())
}

fn informational_shell(document: &str, text: &str) -> TaskResult<String> {
    let (before, _, after) = split_policy_block(document, text)?;
    Ok(format!("{before}{NOTICE_INFORMATIONAL_PLACEHOLDER}{after}"))
}

fn check_authoritative_policy_document(document: &str, text: &str) -> TaskResult {
    check_authoritative_policy_block(document, text)?;
    let expected = match document {
        AOT_DOCUMENT => AOT_INFORMATIONAL_SHA256,
        "docs/artifact-distribution.md" => DISTRIBUTION_INFORMATIONAL_SHA256,
        _ => return Err(format!("no reviewed informational digest for {document}")),
    };
    let actual = sha256(informational_shell(document, text)?.as_bytes());
    if actual != expected {
        return Err(format!(
            "{document} informational bytes do not match the reviewed template; expected SHA-256 {expected}, found {actual}"
        ));
    }
    Ok(())
}

/// Validate every byte of the NOTICE a recipient receives. The authoritative
/// block is rendered from machine-readable policy constants; the remaining
/// attribution is bound by a reviewed digest. This rejects every append-only
/// mutation without pretending to classify arbitrary prose semantically.
fn check_authoritative_notice(notice: &str) -> TaskResult {
    check_authoritative_policy_block("NOTICE", notice)?;
    let shell_digest = sha256(informational_shell("NOTICE", notice)?.as_bytes());
    if shell_digest != NOTICE_INFORMATIONAL_SHA256 {
        return Err(format!(
            "NOTICE informational bytes do not match the reviewed template; expected SHA-256 {NOTICE_INFORMATIONAL_SHA256}, found {shell_digest}"
        ));
    }
    Ok(())
}

pub(crate) fn run(args: &[String]) -> TaskResult {
    let mut out = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--out" if index + 1 < args.len() => {
                out = Some(Path::new(&args[index + 1]).to_path_buf());
                index += 1;
            }
            _ => return Err("usage: artifact-notice --out DIR".into()),
        }
        index += 1;
    }
    let out = out.ok_or_else(|| "usage: artifact-notice --out DIR".to_string())?;
    let root = repo_root()?;
    let trusted = generate(&root, &out)?;
    verify_bundle(&out, &trusted)?;
    println!(
        "Wrote and verified the generated-artifact notice bundle at {}: {}",
        out.display(),
        BUNDLE_FILES.join(", ")
    );
    Ok(())
}

/// Generate the bundle into a temporary directory and verify it, without
/// leaving anything in the worktree. Part of `release-check`.
pub(crate) fn release_gate() -> TaskResult {
    let root = repo_root()?;
    let temporary = temp_dir("artifact-notice")?;
    let trusted = generate(&root, temporary.path())?;
    verify_bundle(temporary.path(), &trusted)?;
    // A bundle that parses can still carry notice text describing a model the
    // repository no longer implements, so validate the text here as well as in
    // the metadata check, and this gate runs even if an earlier check fails.
    check_notice_text(&root)?;
    println!("Generated-artifact notice bundle: complete and parseable.");
    Ok(())
}

fn generate(root: &Path, out: &Path) -> TaskResult<TrustedBundle> {
    if !root.join(RUNTIME_MANIFEST).is_file() {
        return Err(format!("not a PineWASM checkout: {}", root.display()));
    }
    // Cargo metadata retains logical registry identities for substituted sources.
    // Reject source replacement before reading Git state, package bytes or licenses.
    reject_untrusted_cargo_sources(root)?;
    // A HEAD-only source reference must not describe uncommitted inputs.
    if !git(root, &["status", "--porcelain", "--untracked-files=all"])?
        .trim()
        .is_empty()
    {
        return Err("artifact-notice requires a clean source checkout so corresponding_source.commit identifies its inputs".into());
    }
    let trusted = reconstruct_trusted_bundle(root)?;
    write_trusted_bundle(out, &trusted)?;
    Ok(trusted)
}

/// Rebuild every asserted bundle value from the checkout that authorizes it.
fn reconstruct_trusted_bundle(root: &Path) -> TaskResult<TrustedBundle> {
    let inventory = licenses::runtime_inventory(root)?;
    let runtime_blobs = runtime_checksums(root)?;
    let constants = artifact_constants(root)?;
    let commit = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let manifest = render_manifest(&ManifestInputs {
        constants,
        runtime_blobs,
        packages: &inventory.packages,
        repository: PACKAGE_REPOSITORY,
        commit: &commit,
    })?;
    trusted_bundle_from(root, &inventory, manifest)
}

fn trusted_bundle_from(
    root: &Path,
    inventory: &Inventory,
    manifest: Value,
) -> TaskResult<TrustedBundle> {
    let license = read(&root.join("LICENSE"))?;
    let license_digest = sha256(license.as_bytes());
    if license_digest != LICENSE_SHA256 {
        return Err(format!(
            "LICENSE bytes do not match the reviewed AGPL-3.0-only text; expected SHA-256 {LICENSE_SHA256}, found {license_digest}"
        ));
    }
    let notice = read(&root.join("NOTICE"))?;
    check_authoritative_notice(&notice)?;
    Ok(TrustedBundle {
        license,
        notice,
        runtime_licenses: render_runtime_licenses(inventory),
        manifest,
    })
}

/// Digest the four tracked runtime blobs and require each to match the
/// reviewed checksum table in `docs/aot.md`, so the manifest can only ever
/// pin bytes that the documentation and CI already pin.
pub(crate) fn runtime_checksums(root: &Path) -> TaskResult<BTreeMap<String, String>> {
    let documented = documented_runtime_checksums(&root.join(AOT_DOCUMENT))?;
    let mut checksums = BTreeMap::new();
    for artifact in AOT_ARTIFACTS {
        let actual = sha256_file(&root.join(RUNTIME_DIRECTORY).join(artifact))?;
        match documented.get(artifact) {
            Some(pinned) if pinned == &actual => {}
            Some(pinned) => {
                return Err(format!(
                    "{artifact} does not match the {AOT_DOCUMENT} checksum pin; documented={pinned}, actual={actual}"
                ));
            }
            None => {
                return Err(format!("{AOT_DOCUMENT} has no checksum row for {artifact}"));
            }
        }
        checksums.insert(artifact.to_string(), actual);
    }
    Ok(checksums)
}

pub(crate) fn artifact_constants(root: &Path) -> TaskResult<ArtifactConstants> {
    let library = read(&root.join(AOT_LIBRARY_SOURCE))?;
    let schema = read(&root.join(AOT_SCHEMA_SOURCE))?;
    constants_from_sources(&library, &schema)
}

fn constants_from_sources(library: &str, schema: &str) -> TaskResult<ArtifactConstants> {
    let abi_re = Regex::new(r"(?m)^\s*pub const ABI_VERSION: u32 = (\d+);").expect("valid regex");
    let schema_re = Regex::new(r#"\\"schema_version\\":(\d+)"#).expect("valid regex");
    let standalone_wasm_abi_re = Regex::new(r#",\\"abi_version\\":(\d+)"#).expect("valid regex");
    Ok(ArtifactConstants {
        csaw_abi: single_value(&abi_re, library, "header::ABI_VERSION", AOT_LIBRARY_SOURCE)?,
        module_metadata_schema: single_value(
            &schema_re,
            schema,
            "module_metadata schema_version",
            AOT_SCHEMA_SOURCE,
        )?,
        standalone_wasm_abi: single_value(
            &standalone_wasm_abi_re,
            schema,
            "standalone WebAssembly abi_version",
            AOT_SCHEMA_SOURCE,
        )?,
    })
}

/// Every occurrence of the constant must agree; a source that emits two
/// different versions is a bug the manifest must not paper over.
fn single_value(pattern: &Regex, source: &str, label: &str, file: &str) -> TaskResult<u64> {
    let mut values = pattern
        .captures_iter(source)
        .map(|capture| {
            capture[1]
                .parse::<u64>()
                .map_err(|error| format!("{label} in {file} is not an integer: {error}"))
        })
        .collect::<TaskResult<Vec<_>>>()?;
    values.sort_unstable();
    values.dedup();
    match values.as_slice() {
        [value] => Ok(*value),
        [] => Err(format!("{label} not found in {file}")),
        _ => Err(format!(
            "{label} has conflicting values in {file}: {values:?}"
        )),
    }
}

pub(crate) fn render_manifest(inputs: &ManifestInputs<'_>) -> TaskResult<Value> {
    let mut elections = serde_json::Map::new();
    for package in inputs.packages {
        let elected = elect(&package.expression).ok_or_else(|| {
            format!(
                "no recorded license election for {} ({}); add one to LICENSE_ELECTIONS and NOTICE",
                package.name, package.expression
            )
        })?;
        elections.insert(
            format!("{}@{}", package.name, package.version),
            json!({ "expression": package.expression, "elected": elected }),
        );
    }
    Ok(json!({
        "manifest_schema_version": MANIFEST_SCHEMA_VERSION,
        "generator": "pinewasm_xtask artifact-notice",
        "pinewasm_version": PACKAGE_VERSION,
        "license": LICENSE_EXPRESSION,
        "runtime_exception": RUNTIME_EXCEPTION_STATUS,
        "notice_revision": NOTICE_REVISION,
        "abi_version": inputs.constants.csaw_abi,
        "module_metadata_schema_version": inputs.constants.module_metadata_schema,
        "standalone_wasm_abi_version": inputs.constants.standalone_wasm_abi,
        "runtime_blobs": inputs.runtime_blobs,
        "runtime_closure": inputs.packages.iter().map(InventoryPackage::label).collect::<Vec<_>>(),
        "elections": {
            "status": ELECTION_STATUS,
            "notice_revision": NOTICE_REVISION,
            "packages": elections,
        },
        "corresponding_source": {
            "repository": inputs.repository,
            "commit": inputs.commit,
        },
        "bundle": BUNDLE_FILES,
    }))
}

/// The license elected for one SPDX expression: the expression itself when it
/// offers no choice, otherwise the recorded election. `/` is accepted as the
/// alternative spelling of `OR`.
pub(crate) fn elect(expression: &str) -> Option<String> {
    let normalized = expression.replace('/', " OR ");
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    if !normalized.contains(" OR ") {
        return Some(normalized);
    }
    LICENSE_ELECTIONS
        .iter()
        .find(|(candidate, _, _)| *candidate == normalized)
        .map(|(_, elected, _)| (*elected).to_string())
}

#[cfg(test)]
pub(crate) fn write_bundle(
    root: &Path,
    out: &Path,
    inventory: &Inventory,
    manifest: &Value,
) -> TaskResult {
    let trusted = trusted_bundle_from(root, inventory, manifest.clone())?;
    write_trusted_bundle(out, &trusted)
}

fn write_trusted_bundle(out: &Path, trusted: &TrustedBundle) -> TaskResult {
    // A verified bundle is an exact directory, not four trusted files mixed
    // with arbitrary pre-existing material.
    match fs::symlink_metadata(out) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "bundle output is not a trusted directory: {}",
                    out.display()
                ));
            }
            if fs::read_dir(out)
                .map_err(|error| format!("cannot inspect {}: {error}", out.display()))?
                .next()
                .is_some()
            {
                return Err(format!(
                    "bundle output directory is not empty: {}; choose a fresh directory",
                    out.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(out)
                .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
        }
        Err(error) => return Err(format!("cannot inspect {}: {error}", out.display())),
    }
    write(&out.join("LICENSE"), &trusted.license)?;
    write(&out.join("NOTICE"), &trusted.notice)?;
    write(
        &out.join("THIRD_PARTY_LICENSES.txt"),
        &trusted.runtime_licenses,
    )?;
    let rendered = serde_json::to_string_pretty(&trusted.manifest)
        .map_err(|error| format!("cannot render {MANIFEST_FILE}: {error}"))?;
    write(&out.join(MANIFEST_FILE), &format!("{rendered}\n"))
}

fn render_runtime_licenses(inventory: &Inventory) -> String {
    let mut parts = vec![
        "PINEWASM GENERATED-ARTIFACT RUNTIME LICENSES".to_string(),
        "=".repeat(44),
        String::new(),
        "This file is generated by the Rust-native `pinewasm_xtask artifact-notice` command."
            .into(),
        "Do not edit it by hand.".into(),
        String::new(),
        "It lists the license texts of the third-party dependency closure of the".into(),
        "linked runtime modules under crates/pinewasm_aot/runtime, the code PineWASM".into(),
        "can splice into an emitted artifact. The complete repository inventory is the".into(),
        "THIRD_PARTY_LICENSES.txt file at the source repository.".into(),
        String::new(),
        "Source repository:".into(),
        PACKAGE_REPOSITORY.into(),
        String::new(),
        format!(
            "Runtime closure: {} production Cargo packages.",
            inventory.packages.len()
        ),
    ];
    parts.extend(
        inventory
            .packages
            .iter()
            .map(|package| format!("  - {}", package.label())),
    );
    parts.push(String::new());
    parts.push(
        "Identical license texts are printed once and list every package that uses them.".into(),
    );
    parts.push(String::new());
    let groups = inventory
        .groups
        .iter()
        .map(|(key, group)| {
            (
                key.clone(),
                licenses::LicenseText {
                    text: group.text.clone(),
                    uses: group.uses.clone(),
                },
            )
        })
        .collect();
    parts.extend(licenses::render_groups(groups));
    format!("{}\n", parts.join("\n").trim_end())
}

/// Require every bundle file and a manifest that parses with the pins a
/// distributor relies on.
fn verify_bundle(out: &Path, trusted: &TrustedBundle) -> TaskResult<Value> {
    let mut actual_files = fs::read_dir(out)
        .map_err(|error| format!("cannot inspect bundle directory {}: {error}", out.display()))?
        .map(|entry| {
            let entry = entry.map_err(|error| format!("cannot inspect bundle entry: {error}"))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "unsupported bundle entry: {}",
                    entry.path().display()
                ));
            }
            entry
                .file_name()
                .into_string()
                .map_err(|_| "bundle entry name is not UTF-8".to_string())
        })
        .collect::<TaskResult<Vec<_>>>()?;
    actual_files.sort();
    let mut expected_files = BUNDLE_FILES.map(str::to_string).to_vec();
    expected_files.sort();
    if actual_files != expected_files {
        return Err(format!(
            "bundle file set does not exactly match the trusted set; expected {expected_files:?}, found {actual_files:?}"
        ));
    }
    for file in BUNDLE_FILES {
        let path = out.join(file);
        let size = fs::metadata(&path)
            .map_err(|error| format!("bundle file missing: {}: {error}", path.display()))?
            .len();
        if size == 0 {
            return Err(format!("bundle file is empty: {}", path.display()));
        }
    }
    let manifest: Value = serde_json::from_str(&read(&out.join(MANIFEST_FILE))?)
        .map_err(|error| format!("{MANIFEST_FILE} does not parse: {error}"))?;
    for key in [
        "pinewasm_version",
        "license",
        "runtime_exception",
        "abi_version",
        "module_metadata_schema_version",
        "standalone_wasm_abi_version",
        "runtime_blobs",
        "runtime_closure",
        "elections",
        "corresponding_source",
    ] {
        if manifest.get(key).is_none() {
            return Err(format!("{MANIFEST_FILE} lacks required key {key:?}"));
        }
    }
    if manifest["license"].as_str() != Some(LICENSE_EXPRESSION) {
        return Err(format!(
            "{MANIFEST_FILE} license must be {LICENSE_EXPRESSION}, found {}",
            manifest["license"]
        ));
    }
    // The bundle a distributor conveys must state the model the repository
    // implements, so a stale manifest is rejected here rather than shipped
    // beside an artifact.
    for (label, actual, expected) in [
        (
            "runtime_exception",
            &manifest["runtime_exception"],
            RUNTIME_EXCEPTION_STATUS,
        ),
        (
            "elections.status",
            &manifest["elections"]["status"],
            ELECTION_STATUS,
        ),
    ] {
        if actual.as_str() != Some(expected) {
            return Err(format!(
                "{MANIFEST_FILE} {label} must be {expected:?}, found {actual}"
            ));
        }
    }
    let notice = read(&out.join("NOTICE"))?;
    check_authoritative_notice(&notice)?;
    let election_packages = manifest["elections"]["packages"]
        .as_object()
        .ok_or_else(|| format!("{MANIFEST_FILE} elections.packages must be an object"))?;
    for (package, record) in election_packages {
        let expression = record["expression"].as_str().unwrap_or_default();
        let elected = record["elected"].as_str().unwrap_or_default();
        let expected = elect(expression).ok_or_else(|| {
            format!("{MANIFEST_FILE} has no recorded election for {package} ({expression})")
        })?;
        if elected != expected {
            return Err(format!(
                "{MANIFEST_FILE} election for {package} must be {expected:?}, found {elected:?}"
            ));
        }
        if expression.contains(" OR ")
            && !LICENSE_ELECTIONS
                .iter()
                .any(|(candidate, choice, _)| *candidate == expression && *choice == elected)
        {
            return Err(format!(
                "NOTICE policy source has no election for {package}: {expression} -> {elected}"
            ));
        }
    }
    let digest_re = Regex::new(r"^[0-9a-f]{64}$").expect("valid regex");
    for artifact in AOT_ARTIFACTS {
        let digest = manifest["runtime_blobs"][artifact]
            .as_str()
            .unwrap_or_default();
        if !digest_re.is_match(digest) {
            return Err(format!(
                "{MANIFEST_FILE} runtime_blobs lacks a SHA-256 digest for {artifact}"
            ));
        }
    }
    if manifest["corresponding_source"]["commit"]
        .as_str()
        .is_none_or(|commit| commit.len() != 40)
    {
        return Err(format!(
            "{MANIFEST_FILE} corresponding_source.commit must be a full Git object id"
        ));
    }
    if manifest != trusted.manifest {
        return Err(format!(
            "{MANIFEST_FILE} does not exactly match the trusted manifest reconstructed from the source checkout"
        ));
    }
    let canonical_manifest = serde_json::to_string_pretty(&trusted.manifest)
        .map_err(|error| format!("cannot render trusted {MANIFEST_FILE}: {error}"))?;
    if read(&out.join(MANIFEST_FILE))? != format!("{canonical_manifest}\n") {
        return Err(format!(
            "{MANIFEST_FILE} bytes do not match the canonical trusted rendering"
        ));
    }
    for (file, expected) in [
        ("LICENSE", trusted.license.as_str()),
        ("NOTICE", trusted.notice.as_str()),
        (
            "THIRD_PARTY_LICENSES.txt",
            trusted.runtime_licenses.as_str(),
        ),
    ] {
        if read(&out.join(file))? != expected {
            return Err(format!(
                "bundle file {file} does not exactly match the trusted generated contents"
            ));
        }
    }
    Ok(manifest)
}

fn read(path: &Path) -> TaskResult<String> {
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn write(path: &Path, contents: &str) -> TaskResult {
    // create_new also refuses symlinks/hard links installed after the preflight.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .map_err(|error| format!("cannot create new bundle file {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{sha256, temp_dir, TempDir};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    #[test]
    fn bundle_output_cannot_overwrite_source_or_existing_notices() {
        let temporary = temp_dir("artifact-notice-output-alias").unwrap();
        let root = temporary.path();
        let original_license = read(&repository_root().join("LICENSE")).unwrap();
        let original_notice = read(&repository_root().join("NOTICE")).unwrap();
        fs::write(root.join("LICENSE"), &original_license).unwrap();
        fs::write(root.join("NOTICE"), &original_notice).unwrap();
        let error = write_bundle(root, root, &synthetic_inventory(), &manifest()).unwrap_err();
        assert!(error.contains("output directory is not empty"), "{error}");
        assert_eq!(read(&root.join("LICENSE")).unwrap(), original_license);
        assert_eq!(read(&root.join("NOTICE")).unwrap(), original_notice);
        let out = root.join("bundle");
        fs::create_dir(&out).unwrap();
        fs::hard_link(root.join("NOTICE"), out.join("NOTICE")).unwrap();
        assert!(write_bundle(root, &out, &synthetic_inventory(), &manifest()).is_err());
        assert!(!out.join("LICENSE").exists());
        assert_eq!(read(&root.join("NOTICE")).unwrap(), original_notice);
    }

    #[cfg(unix)]
    #[test]
    fn bundle_output_rejects_symlink_aliases() {
        let temporary = temp_dir("artifact-notice-output-symlink").unwrap();
        let root = temporary.path();
        let original_license = read(&repository_root().join("LICENSE")).unwrap();
        fs::write(root.join("LICENSE"), &original_license).unwrap();
        fs::write(root.join("NOTICE"), "original notice").unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(root, &alias).unwrap();
        assert!(write_bundle(root, &alias, &synthetic_inventory(), &manifest()).is_err());
        assert_eq!(read(&root.join("LICENSE")).unwrap(), original_license);
        let out = root.join("bundle");
        fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(root.join("NOTICE"), out.join("NOTICE")).unwrap();
        assert!(write_bundle(root, &out, &synthetic_inventory(), &manifest()).is_err());
        assert!(!out.join("LICENSE").exists());
        assert_eq!(read(&root.join("NOTICE")).unwrap(), "original notice");
    }

    #[test]
    fn notice_generation_rejects_source_replacement_before_inventory() {
        for ancestor in [false, true] {
            let temporary = temp_dir("artifact-notice-source-override").unwrap();
            let root = temporary.path().join("checkout");
            fs::create_dir_all(root.join(RUNTIME_DIRECTORY)).unwrap();
            fs::write(
                root.join(RUNTIME_MANIFEST),
                "[package]\nname = \"fixture\"\n",
            )
            .unwrap();
            let config_root = if ancestor {
                temporary.path()
            } else {
                root.as_path()
            };
            fs::create_dir_all(config_root.join(".cargo")).unwrap();
            fs::write(
                config_root.join(".cargo/config.toml"),
                "[source.crates-io]\nreplace-with = \"substituted\"\n[source.substituted]\ndirectory = \"vendor\"\n",
            ).unwrap();
            // No Git repository or usable manifest: refuse before inventory runs.
            let out = temporary.path().join("bundle");
            let error = generate(&root, &out).unwrap_err();
            assert!(error.contains("dependency source override"), "{error}");
            assert!(!out.exists());
        }
    }

    #[test]
    fn repository_notice_text_implements_the_notice_model() {
        let root = repository_root();
        check_notice_text(&root).unwrap();
        let notice = read(&root.join("NOTICE")).unwrap();
        check_authoritative_notice(&notice).unwrap();

        let changed_election = notice.replace(
            "- Apache-2.0 OR BSL-1.0 -> Apache-2.0",
            "- Apache-2.0 OR BSL-1.0 -> BSL-1.0",
        );
        let error = check_authoritative_notice(&changed_election).unwrap_err();
        assert!(error.contains("is not byte-exact"), "{error}");

        // These two semantic aliases sit outside the generated block, so the
        // reviewed shell digest must still reject them.
        for addition in [
            "PineWASM hereby grants a separate runtime output exception.",
            "For the Apache-2.0 OR BSL-1.0 family, PineWASM elects BSL-1.0.",
        ] {
            let error = check_authoritative_notice(&format!("{notice}\n{addition}\n")).unwrap_err();
            assert!(error.contains("informational bytes"), "{addition}: {error}");
        }

        let duplicated = format!("{notice}\n{}\n", render_authoritative_policy_block());
        let error = check_authoritative_notice(&duplicated).unwrap_err();
        assert!(error.contains("more than one"), "{error}");

        for (document, addition) in [
            (
                AOT_DOCUMENT,
                "PineWASM hereby grants a separate runtime output exception.",
            ),
            (
                "docs/artifact-distribution.md",
                "For the Apache-2.0 OR BSL-1.0 family, PineWASM elects BSL-1.0.",
            ),
        ] {
            let text = read(&root.join(document)).unwrap();
            let error =
                check_authoritative_policy_document(document, &format!("{text}\n{addition}\n"))
                    .unwrap_err();
            assert!(error.contains("informational bytes"), "{document}: {error}");
        }
    }

    #[test]
    fn trusted_bundle_rejects_changed_project_license_bytes() {
        let root = checkout_with_blobs();
        let license = read(&root.path().join("LICENSE")).unwrap();
        fs::write(
            root.path().join("LICENSE"),
            format!("{license}\nPineWASM hereby grants a separate runtime output exception.\n"),
        )
        .unwrap();
        let error = write_bundle(
            root.path(),
            &root.path().join("bundle"),
            &synthetic_inventory(),
            &manifest(),
        )
        .unwrap_err();
        assert!(error.contains("reviewed AGPL-3.0-only text"), "{error}");
    }

    #[test]
    fn bundle_output_rejects_untrusted_extra_files() {
        let root = checkout_with_blobs();
        let out = root.path().join("bundle");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("UNTRUSTED-EXTRA.txt"), "untrusted").unwrap();
        let error =
            write_bundle(root.path(), &out, &synthetic_inventory(), &manifest()).unwrap_err();
        assert!(error.contains("not empty"), "{error}");
    }

    #[test]
    fn approved_elections_require_exact_notice_prose_and_manifest_values() {
        let notice = read(&repository_root().join("NOTICE")).unwrap();
        check_authoritative_notice(&notice).unwrap();

        let root = checkout_with_blobs();
        fs::write(root.path().join("NOTICE"), &notice).unwrap();
        let out = root.path().join("bundle");
        let trusted = synthetic_trusted_bundle(root.path());
        let mut stale = manifest();
        stale["elections"]["packages"]["ryu@1.0.23"]["elected"] = json!("BSL-1.0");
        write_bundle(root.path(), &out, &synthetic_inventory(), &stale).unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(
            error.contains("ryu@1.0.23 must be \"Apache-2.0\""),
            "{error}"
        );
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crate lives two levels below the repository root")
            .to_path_buf()
    }

    fn package(name: &str, version: &str, expression: &str) -> InventoryPackage {
        InventoryPackage {
            name: name.into(),
            version: version.into(),
            expression: expression.into(),
        }
    }

    fn runtime_closure() -> Vec<InventoryPackage> {
        vec![
            package("itoa", "1.0.18", "MIT OR Apache-2.0"),
            package("libm", "0.2.16", "MIT"),
            package("ryu", "1.0.23", "Apache-2.0 OR BSL-1.0"),
        ]
    }

    fn synthetic_trusted_bundle(root: &Path) -> TrustedBundle {
        trusted_bundle_from(root, &synthetic_inventory(), manifest()).unwrap()
    }

    fn synthetic_inventory() -> Inventory {
        let packages = runtime_closure();
        let mut groups = BTreeMap::new();
        for (label, text) in [
            ("itoa@1.0.18 (MIT OR Apache-2.0); LICENSE-MIT", "MIT text"),
            (
                "ryu@1.0.23 (Apache-2.0 OR BSL-1.0); LICENSE-APACHE",
                "Apache text",
            ),
        ] {
            groups.insert(
                sha256(text.as_bytes()),
                licenses::LicenseText {
                    text: text.into(),
                    uses: BTreeSet::from([format!("Cargo: {label}")]),
                },
            );
        }
        Inventory { packages, groups }
    }

    fn constants() -> ArtifactConstants {
        ArtifactConstants {
            csaw_abi: 6,
            module_metadata_schema: 1,
            standalone_wasm_abi: 1,
        }
    }

    fn blobs() -> BTreeMap<String, String> {
        AOT_ARTIFACTS
            .iter()
            .map(|artifact| ((*artifact).to_string(), sha256(artifact.as_bytes())))
            .collect()
    }

    fn manifest() -> Value {
        render_manifest(&ManifestInputs {
            constants: constants(),
            runtime_blobs: blobs(),
            packages: &runtime_closure(),
            repository: PACKAGE_REPOSITORY,
            commit: COMMIT,
        })
        .unwrap()
    }

    /// A checkout skeleton with the four blobs and a matching `docs/aot.md`
    /// checksum table.
    fn checkout_with_blobs() -> TempDir {
        let temporary = temp_dir("artifact-notice-checkout").unwrap();
        let root = temporary.path();
        fs::create_dir_all(root.join(RUNTIME_DIRECTORY)).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        let mut table =
            String::from("| Runtime artifact | SHA-256 |\n| ---------------- | ------- |\n");
        for artifact in AOT_ARTIFACTS {
            fs::write(root.join(RUNTIME_DIRECTORY).join(artifact), artifact).unwrap();
            table.push_str(&format!(
                "| `{artifact}` | `{}` |\n",
                sha256(artifact.as_bytes())
            ));
        }
        fs::write(root.join(AOT_DOCUMENT), table).unwrap();
        fs::write(
            root.join("LICENSE"),
            read(&repository_root().join("LICENSE")).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("NOTICE"),
            read(&repository_root().join("NOTICE")).unwrap(),
        )
        .unwrap();
        temporary
    }

    #[test]
    fn uncommitted_inputs_cannot_be_labelled_as_corresponding_to_head() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        fs::write(root.join(RUNTIME_MANIFEST), "[package]\n").unwrap();
        git(root, &["init"]).unwrap();
        // Refuse before Cargo metadata (and therefore without network access).
        let error = generate(root, &root.join("bundle")).unwrap_err();
        assert!(
            error.contains("requires a clean source checkout"),
            "{error}"
        );
        assert!(!root.join("bundle").exists());
    }

    #[test]
    fn manifest_pins_identity_status_and_elections() {
        let manifest = manifest();
        assert_eq!(manifest["license"], LICENSE_EXPRESSION);
        assert_eq!(manifest["runtime_exception"], RUNTIME_EXCEPTION_STATUS);
        assert_eq!(manifest["notice_revision"], NOTICE_REVISION);
        assert_eq!(manifest["pinewasm_version"], PACKAGE_VERSION);
        assert_eq!(manifest["abi_version"], 6);
        assert_eq!(manifest["module_metadata_schema_version"], 1);
        assert_eq!(manifest["standalone_wasm_abi_version"], 1);
        assert_eq!(manifest["corresponding_source"]["commit"], COMMIT);
        assert_eq!(
            manifest["corresponding_source"]["repository"],
            PACKAGE_REPOSITORY
        );
        assert_eq!(manifest["runtime_blobs"], json!(blobs()));
        assert_eq!(
            manifest["runtime_closure"],
            json!([
                "itoa@1.0.18 (MIT OR Apache-2.0)",
                "libm@0.2.16 (MIT)",
                "ryu@1.0.23 (Apache-2.0 OR BSL-1.0)"
            ])
        );
        let elections = &manifest["elections"];
        assert_eq!(elections["status"], ELECTION_STATUS);
        assert_eq!(elections["packages"]["itoa@1.0.18"]["elected"], "MIT");
        assert_eq!(elections["packages"]["libm@0.2.16"]["elected"], "MIT");
        assert_eq!(elections["packages"]["ryu@1.0.23"]["elected"], "Apache-2.0");
        assert_eq!(manifest["bundle"], json!(BUNDLE_FILES));
    }

    #[test]
    fn unrecorded_multi_license_expression_fails_closed() {
        let packages = vec![package("novel", "0.1.0", "MIT OR Zlib")];
        let error = render_manifest(&ManifestInputs {
            constants: constants(),
            runtime_blobs: blobs(),
            packages: &packages,
            repository: PACKAGE_REPOSITORY,
            commit: COMMIT,
        })
        .unwrap_err();
        assert!(
            error.contains("no recorded license election for novel (MIT OR Zlib)"),
            "{error}"
        );
        assert_eq!(elect("MIT/Apache-2.0").as_deref(), Some("MIT"));
        assert_eq!(elect("Unlicense OR MIT").as_deref(), Some("MIT"));
        assert_eq!(elect("Unicode-3.0").as_deref(), Some("Unicode-3.0"));
        assert_eq!(
            elect("(MIT OR Apache-2.0) AND Unicode-3.0").as_deref(),
            Some("MIT AND Unicode-3.0")
        );
        assert_eq!(elect("(MIT OR Zlib) AND Unicode-3.0"), None);
    }

    #[test]
    fn runtime_checksums_require_the_documented_pins() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let checksums = runtime_checksums(root).unwrap();
        assert_eq!(checksums, blobs());

        fs::write(
            root.join(RUNTIME_DIRECTORY).join(AOT_ARTIFACTS[1]),
            "rebuilt bytes",
        )
        .unwrap();
        let error = runtime_checksums(root).unwrap_err();
        assert!(
            error.contains("pinewasm_aot_runtime_standalone_wasm.wasm does not match"),
            "{error}"
        );

        fs::write(root.join(AOT_DOCUMENT), "no table\n").unwrap();
        let error = runtime_checksums(root).unwrap_err();
        assert!(error.contains("has no checksum row for"), "{error}");
    }

    #[test]
    fn bundle_contains_every_expected_file_and_verifies() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let out = root.join("bundle");
        let trusted = synthetic_trusted_bundle(root);
        write_bundle(root, &out, &synthetic_inventory(), &manifest()).unwrap();
        let mut listed = fs::read_dir(&out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        listed.sort();
        let mut expected = BUNDLE_FILES.map(String::from).to_vec();
        expected.sort();
        assert_eq!(listed, expected);
        assert_eq!(
            fs::read_to_string(out.join("LICENSE")).unwrap(),
            read(&repository_root().join("LICENSE")).unwrap()
        );
        let licenses = fs::read_to_string(out.join("THIRD_PARTY_LICENSES.txt")).unwrap();
        assert!(
            licenses.contains("Runtime closure: 3 production Cargo packages."),
            "{licenses}"
        );
        assert!(
            licenses.contains("  - ryu@1.0.23 (Apache-2.0 OR BSL-1.0)"),
            "{licenses}"
        );
        assert!(licenses.contains("--- License text 1 ---\nApplies to:\n  - Cargo: itoa@1.0.18 (MIT OR Apache-2.0); LICENSE-MIT\n\nMIT text"), "{licenses}");
        let parsed = verify_bundle(&out, &trusted).unwrap();
        assert_eq!(parsed["abi_version"], 6);

        fs::remove_file(out.join("NOTICE")).unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(error.contains("file set does not exactly match"), "{error}");
    }

    #[test]
    fn verify_bundle_rejects_incomplete_manifests() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let out = root.join("bundle");
        let trusted = synthetic_trusted_bundle(root);
        let mut incomplete = manifest();
        incomplete["runtime_blobs"][AOT_ARTIFACTS[0]] = json!("not a digest");
        write_bundle(root, &out, &synthetic_inventory(), &incomplete).unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(
            error.contains("lacks a SHA-256 digest for pinewasm_aot_runtime.wasm"),
            "{error}"
        );

        fs::remove_dir_all(&out).unwrap();
        let mut relicensed = manifest();
        relicensed["license"] = json!("MIT");
        write_bundle(root, &out, &synthetic_inventory(), &relicensed).unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(error.contains("license must be AGPL-3.0-only"), "{error}");

        fs::remove_dir_all(&out).unwrap();
        let mut stale = manifest();
        stale["runtime_exception"] = json!("pending");
        write_bundle(root, &out, &synthetic_inventory(), &stale).unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(
            error.contains("runtime_exception must be \"none\""),
            "{error}"
        );

        fs::write(out.join(MANIFEST_FILE), "{").unwrap();
        let error = verify_bundle(&out, &trusted).unwrap_err();
        assert!(error.contains("does not parse"), "{error}");
    }

    #[test]
    fn verify_bundle_rejects_valid_shape_identity_omissions_and_substitutions() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let trusted = synthetic_trusted_bundle(root);
        let reject_manifest = |name: &str, changed: &Value| {
            let out = root.join(name);
            write_bundle(root, &out, &synthetic_inventory(), changed).unwrap();
            let error = verify_bundle(&out, &trusted).unwrap_err();
            assert!(error.contains("does not exactly match"), "{name}: {error}");
        };

        let mut missing_election = manifest();
        missing_election["elections"]["packages"]
            .as_object_mut()
            .unwrap()
            .remove("ryu@1.0.23");
        reject_manifest("missing-election", &missing_election);

        let mut missing_closure_package = manifest();
        missing_closure_package["runtime_closure"]
            .as_array_mut()
            .unwrap()
            .remove(2);
        reject_manifest("missing-closure-package", &missing_closure_package);

        let mut substituted_digest = manifest();
        substituted_digest["runtime_blobs"][AOT_ARTIFACTS[0]] = json!("0".repeat(64));
        reject_manifest("substituted-digest", &substituted_digest);

        let mut changed_decisions = manifest();
        changed_decisions["notice_revision"] = json!("artifact-notice-other");
        changed_decisions["elections"]["notice_revision"] = json!("artifact-notice-other");
        reject_manifest("changed-decisions", &changed_decisions);

        let mut changed_source = manifest();
        changed_source["corresponding_source"]["repository"] =
            json!("https://example.invalid/substitute");
        changed_source["corresponding_source"]["commit"] = json!("f".repeat(40));
        reject_manifest("changed-source", &changed_source);

        for (name, key, value) in [
            ("changed-schema", "manifest_schema_version", json!(2)),
            ("changed-generator", "generator", json!("other-generator")),
            (
                "changed-bundle-list",
                "bundle",
                json!(["artifact-manifest.json"]),
            ),
        ] {
            let mut changed = manifest();
            changed[key] = value;
            reject_manifest(name, &changed);
        }
    }

    #[test]
    fn verify_bundle_rejects_altered_legal_and_runtime_license_bytes() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let trusted = synthetic_trusted_bundle(root);

        for (directory, file, contents, expected) in [
            (
                "altered-license",
                "LICENSE",
                format!("{}altered\n", trusted.license),
                "does not exactly match the trusted generated contents",
            ),
            (
                "altered-notice",
                "NOTICE",
                format!("{}altered\n", trusted.notice),
                "informational bytes do not match the reviewed template",
            ),
            (
                "altered-runtime-licenses",
                "THIRD_PARTY_LICENSES.txt",
                format!("{}altered\n", trusted.runtime_licenses),
                "does not exactly match the trusted generated contents",
            ),
        ] {
            let out = root.join(directory);
            write_bundle(root, &out, &synthetic_inventory(), &manifest()).unwrap();
            fs::write(out.join(file), contents).unwrap();
            let error = verify_bundle(&out, &trusted).unwrap_err();
            assert!(error.contains(expected), "{directory}: {error}");
        }
    }

    #[test]
    fn verify_bundle_rejects_policy_invalid_trusted_notice_bytes() {
        let temporary = checkout_with_blobs();
        let root = temporary.path();
        let notice = read(&repository_root().join("NOTICE")).unwrap();

        for (directory, notice, expected) in [
            (
                "missing-notice-model",
                notice.replace(POLICY_BLOCK_BEGIN, "<!-- POLICY REMOVED -->"),
                "lacks the generated artifact-policy block",
            ),
            (
                "contradictory-notice-model",
                format!("{notice}\nPineWASM hereby grants a separate runtime output exception.\n"),
                "informational bytes do not match",
            ),
            (
                "contradictory-dependency-election",
                notice.replace(
                    "- Apache-2.0 OR BSL-1.0 -> Apache-2.0",
                    "- Apache-2.0 OR BSL-1.0 -> BSL-1.0",
                ),
                "is not byte-exact",
            ),
        ] {
            let out = root.join(directory);
            let mut trusted = synthetic_trusted_bundle(root);
            trusted.notice = notice;
            write_trusted_bundle(&out, &trusted).unwrap();
            let error = verify_bundle(&out, &trusted).unwrap_err();
            assert!(error.contains(expected), "{directory}: {error}");
        }
    }

    #[test]
    fn constants_are_parsed_from_source_and_must_agree() {
        let library = "pub mod header {\n    pub const ABI_VERSION: u32 = 6;\n}\n";
        let schema = concat!(
            "let mut out = String::from(\"{\\\"schema_version\\\":1\");\n",
            "let mut out = String::from(\"{\\\"schema_version\\\":1\");\n",
            "out.push_str(\",\\\"abi_version\\\":1\");\n",
        );
        assert_eq!(
            constants_from_sources(library, schema).unwrap(),
            constants()
        );

        let drifted = format!("{schema}let x = \"{{\\\"schema_version\\\":2\";\n");
        let error = constants_from_sources(library, &drifted).unwrap_err();
        assert!(error.contains("conflicting values"), "{error}");
        let error = constants_from_sources("", schema).unwrap_err();
        assert!(error.contains("header::ABI_VERSION not found"), "{error}");

        // The real sources pin the values documented in docs/aot.md.
        assert_eq!(artifact_constants(&repository_root()).unwrap(), constants());
    }

    #[test]
    fn tracked_runtime_blobs_match_their_documented_pins() {
        let root = repository_root();
        let checksums = runtime_checksums(&root).unwrap();
        let mut expected = AOT_ARTIFACTS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            checksums.keys().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
    }
}
