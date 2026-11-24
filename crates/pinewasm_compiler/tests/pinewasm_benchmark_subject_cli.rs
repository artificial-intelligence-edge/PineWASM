//! Behavioural checks for the measured benchmark subject.
//!
//! These are the properties the harness relies on and cannot check on its own:
//! that every implemented lane materializes the same series, that the
//! incremental path lands on the batch path's answer, that a lane the manifest
//! could name but the binary does not implement fails loudly instead of
//! reporting a fast zero, and that a failed run never travels as a status the
//! correctness gate would accept.

use pinewasm_compiler::MAX_INCREMENTAL_BARS;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new(label: &str) -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pinewasm-benchmark-subject-{}-{id}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary workspace");
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("write temporary input");
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A short deterministic feed. Exact values do not matter to these tests; that
/// every lane sees the same ones does.
fn feed(bars: usize) -> String {
    let close: Vec<f64> = (0..bars).map(|i| 100.0 + i as f64 * 0.5).collect();
    let data = json!({
        "data": {
            "open": close.iter().map(|c| c - 0.25).collect::<Vec<_>>(),
            "high": close.iter().map(|c| c + 1.0).collect::<Vec<_>>(),
            "low": close.iter().map(|c| c - 1.0).collect::<Vec<_>>(),
            "close": close,
            "volume": vec![1000.0; bars],
            "time": (0..bars).map(|i| 1_577_836_800_000_i64 + i as i64 * 60_000).collect::<Vec<_>>(),
            "timeframe": "1",
            "mintick": 0.01,
        }
    });
    serde_json::to_string(&data).expect("serialize feed")
}

fn measure(
    workspace: &Workspace,
    source: &str,
    lane: &str,
    warmups: u64,
    iterations: u64,
) -> Value {
    measure_over(workspace, source, lane, 64, warmups, iterations)
}

fn measure_over(
    workspace: &Workspace,
    source: &str,
    lane: &str,
    bars: usize,
    warmups: u64,
    iterations: u64,
) -> Value {
    let label = format!("{lane}-{bars}");
    let source_path = workspace.write(&format!("{label}.pine"), source);
    let input_path = workspace.write(&format!("{label}.input.json"), &feed(bars));
    let request_path = workspace.0.join(format!("{label}.request.json"));
    let response_path = workspace.0.join(format!("{label}.response.json"));
    let request = json!({
        "source": source_path,
        "input": input_path,
        "lane": lane,
        "warmups": warmups,
        "iterations": iterations,
    });
    fs::write(
        &request_path,
        serde_json::to_vec(&request).expect("serialize request"),
    )
    .expect("write request");
    let output = Command::new(env!("CARGO_BIN_EXE_pinewasm-benchmark-subject"))
        .arg(&request_path)
        .arg(&response_path)
        .output()
        .expect("run the benchmark subject");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&fs::read(&response_path).expect("read response"))
        .expect("valid response JSON")
}

const CUMULATIVE: &str =
    "//@version=6\nindicator(\"Cumulative\")\nvar float total = 0.0\ntotal += close\nplot(total, \"Cumulative\")\n";

#[test]
fn every_lane_materializes_the_same_series() {
    let workspace = Workspace::new("lane-agreement");
    // `persistent` state is the case that would diverge first if a lane reused
    // execution state it should have rebuilt, so it is the right shape here.
    let batch = measure(&workspace, CUMULATIVE, "cold-runtime", 0, 1);
    assert_eq!(batch["status"], "ok");
    for lane in ["compile", "cached", "incremental-bar", "cold-process"] {
        let other = measure(&workspace, CUMULATIVE, lane, 1, 2);
        assert_eq!(other["status"], "ok", "lane {lane}");
        assert_eq!(other["result"], batch["result"], "lane {lane} series");
    }
}

#[test]
fn the_incremental_lane_reaches_the_final_bar_through_its_own_path() {
    let workspace = Workspace::new("incremental-final-bar");
    let batch = measure(&workspace, CUMULATIVE, "cold-runtime", 0, 1);
    let incremental = measure(&workspace, CUMULATIVE, "incremental-bar", 0, 5);
    // Repeating the forming bar must not accumulate: five timed repetitions
    // plus the commit have to land on the same total the batch run produced.
    let final_of = |value: &Value| {
        value["result"]["plots"][0]["values"]
            .as_array()
            .unwrap()
            .last()
            .cloned()
            .unwrap()
    };
    assert_eq!(final_of(&incremental), final_of(&batch));
    assert_eq!(incremental["iterations"], json!(5));
}

/// Lowest per-iteration reading of three measured blocks.
///
/// A minimum is the right statistic for a scope check: interference can only
/// make a block slower, so the smallest of several readings is the closest
/// available estimate of the work itself.
fn fastest_per_iteration(workspace: &Workspace, source: &str, lane: &str, bars: usize) -> f64 {
    (0..3)
        .map(|_| {
            let measured = measure_over(workspace, source, lane, bars, 50, 200);
            assert_eq!(measured["status"], "ok", "lane {lane} at {bars} bars");
            measured["per_iteration_ms"].as_f64().expect("quotient")
        })
        .fold(f64::INFINITY, f64::min)
}

#[test]
fn the_incremental_lane_times_one_bar_rather_than_the_history_behind_it() {
    let workspace = Workspace::new("incremental-scope");
    // `CUMULATIVE` performs a fixed amount of work per bar, so a boundary that
    // really measures one bar reads the same at both feed lengths. A lane that
    // puts `IncrementalSession::advance` inside the timer snapshots every plot
    // lane from bar zero. Its reading then grows with the history: a release
    // build reads 0.0025 ms at 1000 bars against 0.045 ms at 20000 on this
    // constant-work script, which measures result materialization under the
    // name of one bar.
    let short = fastest_per_iteration(&workspace, CUMULATIVE, "incremental-bar", 256);
    let long = fastest_per_iteration(&workspace, CUMULATIVE, "incremental-bar", 4096);
    // Deliberately loose, and both conditions must hold: a sixteen-fold
    // history is what this is aimed at, not ordinary host noise, and a tiny
    // absolute reading cannot trip it.
    assert!(
        long <= short * 3.0 || long - short <= 0.01,
        "the incremental lane scaled with its history: {short} ms per bar at 256 bars against {long} ms at 4096"
    );
}

#[test]
fn every_declared_bar_count_fits_one_incremental_session() {
    // The subject-tier manifest is validated by `pinewasm_xtask`, which cannot
    // see the VM's resident-bar ceiling. Hold the relationship from this side:
    // a declared feed longer than one session can hold fails every
    // `incremental-bar` cell before measurement starts with "incremental session
    // bar limit exceeded", after the suite has already built a release binary
    // and prepared its inputs.
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../benchmarks/manifests/subject-tiers-v1.json"),
        )
        .expect("read the subject-tier manifest"),
    )
    .expect("valid manifest JSON");
    let bars = manifest["bars"].as_array().expect("declared bar counts");
    assert!(
        !bars.is_empty(),
        "a suite with no feed sizes measures nothing"
    );
    for value in bars {
        let count = value.as_u64().expect("integer bar count") as usize;
        // The lane seeds `count - 1` history bars and re-executes the last.
        assert!(count >= 2, "{count} bars leaves no forming bar to measure");
        assert!(
            count <= MAX_INCREMENTAL_BARS,
            "{count} bars exceeds the {MAX_INCREMENTAL_BARS}-bar incremental session ceiling"
        );
    }
}

#[test]
fn measured_blocks_report_their_own_per_iteration_quotient() {
    let workspace = Workspace::new("iteration-accounting");
    let measured = measure(&workspace, CUMULATIVE, "cached", 2, 4);
    let block = measured["elapsed_ms"].as_f64().expect("block reading");
    let per_iteration = measured["per_iteration_ms"].as_f64().expect("quotient");
    assert_eq!(measured["iterations"], json!(4));
    assert!(block > 0.0 && per_iteration > 0.0);
    assert!((block / 4.0 - per_iteration).abs() < 1e-9);
}

#[test]
fn the_artifact_lane_writes_the_reusable_program_and_claims_no_time() {
    let workspace = Workspace::new("artifact");
    let source_path = workspace.write("artifact.pine", CUMULATIVE);
    let input_path = workspace.write("artifact.input.json", &feed(64));
    let artifact_path = workspace.0.join("artifact.bin");
    let request_path = workspace.0.join("artifact.request.json");
    let response_path = workspace.0.join("artifact.response.json");
    fs::write(
        &request_path,
        serde_json::to_vec(&json!({
            "source": source_path,
            "input": input_path,
            "lane": "artifact-size",
            "warmups": 0,
            "iterations": 0,
            "artifact_out": artifact_path,
        }))
        .expect("serialize request"),
    )
    .expect("write request");
    let output = Command::new(env!("CARGO_BIN_EXE_pinewasm-benchmark-subject"))
        .arg(&request_path)
        .arg(&response_path)
        .output()
        .expect("run the benchmark subject");
    assert!(output.status.success());
    let response: Value =
        serde_json::from_slice(&fs::read(&response_path).expect("read response")).unwrap();
    assert_eq!(response["status"], "ok");
    // A size measurement that also carried a duration would invite someone to
    // quote it as a timing row.
    assert!(response.get("elapsed_ms").is_none());
    assert!(!fs::read(&artifact_path).expect("artifact bytes").is_empty());
}

#[test]
fn an_unimplemented_lane_is_an_error_rather_than_an_instant_result() {
    let workspace = Workspace::new("unimplemented-lane");
    let response = measure(&workspace, CUMULATIVE, "hosted-aot", 0, 1);
    assert_eq!(response["status"], "error");
    assert!(response["message"]
        .as_str()
        .expect("message")
        .contains("unimplemented lane"));
    assert!(response.get("elapsed_ms").is_none());
}

#[test]
fn a_failing_script_never_reports_a_passing_status() {
    let workspace = Workspace::new("runtime-failure");
    let response = measure(
        &workspace,
        "//@version=6\nindicator(\"Runtime failure\")\na = array.new_float(0)\nplot(array.get(a, 0))\n",
        "cold-runtime",
        0,
        1,
    );
    assert_eq!(response["status"], "error");
    assert!(response["message"]
        .as_str()
        .expect("message")
        .contains("runtime error"));
    assert!(response.get("result").is_none());
}
