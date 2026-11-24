//! Maintainer tool: native-coverage census. For every compiler-supported
//! fixture, record the first native blocker (if any) and print the aggregate.
//!
//! Run explicitly from the repository root when planning native-coverage work:
//!
//! ```text
//! cargo test -p pinewasm_aot --test census -- --nocapture
//! ```
//!
//! The final assertion pins the exact repository-fixture boundary. A deliberate
//! fixture or lowering change must update the assertion.
//!
//! It also counts how many native programs link the ta engine. That number
//! distinguishes self-contained modules from modules that still rely on the
//! interpreter host. Keeping the census in-tree makes coverage statements
//! reproducible from the repository's own Pine fixtures.

use pinewasm_aot::{audit_standalone, compile_program};
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
fn native_blocker_census() {
    let mut files = Vec::new();
    collect_pine_files(Path::new("../../tests/compile-pass"), &mut files);
    files.sort();

    let mut native = 0usize;
    let mut links_ta = 0usize;
    let mut imports_vm = 0usize;
    let mut standalone = 0usize;
    let mut blocked: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match compile_program(&program) {
            Ok(module) => {
                native += 1;
                links_ta += usize::from(module.layout.links_ta);
                imports_vm += usize::from(module.layout.imports_vm);
                // The gate's own definition of portable, over real bytes, so
                // this figure cannot drift from what the gate would allow.
                if audit_standalone(&module.wasm).is_ok() {
                    standalone += 1;
                }
            }
            Err(error) => blocked.entry(error.to_string()).or_default().push(name),
        }
    }

    let hosted: usize = blocked.values().map(Vec::len).sum();
    eprintln!("[census] native={native} hosted={hosted}");
    eprintln!(
        "[census] of the native ones: links_ta={links_ta} imports_vm={imports_vm} \
         standalone={standalone}"
    );
    let mut counts: Vec<(usize, &String, &Vec<String>)> = blocked
        .iter()
        .map(|(reason, files)| (files.len(), reason, files))
        .collect();
    counts.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (count, reason, files) in counts {
        let sample: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        eprintln!("[census] {count:>4}  {reason}  e.g. {sample:?}");
    }
    let identity_blocker =
        pinewasm_aot::AotError::UnsupportedOpcode("color input provenance".into()).to_string();
    assert_eq!(
        blocked[&identity_blocker],
        vec!["inputs.pine".to_string()],
        "review every fixture moved by the color identity gate"
    );
    let table_replacement_blocker = pinewasm_aot::AotError::UnsupportedOpcode(
        "table position replacement requires interpreter".into(),
    )
    .to_string();
    assert_eq!(
        blocked[&table_replacement_blocker],
        vec!["drawings.pine".to_string()],
        "same-position table replacement must stay on the interpreter-hosted tier"
    );
    assert_eq!(
        (native, hosted, standalone, imports_vm, links_ta),
        (48, 3, 31, 17, 6),
        "AOT census changed; review the boundary, update this assertion, \
         and reassess the AOT description"
    );
}
