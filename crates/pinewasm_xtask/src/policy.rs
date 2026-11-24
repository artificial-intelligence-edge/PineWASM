//! Public licensing checks: the canonical policy sentences every licensing
//! document must carry.

use crate::command::repo_root;
use crate::TaskResult;
use std::fs;
use std::path::Path;

/// The one phrase every document uses for the code AI Edge LLC can offer
/// under alternative terms.
pub(crate) const RIGHTS_FRAGMENT: &str =
    "portions for which AI Edge LLC holds the necessary rights";

/// The public commercial-use position, stated identically everywhere.
pub(crate) const COMMERCIAL_USE_SENTENCE: &str = "Commercial use is permitted under AGPL-3.0-only, subject to that license's conditions; alternative commercial terms are optional, never a prerequisite.";

/// What compiling a caller's script does not do.
pub(crate) const INPUT_OWNERSHIP_SENTENCE: &str = "Compiling caller-supplied Pine source with PineWASM does not give AI Edge LLC any ownership of that source or of the parts of the output that merely represent it.";

/// Every document that states the licensing position. Each must contain all
/// three canonical sentences.
pub(crate) const POLICY_DOCUMENTS: [&str; 5] = [
    "README.md",
    "docs/licensing.md",
    "CONTRIBUTING.md",
    "docs/aot.md",
    "docs/artifact-distribution.md",
];

const CANONICAL_SENTENCES: [(&str, &str); 3] = [
    ("relicensable-portions wording", RIGHTS_FRAGMENT),
    ("commercial-use sentence", COMMERCIAL_USE_SENTENCE),
    ("input-ownership sentence", INPUT_OWNERSHIP_SENTENCE),
];

pub(crate) fn public_check(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("usage: policy-check".into());
    }
    let root = repo_root()?;
    check_documents(&root)?;
    crate::artifact_notice::check_notice_text(&root)
}

/// Require every canonical policy sentence in every policy document.
/// Whitespace runs are collapsed on both sides so ordinary paragraph wrapping
/// does not count as drift.
pub(crate) fn check_documents(root: &Path) -> TaskResult {
    let mut failures = Vec::new();
    for document in POLICY_DOCUMENTS {
        let text = fs::read_to_string(root.join(document))
            .map_err(|error| format!("cannot read {document}: {error}"))?;
        let normalized = collapse_whitespace(&text);
        for (label, sentence) in CANONICAL_SENTENCES {
            if !normalized.contains(&collapse_whitespace(sentence)) {
                failures.push(format!("{document} does not contain the canonical {label}"));
            }
        }
    }
    if failures.is_empty() {
        println!(
            "Policy documents agree: {} canonical sentences in {} documents.",
            CANONICAL_SENTENCES.len(),
            POLICY_DOCUMENTS.len()
        );
        Ok(())
    } else {
        Err(format!(
            "licensing-policy wording has drifted:\n  {}",
            failures.join("\n  ")
        ))
    }
}

pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::temp_dir;
    use std::path::PathBuf;

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crate lives two levels below the repository root")
            .to_path_buf()
    }

    #[test]
    fn policy_documents_in_the_repository_agree() {
        check_documents(&repository_root()).unwrap();
    }

    #[test]
    fn policy_document_drift_is_reported_by_document_and_sentence() {
        let temporary = temp_dir("policy-drift").unwrap();
        let root = temporary.path();
        for document in POLICY_DOCUMENTS {
            let path = root.join(document);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut text = format!("{RIGHTS_FRAGMENT}\n{COMMERCIAL_USE_SENTENCE}\n");
            if document != "docs/aot.md" {
                // Wrapping the sentence across lines must still count as verbatim.
                text.push_str(&INPUT_OWNERSHIP_SENTENCE.replace(" of ", "\n  of "));
            }
            fs::write(&path, text).unwrap();
        }
        let error = check_documents(root).unwrap_err();
        assert!(
            error.contains("docs/aot.md does not contain the canonical input-ownership sentence"),
            "{error}"
        );
        assert!(!error.contains("README.md does not"), "{error}");
    }
}
