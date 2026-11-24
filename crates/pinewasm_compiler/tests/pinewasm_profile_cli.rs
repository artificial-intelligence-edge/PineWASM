use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempFile(PathBuf);

impl TempFile {
    fn pine(source: &str) -> Self {
        Self::pine_named("source.pine", source)
    }

    fn pine_named(name: &str, source: &str) -> Self {
        Self::named(name, source)
    }

    fn named(name: &str, contents: &str) -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pinewasm-profile-cli-{}-{id}-{name}",
            std::process::id()
        ));
        fs::write(&path, contents).expect("write temporary test input");
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(file: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pinewasm-profile"))
        .args(args)
        .arg(file)
        .output()
        .expect("run pinewasm-profile")
}

fn profile_record(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .lines()
        .find_map(|line| line.strip_prefix("pinewasm-profile-json:"))
        .expect("machine-readable profile record");
    serde_json::from_str(json).expect("valid profile JSON")
}

#[test]
fn compile_failure_emits_a_record_and_exits_nonzero() {
    let source =
        TempFile::pine("//@version=6\nindicator(\"Compile failure\")\nplot(unknown_identifier)\n");
    let output = run(&source.0, &["--repeat", "1", "--no-run"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("compile failed"));
    let record = profile_record(&output);
    assert_eq!(record["file"].as_str(), source.0.to_str());
    assert!(record.get("ilMs").is_some());
    assert!(record.get("csilMs").is_none());
}

#[test]
fn runtime_failure_emits_a_record_and_exits_nonzero() {
    let source = TempFile::pine(
        "//@version=6\nindicator(\"Runtime failure\")\na = array.new_float(0)\nplot(array.get(a, 0))\n",
    );
    let output = run(&source.0, &["--repeat", "1", "--bars", "1"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("runtime error:"));
    assert_eq!(profile_record(&output)["file"].as_str(), source.0.to_str());
}

#[cfg(unix)]
#[test]
fn machine_record_json_escapes_an_unusual_filename() {
    let source = TempFile::pine_named(
        "quote-\"-backslash-\\-newline-\n.pine",
        "//@version=6\nindicator(\"Valid\")\nplot(close)\n",
    );
    let output = run(&source.0, &["--repeat", "1", "--no-run"]);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(profile_record(&output)["file"].as_str(), source.0.to_str());
}

#[test]
fn quoted_csv_fields_are_parsed_as_market_data() {
    let source = TempFile::pine("//@version=6\nindicator(\"Valid\")\nplot(close)\n");
    let csv = TempFile::named(
        "feed.csv",
        "\"time\",\"open\",\"high\",\"low\",\"close\",\"volume\"\r\n\
         \"1000000000\",\"1.0\",\"2.0\",\"0.5\",\"1.5\",\"10\"\r\n\
         \"1000000060\",\"1.5\",\"2.5\",\"1.0\",\"2.0\",\"20\"\r\n",
    );
    let csv_path = csv.0.to_str().expect("temporary path is UTF-8");
    let output = run(&source.0, &["--repeat", "1", "--csv", csv_path]);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 bars"));
    assert_eq!(profile_record(&output)["file"].as_str(), source.0.to_str());
}
