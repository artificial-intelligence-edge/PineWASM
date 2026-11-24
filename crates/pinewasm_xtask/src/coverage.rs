use crate::command::{repo_root, sha256_file};
use crate::TaskResult;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const STATUSES: [&str; 4] = ["unsupported", "partial", "supported", "verified"];
pub(crate) const AOT_ARTIFACTS: [&str; 4] = [
    "pinewasm_aot_runtime.wasm",
    "pinewasm_aot_runtime_standalone_wasm.wasm",
    "pinewasm_aot_runtime_broker.wasm",
    "pinewasm_aot_runtime_ta.wasm",
];

type Counts = BTreeMap<String, usize>;

pub(crate) fn check() -> TaskResult {
    let root = repo_root()?;
    let matrix = root.join("docs/pine-v6-coverage.md");
    let readme = root.join("README.md");
    let language_doc = root.join("docs/language-support.md");
    let codes_file = root.join("crates/pinewasm_span/src/codes.rs");
    let aot_doc = root.join("docs/aot.md");
    let abi_doc = root.join("docs/spec/csaw-module-abi.md");
    let runtime_doc = root.join("docs/runtime.md");
    let vm_source = root.join("crates/pinewasm_vm/src/lib.rs");
    let emit_source = root.join("crates/pinewasm_aot/src/emit.rs");
    let runtime_directory = root.join("crates/pinewasm_aot/runtime");

    let source = read(&matrix)?;
    let lines = source.lines().collect::<Vec<_>>();
    let section_re = Regex::new(r"^#{2,3}\s+(\d+(?:\.\d+)?)\.?\s+(.+)$").expect("valid regex");
    let summary_re = Regex::new(r"^\|\s*(\d+(?:\.\d+)?)\.?\s+(.+?)\s*\|\s*(\d+)\s*\|\s*(\d+)\s*\|\s*(\d+)\s*\|\s*(\d+)\s*\|\s*(\d+)\s*\|$").expect("valid regex");
    let total_re = Regex::new(r"^\|\s*\*\*Total\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|$").expect("valid regex");
    let link_re = Regex::new(r"\[(`?)([^\]]+?)`?\]\(([^)#]+)(?:#[^)]+)?\)").expect("valid regex");
    let test_name_re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("valid regex");
    let mut errors = Vec::new();
    let mut current_section = String::new();
    let mut rows = Vec::new();
    let mut summaries: BTreeMap<String, (Counts, usize, usize)> = BTreeMap::new();
    let mut declared_total = None;
    let mut linked_tests = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let line_number = index + 1;
        if let Some(captures) = section_re.captures(line) {
            current_section = captures[1].to_string();
        }
        if let Some(captures) = summary_re.captures(line) {
            summaries.insert(
                captures[1].to_string(),
                (
                    counts_from(&captures, 3),
                    captures[7].parse().unwrap_or(0),
                    line_number,
                ),
            );
        } else if let Some(captures) = total_re.captures(line) {
            declared_total = Some((
                counts_from(&captures, 1),
                captures[5].parse().unwrap_or(0),
                line_number,
            ));
        }
        if !line.starts_with('|') {
            continue;
        }
        let cells = split_row(line);
        if cells.len() != 4 {
            continue;
        }
        let status = cells[1].trim_matches(['`', '*']).to_ascii_lowercase();
        if !STATUSES.contains(&status.as_str()) {
            continue;
        }
        rows.push((current_section.clone(), status.clone()));
        let mut row_tests = 0usize;
        for (cell_name, cell) in [("Tests", &cells[2]), ("Notes", &cells[3])] {
            for captures in link_re.captures_iter(cell) {
                let label = captures
                    .get(2)
                    .map_or("", |value| value.as_str())
                    .trim_matches('`');
                let target = captures.get(3).map_or("", |value| value.as_str());
                if target.starts_with("http://") || target.starts_with("https://") {
                    continue;
                }
                let candidate = matrix.parent().unwrap_or(&root).join(target);
                let resolved = match candidate.canonicalize() {
                    Ok(path) if path.starts_with(&root) => path,
                    Ok(_) => {
                        errors.push(format!("docs/pine-v6-coverage.md:{line_number}: {cell_name} link escapes the repository: {target}"));
                        continue;
                    }
                    Err(_) => {
                        errors.push(format!("docs/pine-v6-coverage.md:{line_number}: {cell_name} link is not a file: {target}"));
                        continue;
                    }
                };
                if resolved.extension().and_then(|value| value.to_str()) == Some("rs")
                    && test_name_re.is_match(label)
                {
                    if is_rust_test(&read(&resolved)?, label) {
                        linked_tests += 1;
                        if cell_name == "Tests" {
                            row_tests += 1;
                        }
                    } else {
                        errors.push(format!("docs/pine-v6-coverage.md:{line_number}: linked Rust symbol is not a test: {label} in {target}"));
                    }
                }
            }
        }
        if matches!(status.as_str(), "supported" | "verified") {
            if !link_re.is_match(&cells[2]) {
                errors.push(format!(
                    "docs/pine-v6-coverage.md:{line_number}: {status} row has no linked evidence"
                ));
            }
            if row_tests == 0 {
                errors.push(format!("docs/pine-v6-coverage.md:{line_number}: {status} row has no linked local Rust test declaration"));
            }
        }
    }
    if rows.is_empty() {
        errors.push("coverage matrix contains no feature rows".into());
    }

    let mut actual_sections: BTreeMap<String, Counts> = BTreeMap::new();
    let mut actual_total = blank_counts();
    for (section, status) in &rows {
        *actual_sections
            .entry(section.clone())
            .or_insert_with(blank_counts)
            .entry(status.clone())
            .or_default() += 1;
        *actual_total.entry(status.clone()).or_default() += 1;
    }
    for (section, counts) in &actual_sections {
        match summaries.get(section) {
            Some((declared, total, _line))
                if declared == counts && *total == counts.values().sum::<usize>() => {}
            Some((declared, total, line)) => errors.push(format!("docs/pine-v6-coverage.md:{line}: summary for {section} is stale; declared {declared:?} total={total}, actual {counts:?} total={}", counts.values().sum::<usize>())),
            None => errors.push(format!("coverage summary is missing section {section}")),
        }
    }
    for section in summaries.keys() {
        if !actual_sections.contains_key(section) {
            errors.push(format!(
                "coverage summary has section {section}, but no feature table exists"
            ));
        }
    }
    match declared_total {
        Some((counts, total, _line)) if counts == actual_total && total == rows.len() => {}
        Some((counts, total, line)) => errors.push(format!("docs/pine-v6-coverage.md:{line}: total is stale; declared {counts:?} total={total}, actual {actual_total:?} total={}", rows.len())),
        None => errors.push("coverage summary has no total row".into()),
    }

    validate_readme_counts(&read(&readme)?, &actual_total, rows.len(), &mut errors);
    validate_budgets(
        [
            (
                "VM default",
                &vm_source,
                r"(?m)^pub const DEFAULT_MAX_INSTRUCTIONS_PER_BAR: u64 = ([0-9_]+);$",
            ),
            (
                "AOT default",
                &emit_source,
                r"(?m)^const DEFAULT_BUDGET: i64 = ([0-9_]+);$",
            ),
            (
                "ABI table",
                &abi_doc,
                r"(?m)^\| `0x30` .*?\| `budget_per_bar` \|.*?default ([0-9,]+)\s+\|$",
            ),
            (
                "ABI prose",
                &abi_doc,
                r"The defaults are ([0-9,]+) instructions per bar",
            ),
            (
                "runtime documentation",
                &runtime_doc,
                r"(?m)^\| `max_instructions_per_bar` \| ([0-9,]+)\s+\|",
            ),
            (
                "coverage matrix",
                &matrix,
                r"(?m)^\| Resource limits .*?defaults ([0-9,]+) instructions/bar",
            ),
        ],
        &root,
        &mut errors,
    )?;
    let diagnostic_count = validate_diagnostics(&root, &codes_file, &language_doc, &mut errors)?;
    validate_aot_hashes(&aot_doc, &runtime_directory, &mut errors)?;

    if !errors.is_empty() {
        for error in &errors {
            eprintln!("ERROR: {error}");
        }
        return Err(format!(
            "coverage metadata check failed with {} error(s)",
            errors.len()
        ));
    }
    println!(
        "Coverage metadata check: {}; total={}; {linked_tests} linked Rust test declarations, README counts, {diagnostic_count} diagnostic codes, and {} AOT checksums agree mechanically; VM, AOT, ABI, runtime, and coverage budget defaults agree. This check does not assess whether evidence proves each feature claim.",
        STATUSES
            .iter()
            .map(|status| format!("{status}={}", actual_total[*status]))
            .collect::<Vec<_>>()
            .join(", "),
        rows.len(),
        AOT_ARTIFACTS.len()
    );
    Ok(())
}

fn split_row(line: &str) -> Vec<String> {
    let body = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut code = false;
    let mut escaped = false;
    for character in body.chars() {
        if escaped {
            if character != '|' {
                current.push('\\');
            }
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '`' {
            code = !code;
            current.push(character);
        } else if character == '|' && !code {
            cells.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    cells.push(current.trim().to_string());
    cells
}

fn blank_counts() -> Counts {
    STATUSES
        .into_iter()
        .map(|status| (status.to_string(), 0))
        .collect()
}

fn counts_from(captures: &regex::Captures<'_>, start: usize) -> Counts {
    STATUSES
        .iter()
        .enumerate()
        .map(|(offset, status)| {
            (
                (*status).to_string(),
                captures[start + offset].parse().unwrap_or(0),
            )
        })
        .collect()
}

fn read(path: &Path) -> TaskResult<String> {
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn is_rust_test(source: &str, name: &str) -> bool {
    let declaration =
        Regex::new(&format!(r"\bfn\s+{}\s*\(", regex::escape(name))).expect("valid regex");
    let test_attribute =
        Regex::new(r"#\[(?:[A-Za-z_][A-Za-z0-9_]*::)*test(?:\([^]]*\))?\]").expect("valid regex");
    let lines = source.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if !declaration.is_match(line) {
            continue;
        }
        let mut cursor = index;
        while cursor > 0 {
            cursor -= 1;
            let prefix = lines[cursor].trim();
            if prefix.is_empty() || prefix.starts_with("//") {
                continue;
            }
            if prefix.starts_with("#[") {
                if test_attribute.is_match(prefix) {
                    return true;
                }
                continue;
            }
            break;
        }
    }
    false
}

fn validate_readme_counts(
    source: &str,
    expected: &Counts,
    expected_total: usize,
    errors: &mut Vec<String>,
) {
    let count_re = Regex::new(
        r"^\|\s*`(unsupported|partial|supported|verified)`\s*\|.*\|\s*\*\*(\d+)\*\*\s*\|$",
    )
    .expect("valid regex");
    let total_re =
        Regex::new(r"^\|\s*\|\s*\*\*Total enumerated rows\*\*\s*\|\s*\*\*(\d+)\*\*\s*\|$")
            .expect("valid regex");
    let mut actual = blank_counts();
    let mut total = None;
    for line in source.lines() {
        if let Some(captures) = count_re.captures(line) {
            actual.insert(captures[1].to_string(), captures[2].parse().unwrap_or(0));
        }
        if let Some(captures) = total_re.captures(line) {
            total = captures[1].parse().ok();
        }
    }
    if &actual != expected || total != Some(expected_total) {
        errors.push(format!("README coverage table is stale; found {actual:?} total={total:?}, expected {expected:?} total={expected_total}"));
    }
}

fn validate_budgets<const N: usize>(
    authorities: [(&str, &PathBuf, &str); N],
    root: &Path,
    errors: &mut Vec<String>,
) -> TaskResult {
    let mut values = BTreeMap::new();
    for (label, path, pattern) in authorities {
        let regex =
            Regex::new(pattern).map_err(|error| format!("invalid {label} regex: {error}"))?;
        let source = read(path)?;
        let matches = regex.captures_iter(&source).collect::<Vec<_>>();
        if matches.len() != 1 {
            errors.push(format!(
                "{}: expected exactly one {label}, found {}",
                path.strip_prefix(root).unwrap_or(path).display(),
                matches.len()
            ));
            continue;
        }
        let value = matches[0][1]
            .replace(['_', ','], "")
            .parse::<u64>()
            .map_err(|error| format!("cannot parse {label}: {error}"))?;
        values.insert(label, value);
    }
    if values.values().copied().collect::<BTreeSet<_>>().len() > 1 {
        errors.push(format!(
            "per-bar instruction-budget defaults disagree: {values:?}"
        ));
    }
    Ok(())
}

fn validate_diagnostics(
    root: &Path,
    codes_file: &Path,
    language_doc: &Path,
    errors: &mut Vec<String>,
) -> TaskResult<usize> {
    let constant_re =
        Regex::new(r#"(?m)^pub const ([A-Z]+\d+): &str = "([A-Z]+\d+)";$"#).expect("valid regex");
    let table_re =
        Regex::new(r"(?m)^\|\s*([A-Z]+\d+)\s*\|\s*(?:error|warning)\s*\|").expect("valid regex");
    let constants_source = read(codes_file)?;
    let code_pairs = constant_re
        .captures_iter(&constants_source)
        .map(|capture| (capture[1].to_string(), capture[2].to_string()))
        .collect::<Vec<_>>();
    for (name, value) in &code_pairs {
        if name != value {
            errors.push(format!(
                "diagnostic constant name {name} disagrees with wire value {value}"
            ));
        }
    }
    let code_names = code_pairs
        .iter()
        .map(|pair| pair.0.clone())
        .collect::<Vec<_>>();
    let documented = table_re
        .captures_iter(&read(language_doc)?)
        .map(|capture| capture[1].to_string())
        .collect::<Vec<_>>();
    validate_unique("diagnostic registry", &code_names, errors);
    validate_unique("diagnostic table", &documented, errors);
    let code_set = code_names.iter().cloned().collect::<BTreeSet<_>>();
    let documented_set = documented.iter().cloned().collect::<BTreeSet<_>>();
    if code_set != documented_set {
        errors.push(format!("diagnostic table and code registry disagree; missing from docs={:?}, absent from registry={:?}", code_set.difference(&documented_set).collect::<Vec<_>>(), documented_set.difference(&code_set).collect::<Vec<_>>()));
    }

    let literal_re = Regex::new(r##"(?:b|r|br|rb)?#{0,16}"((?:CE|CW|E|W)\d{4,5})"#{0,16}"##)
        .expect("valid regex");
    let inline_tests = Regex::new(r"(?m)^#\[cfg\(test\)\]\s*\nmod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")
        .expect("valid regex");
    for path in rust_sources(&root.join("crates"))? {
        if path == codes_file
            || path.file_name().and_then(|value| value.to_str()) == Some("tests.rs")
            || path.components().any(|part| part.as_os_str() == "tests")
        {
            continue;
        }
        let source = read(&path)?;
        let production = inline_tests
            .find(&source)
            .map_or(source.as_str(), |match_| &source[..match_.start()]);
        for capture in literal_re.captures_iter(production) {
            let location = capture.get(0).map_or(0, |match_| match_.start());
            let line = production[..location]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            errors.push(format!("{}:{line}: raw production diagnostic code literal {:?}; reference pinewasm_span::codes::{} instead", path.strip_prefix(root).unwrap_or(&path).display(), &capture[1], &capture[1]));
        }
    }
    Ok(code_names.len())
}

fn validate_unique(label: &str, values: &[String], errors: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            duplicates.insert(value);
        }
    }
    if !duplicates.is_empty() {
        errors.push(format!("{label} contains duplicate codes: {duplicates:?}"));
    }
}

fn rust_sources(root: &Path) -> TaskResult<Vec<PathBuf>> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) -> TaskResult {
        for entry in fs::read_dir(directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        {
            let path = entry
                .map_err(|error| format!("cannot read directory entry: {error}"))?
                .path();
            if path.is_dir() {
                visit(&path, output)?;
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
                && path.components().any(|part| part.as_os_str() == "src")
            {
                output.push(path);
            }
        }
        Ok(())
    }
    let mut output = Vec::new();
    visit(root, &mut output)?;
    output.sort();
    Ok(output)
}

/// The runtime checksum table in `docs/aot.md`, keyed by artifact
/// file name. A duplicate row is an error because the table is the pin every
/// mechanical check compares the tracked blobs against.
pub(crate) fn documented_runtime_checksums(
    document: &Path,
) -> TaskResult<BTreeMap<String, String>> {
    let row_re = Regex::new(r"(?m)^\|\s*`([^`]+\.wasm)`\s*\|\s*`([0-9a-f]{64})`\s*\|$")
        .expect("valid regex");
    let mut documented = BTreeMap::new();
    for capture in row_re.captures_iter(&read(document)?) {
        if documented
            .insert(capture[1].to_string(), capture[2].to_string())
            .is_some()
        {
            return Err(format!(
                "{} contains duplicate runtime checksum row for {}",
                document.display(),
                &capture[1]
            ));
        }
    }
    Ok(documented)
}

fn validate_aot_hashes(document: &Path, runtime: &Path, errors: &mut Vec<String>) -> TaskResult {
    let documented = match documented_runtime_checksums(document) {
        Ok(documented) => documented,
        Err(error) => {
            errors.push(error);
            return Ok(());
        }
    };
    let expected = AOT_ARTIFACTS
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let actual = documented.keys().cloned().collect::<BTreeSet<_>>();
    if expected != actual {
        errors.push(format!("AOT runtime checksum table disagrees with the maintained artifact set; missing rows={:?}, unknown rows={:?}", expected.difference(&actual).collect::<Vec<_>>(), actual.difference(&expected).collect::<Vec<_>>()));
    }
    for artifact in AOT_ARTIFACTS {
        let actual_hash = sha256_file(&runtime.join(artifact))?;
        if documented.get(artifact) != Some(&actual_hash) {
            errors.push(format!(
                "{} has a stale checksum for {artifact}; documented={:?}, actual={actual_hash}",
                document.display(),
                documented.get(artifact)
            ));
        }
    }
    Ok(())
}
