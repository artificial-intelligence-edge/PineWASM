//! Maintainer tool: standalone WebAssembly-eligibility census. For every compiler-supported public
//! fixture, attempt a strategy-type standalone WebAssembly emit with the default signal designation
//! and separately an indicator-type emit. It prints aggregates and refusal
//! reasons with examples.
//!
//! Run explicitly from the repository root when planning standalone WebAssembly host coverage:
//!
//! ```text
//! cargo test -p pinewasm_aot --test standalone_wasm_census -- --nocapture
//! ```
//!
//! Two assertions guard the boundary. No refusal of a fixture may be of the
//! internal class ([`RefusalClass::Internal`]): a fixture is front-end output,
//! so such a refusal is a compiler defect, not a limit. And the count of
//! refusals per kind is pinned, so a change in what is refused is reviewed
//! instead of passing silently.
//!
//! The final assertion pins the exact repository-fixture boundary. A deliberate
//! fixture or eligibility change must update the assertion and reassess the
//! public description of the census.
//!
//! Why it exists beside `census.rs`: the native census counts what *lowers*;
//! this one counts what a standalone WebAssembly-compatible host would accept. The gap between the
//! two is dominated by signal designation — a program refused only for
//! "multiple plots, no designated signal" becomes eligible the moment a host
//! lets the user pick the signal plot, because `compile_standalone_wasm_program` checks
//! standalone eligibility before designation. Planning against the native
//! number overstates default eligibility; planning against the
//! default-designation number understates what a host-provided designation
//! unlocks.

use pinewasm_aot::{
    audit_standalone_wasm, compile_standalone_wasm_program, AotError, RefusalClass,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn collect_pine_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable fixture directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "generated_failures")
            {
                continue;
            }
            collect_pine_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "pine")
        {
            out.push(path);
        }
    }
}

#[test]
fn standalone_wasm_eligibility_census() {
    let mut files = Vec::new();
    collect_pine_files(Path::new("../../tests/compile-pass"), &mut files);
    files.sort();

    let mut supported = 0usize;
    let mut strategy_ok = 0usize;
    let mut indicator_ok = 0usize;
    let mut refusals: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut by_kind: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut internal: Vec<String> = Vec::new();
    let mut record = |mode: &'static str, name: &str, error: &AotError| {
        *by_kind.entry((mode, error.kind())).or_default() += 1;
        if error.kind() == "UnsupportedOpcode" {
            // These two fixtures intentionally stay off the standalone tier:
            // input-color provenance is unknown at compile time, and a
            // same-position table replacement requires interpreter alias state.
            let expected_reason = match name {
                "inputs.pine" => "color input provenance",
                "drawings.pine" => "table position replacement requires interpreter",
                _ => panic!("unexpected standalone-Wasm refusal: {name}: {error}"),
            };
            assert_eq!(error, &AotError::UnsupportedOpcode(expected_reason.into()));
        }
        if error.class() == RefusalClass::Internal {
            internal.push(format!("{name} ({mode}): {error}"));
        }
    };

    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let result = compile(&source, CompileOptions::default());
        let Some(program) = result.program else {
            continue;
        };
        if result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error)
        {
            continue;
        }
        supported += 1;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let strategy = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Strategy,
            },
        );
        match strategy {
            Ok(wasm) => {
                // The producer audit skips re-validating verified runtime
                // copies; the complete public audit must reach the same
                // verdict on every artifact it accepted.
                audit_standalone_wasm(&wasm)
                    .unwrap_or_else(|error| panic!("{name}: public audit disagrees: {error}"));
                strategy_ok += 1;
            }
            Err(error) => {
                record("strategy", &name, &error);
                refusals
                    .entry(error.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }

        // A strategy() source can never be an indicator-type module — the
        // module type is forced to `strategy` — so counting its
        // successful emit here would quietly inflate the indicator column
        // with strategy modules.
        if program.metadata.strategy.is_none() {
            match compile_standalone_wasm_program(
                &program,
                &StandaloneWasmOptions {
                    signal_plot: None,
                    module_type: StandaloneWasmModuleType::Indicator,
                },
            ) {
                Ok(wasm) => {
                    audit_standalone_wasm(&wasm).unwrap_or_else(|error| {
                        panic!("{}: public audit disagrees: {error}", path.display())
                    });
                    indicator_ok += 1;
                }
                Err(error) => record("indicator", &name, &error),
            }
        }
    }

    eprintln!(
        "[standalone-wasm-census] compiler-supported={supported} strategy-standalone-wasm-ok={strategy_ok} \
         indicator-standalone-wasm-ok={indicator_ok}"
    );
    let mut counts: Vec<(usize, &String, &Vec<String>)> = refusals
        .iter()
        .map(|(reason, files)| (files.len(), reason, files))
        .collect();
    counts.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (count, reason, files) in counts {
        let sample: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        eprintln!("[standalone-wasm-census] {count:>4}  {reason}  e.g. {sample:?}");
    }
    for ((mode, kind), count) in &by_kind {
        eprintln!("[standalone-wasm-census] kind {mode:<9} {kind:<36} {count:>3}");
    }
    assert!(
        internal.is_empty(),
        "the backend refused front-end output for an internal reason; this is a compiler \
         defect, not a capability limit:\n{}",
        internal.join("\n")
    );
    let pinned: BTreeMap<(&'static str, &'static str), usize> = PINNED_REFUSALS
        .iter()
        .map(|&(mode, kind, count)| ((mode, kind), count))
        .collect();
    assert_eq!(
        by_kind, pinned,
        "the set of refused fixtures changed; review each difference, then update PINNED_REFUSALS"
    );
    assert_eq!(
        (supported, strategy_ok, indicator_ok),
        (51, 12, 31),
        "public standalone WebAssembly census changed; review the boundary, update this assertion, \
         and reassess the public AOT description"
    );
}

/// Refusals per build mode and kind over the public fixtures. Every entry is a
/// capability refusal; the census fails before this comparison if any is not.
const PINNED_REFUSALS: &[(&str, &str, usize)] = &[
    ("indicator", "StandaloneWasmNotStandalone", 15),
    ("indicator", "UnsupportedOpcode", 2),
    ("strategy", "StandaloneWasmNoDesignatedPlot", 22),
    ("strategy", "StandaloneWasmNotStandalone", 15),
    ("strategy", "UnsupportedOpcode", 2),
];
