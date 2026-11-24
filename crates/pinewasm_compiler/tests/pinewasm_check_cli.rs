use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempFile(PathBuf);

impl TempFile {
    fn pine(source: &str) -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pinewasm-check-cli-{}-{id}.pine",
            std::process::id()
        ));
        fs::write(&path, source).expect("write Pine source");
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pinewasm-check"))
        .args(args)
        .output()
        .expect("run pinewasm-check")
}

#[test]
fn help_succeeds_and_no_input_fails() {
    assert!(run(&["--help"]).status.success());
    assert!(!run(&[]).status.success());
}

#[test]
fn builtin_catalog_prints_one_json_document_and_takes_no_files() {
    let output = run(&["--builtin-catalog"]);
    assert!(output.status.success());
    let catalog: Value = serde_json::from_slice(&output.stdout).expect("catalog JSON");
    assert_eq!(catalog["format"], "pinewasm-builtin-catalog");
    assert!(!run(&["--builtin-catalog", "extra.pine"]).status.success());
}

#[test]
fn missing_file_emits_a_failed_verdict_and_exits_nonzero() {
    let output = run(&["/definitely/not/a/pinewasm-input.pine"]);
    assert!(!output.status.success());
    let verdict: Value = serde_json::from_slice(&output.stdout).expect("verdict JSON");
    assert_eq!(verdict["ok"], false);
    assert_eq!(verdict["file"], "/definitely/not/a/pinewasm-input.pine");
    assert!(verdict["errors"][0]
        .as_str()
        .is_some_and(|message| message.starts_with("read error:")));
    assert_eq!(verdict["warnings"], Value::Array(Vec::new()));
}

#[test]
fn invalid_source_emits_a_failed_verdict_and_exits_nonzero() {
    let source = TempFile::pine("//@version=6\nindicator(\"Bad\")\nplot(unknown_identifier)\n");
    let output = Command::new(env!("CARGO_BIN_EXE_pinewasm-check"))
        .arg(&source.0)
        .output()
        .expect("run pinewasm-check");
    assert!(!output.status.success());
    let verdict: Value = serde_json::from_slice(&output.stdout).expect("verdict JSON");
    assert_eq!(verdict["ok"], false);
    assert!(verdict["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
}

#[test]
fn valid_source_exits_successfully() {
    let source = TempFile::pine("//@version=6\nindicator(\"Good\")\nplot(close)\n");
    let output = Command::new(env!("CARGO_BIN_EXE_pinewasm-check"))
        .arg(&source.0)
        .output()
        .expect("run pinewasm-check");
    assert!(output.status.success());
    let verdict: Value = serde_json::from_slice(&output.stdout).expect("verdict JSON");
    assert_eq!(verdict["ok"], true);
}
