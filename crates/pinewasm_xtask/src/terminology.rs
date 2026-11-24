//! Keep execution names descriptive across source, paths, and documentation.

use crate::command::{git, repo_root};
use crate::TaskResult;
use regex::Regex;
use std::fs;

fn retired_name() -> Regex {
    // Build the pattern from parts so this file's own text does not match it.
    let upper = ["A", "2"].concat();
    let lower = upper.to_ascii_lowercase();
    Regex::new(&format!(
        r"\b{upper}\b|\b{lower}\b|\b{lower}_|_{lower}\b|_{lower}_|{upper}[A-Z][a-z]|[a-z]{upper}\b|\b{lower}[A-Z]|\b{upper}_|_{upper}\b|PW{upper}"
    ))
    .expect("valid terminology expression")
}

pub(crate) fn check(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("usage: terminology-check".into());
    }
    let root = repo_root()?;
    let pattern = retired_name();
    let mut failures = Vec::new();
    let paths = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    for path in paths.split('\0').filter(|path| !path.is_empty()) {
        let absolute = root.join(path);
        if !absolute.is_file() {
            continue;
        }
        if pattern.is_match(path) {
            failures.push(path.to_owned());
        }
        let bytes = fs::read(&absolute).map_err(|error| format!("{path}: {error}"))?;
        // Binary instructions, hashes and image payloads are not prose. Runtime
        // sources and generated textual bindings remain subject to this gate.
        if bytes.contains(&0) {
            continue;
        }
        if let Ok(source) = std::str::from_utf8(&bytes) {
            for (line, text) in source.lines().enumerate() {
                if pattern.is_match(text) {
                    failures.push(format!("{path}:{}", line + 1));
                }
            }
        }
    }
    if failures.is_empty() {
        println!("Descriptive WebAssembly terminology verified.");
        Ok(())
    } else {
        Err(format!(
            "disallowed compiler terminology in:\n{}",
            failures.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::retired_name;

    #[test]
    fn rejects_retired_names_across_interfaces_and_paths() {
        let label = ["A", "2"].concat();
        let lower = label.to_ascii_lowercase();
        for text in [
            label.clone(),
            lower.clone(),
            format!("compile{label}Program"),
            format!("compile_{lower}_program"),
            format!("{lower}Options"),
            format!("pinewasm-{lower}"),
            format!("runtime_{lower}.wasm"),
            format!("PW{label}BIN1"),
            format!("{label}_OPTIONS"),
        ] {
            assert!(retired_name().is_match(&text), "missed {text}");
        }
        for text in [
            "sha256",
            "Sha256",
            "sma20",
            "0xa2bfe8a1",
            "StandaloneWasmOptions",
            "PWSWBIN1",
        ] {
            assert!(!retired_name().is_match(text), "false positive: {text}");
        }
    }
}
