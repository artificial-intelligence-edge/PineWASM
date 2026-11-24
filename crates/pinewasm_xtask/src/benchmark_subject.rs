//! The PineWASM subject tier: every boundary `docs/benchmarks.md` defines for
//! this implementation alone, checked against the expected series that
//! `benchmark_run` computes locally.
//!
//! The comparison runner in `benchmark_run` needs Node and pinned packages, so
//! its correctness gate cannot run from a clean checkout. This tier
//! closes that gap for the subject: the measured program is
//! `pinewasm-benchmark-subject`, a first-party optimized binary built from this
//! workspace, so `probe` and `smoke` reproduce from the checkout and the pinned
//! Rust toolchain alone: no Node runtime, and no network access beyond the
//! crate registry the workspace build already needs.
//!
//! Everything that decides whether a number is trustworthy is shared with the
//! comparison tier on purpose: the deterministic input generator, the locally
//! computed expected series, the tolerance rules, the comparison, and the
//! archive schema all come from `benchmark_run`. Only the lanes and the
//! statistics differ, because a single-subject tier has no partner to ratio
//! against.
//!
//! This module measures and reports. It accepts no public performance claim.

use crate::benchmark_run::{
    archive, check_result, command_text, epoch, generated, hardware_profile, hash, median,
    merge_hardware, number, p95, preserve_output, read, shuffled, sizes, strings, unpack_archive,
    write, ARCHIVED_BAR_COUNTS,
};
use crate::command::{git, repo_root, sha256, sha256_file};
use crate::TaskResult;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MANIFEST: &str = "benchmarks/manifests/subject-tiers-v1.json";
const SUBJECT_BIN: &str = "pinewasm-benchmark-subject";
const SUBJECT_PACKAGE: &str = "pinewasm_compiler";

/// The timing boundaries this tier implements today.
///
/// `docs/benchmarks.md` defines nine. The three it does not name here —
/// generated JavaScript/Wasm binding, hosted AOT, and standalone AOT — are
/// declared unimplemented in the manifest rather than approximated.
const LANES: &[&str] = &[
    "cold-process",
    "cold-runtime",
    "compile",
    "cached",
    "incremental-bar",
];

/// The non-timing measurement: exact artifact bytes for the reusable compiled
/// program. It is deterministic for a given source and compiler revision,
/// which is what makes it the one benchmark number an ordinary shared runner
/// can legitimately regress on.
const ARTIFACT_LANE: &str = "artifact-size";

/// The non-timing process resource measurement. It runs the exact
/// `cold-process` subject boundary under the operating system's process
/// accounting facility; it is kept out of `LANES` so no wall-clock sample can
/// accidentally be inferred from the sampler wrapper.
const MEMORY_LANE: &str = "peak-memory";
const MEMORY_CADENCE: &str = "Kernel-maintained process high-water mark; no polling interval";

/// The boundaries `docs/benchmarks.md` defines that this tier does not measure.
///
/// The register is closed in both directions. A lane the runner implements may
/// not be declared missing, and none of these may be dropped: the methodology's
/// status table tells a reader that three absent boundaries are named in the
/// manifest with the reason they are absent, so an emptied register would leave
/// that sentence describing something the repository no longer contains.
const UNIMPLEMENTED_LANES: &[&str] = &["hosted-aot", "js-wasm-binding", "standalone-aot"];

const WORKLOADS: &[&str] = &[
    "history",
    "arithmetic",
    "sma",
    "control",
    "persistent",
    "collections",
    "legacy-mixed",
];

fn manifest(root: &Path) -> TaskResult<Value> {
    let m = read(&root.join(MANIFEST))?;
    if number(&m, "schema_version")? != 1
        || m["suite"] != "subject-tiers-v1"
        || m["status"] != "exploratory-not-accepted"
        || m["subject"] != "pinewasm"
    {
        return Err("invalid subject-tier manifest identity".into());
    }
    if !(30..=1000).contains(&number(&m, "samples")?) {
        return Err("sample count outside the supported range".into());
    }
    if number(&m, "timeout_seconds")? == 0 || number(&m, "timeout_seconds")? > 600 {
        return Err("invalid timeout".into());
    }
    if number(&m, "bootstrap_resamples")? != 10000 {
        return Err("invalid bootstrap resample count".into());
    }
    for key in ["workloads", "lanes"] {
        let names = strings(&m, key)?;
        let unique: BTreeSet<_> = names.iter().collect();
        if names.is_empty() || names.len() != unique.len() {
            return Err(format!("empty or duplicated identifiers in {key}"));
        }
    }
    // Closure: nothing may be declared that the runner cannot actually enter,
    // and no implemented lane may be silently dropped from the suite.
    if strings(&m, "workloads")? != WORKLOADS {
        return Err("declared workloads do not match the first-party set".into());
    }
    if strings(&m, "lanes")? != LANES {
        return Err("declared lanes do not match the implemented set".into());
    }
    if m["artifact_lane"] != ARTIFACT_LANE {
        return Err("declared artifact measurement is not implemented".into());
    }
    let memory = &m["memory_lane"];
    if memory["id"] != MEMORY_LANE
        || memory["measured_boundary"] != "cold-process"
        || number(memory, "samples")? != number(&m, "samples")?
    {
        return Err("declared peak-memory measurement is not implemented".into());
    }
    if memory["sampler"].as_str().is_none_or(str::is_empty) {
        return Err("peak-memory sampler is missing".into());
    }
    if memory["cadence"] != MEMORY_CADENCE {
        return Err("peak-memory cadence differs from the runner".into());
    }
    for (platform, sampler, field, bytes_per_unit) in [
        ("macos", "/usr/bin/time -l", "maximum resident set size", 1),
        (
            "linux",
            "/usr/bin/time -v",
            "Maximum resident set size (kbytes)",
            1024,
        ),
    ] {
        let registered = &memory["platforms"][platform];
        if registered["sampler"] != sampler
            || registered["field"] != field
            || registered["bytes_per_unit"] != bytes_per_unit
        {
            return Err(format!(
                "peak-memory {platform} sampler registration drifted"
            ));
        }
    }
    for name in strings(&m, "diagnostic_workloads")? {
        if !WORKLOADS.contains(&name.as_str()) {
            return Err(format!("diagnostic workload {name} is not in the suite"));
        }
    }
    for key in [
        "absolute_tolerance",
        "relative_tolerance",
        "precision_floor_ms",
    ] {
        if !m[key].as_f64().is_some_and(|x| x.is_finite() && x >= 0.0) {
            return Err(format!("invalid {key}"));
        }
    }
    for lane in strings(&m, "lanes")? {
        if m["warmups"][&lane].as_u64().is_none_or(|n| n > 1000) {
            return Err(format!("invalid warmup count for {lane}"));
        }
        let iterations = m["iterations"][&lane]
            .as_u64()
            .ok_or_else(|| format!("missing inner iteration count for {lane}"))?;
        if iterations == 0 || iterations > 100_000 {
            return Err(format!("invalid inner iteration count for {lane}"));
        }
        // A process boundary cannot be repeated inside one measured block; a
        // manifest that asked for it would be reporting a number the runner
        // never took.
        if lane == "cold-process" && iterations != 1 {
            return Err("the cold-process lane measures exactly one process".into());
        }
    }
    if !(50..=5000).contains(&number(&m, "smoke_bars")?) {
        return Err("invalid smoke bar count".into());
    }
    // Closure over the bundle writer as well as the runner. `bars` decides
    // roughly thirty measured blocks per cell, so a value the archive cannot
    // name — or one the incremental session would refuse — has to fail here
    // rather than at the end of a measurement window. The archived set is far
    // below `pinewasm_vm::MAX_INCREMENTAL_BARS`, and
    // `every_declared_bar_count_fits_one_incremental_session` in the subject
    // binary's tests holds that relationship from the other side.
    let bars = sizes(&m)?;
    let unique: BTreeSet<_> = bars.iter().collect();
    if bars.is_empty() || bars.len() != unique.len() {
        return Err("empty or duplicated bar counts".into());
    }
    if let Some(count) = bars
        .iter()
        .find(|count| !ARCHIVED_BAR_COUNTS.contains(count))
    {
        return Err(format!(
            "bar count {count} is outside the archivable set {ARCHIVED_BAR_COUNTS:?}"
        ));
    }
    let register = m["unimplemented_lanes"]
        .as_object()
        .ok_or("unimplemented lane register missing")?;
    for (name, reason) in register {
        if LANES.contains(&name.as_str()) {
            return Err(format!("{name} is implemented but declared missing"));
        }
        if reason.as_str().is_none_or(str::is_empty) {
            return Err(format!("{name} is declared missing without a reason"));
        }
    }
    if register.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != UNIMPLEMENTED_LANES.iter().copied().collect::<BTreeSet<_>>()
    {
        return Err(format!(
            "the unimplemented-lane register must name exactly {UNIMPLEMENTED_LANES:?}"
        ));
    }
    strings(&m, "limitations")?;
    Ok(m)
}

/// Where Cargo put the optimized subject binary.
///
/// `CARGO_TARGET_DIR` is honoured because a reproducer may share one target
/// directory across worktrees; guessing `target/` there would build one binary
/// and hash another.
fn target_root(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || root.join("target"),
        |value| {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        },
    )
}

/// Build the measured subject and return its path.
///
/// `optimized` is false only for the correctness smoke check, which never
/// reports a time and therefore has no reason to pay for a release build on a
/// shared runner. Every timing entry point demands the optimized binary.
fn subject_binary(root: &Path, optimized: bool) -> TaskResult<PathBuf> {
    let mut command = crate::command::pinned_cargo(root)?;
    command.args([
        "build",
        "--locked",
        "-p",
        SUBJECT_PACKAGE,
        "--bin",
        SUBJECT_BIN,
    ]);
    if optimized {
        command.arg("--release");
    }
    crate::command::run(&mut command)?;
    let path = target_root(root)
        .join(if optimized { "release" } else { "debug" })
        .join(SUBJECT_BIN);
    if !path.is_file() {
        return Err(format!("subject binary missing: {}", path.display()));
    }
    Ok(path)
}

fn prepare(root: &Path, bars: &[usize]) -> TaskResult {
    for &count in bars {
        write(
            &target_root(root).join(format!("benchmark-data/{count}.json")),
            &generated(count),
        )?;
    }
    Ok(())
}

#[derive(Clone)]
struct Cell {
    workload: String,
    bars: usize,
    lane: String,
}
impl Cell {
    fn id(&self) -> String {
        format!("{}-{}-{}", self.workload, self.bars, self.lane)
    }
    fn info(&self) -> Value {
        json!({"id":self.id(),"workload":self.workload,"bars":self.bars,"lane":self.lane})
    }
}

fn cells(m: &Value) -> TaskResult<Vec<Cell>> {
    let mut out = Vec::new();
    for workload in strings(m, "workloads")? {
        for bars in sizes(m)? {
            for lane in strings(m, "lanes")? {
                out.push(Cell {
                    workload: workload.clone(),
                    bars,
                    lane,
                });
            }
        }
    }
    Ok(out)
}

/// The three things every invocation needs: the checkout, the manifest, and
/// the exact subject binary the provenance snapshot hashed.
struct Harness<'a> {
    root: &'a Path,
    manifest: &'a Value,
    subject: &'a Path,
}

/// Run one measured invocation in its own process.
///
/// Every lane gets a fresh process, not just `cold-process`: independent
/// samples are what the block design assumes, and a long-lived measuring
/// process would let one cell's allocator and page state leak into the next.
/// For `cold-process` the parent's spawn-to-exit measurement replaces the
/// child's own reading, because that lane's boundary is the process itself.
fn invoke(
    h: &Harness<'_>,
    cell: &Cell,
    folder: &Path,
    label: &str,
    warmups: u64,
    iterations: u64,
) -> TaskResult<Value> {
    let (root, m, subject) = (h.root, h.manifest, h.subject);
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let request = folder.join(format!("{label}.request.json"));
    let response = folder.join(format!("{label}.response.json"));
    let artifact = folder.join(format!("{label}.artifact.bin"));
    let mut body = json!({
        "source": format!("benchmarks/workloads/{}.pine", cell.workload),
        "input": target_root(root).join(format!("benchmark-data/{}.json", cell.bars)),
        "lane": cell.lane,
        "warmups": warmups,
        "iterations": iterations,
    });
    if cell.lane == ARTIFACT_LANE {
        body["artifact_out"] = json!(artifact);
    }
    write(&request, &body)?;
    let log = File::create(folder.join(format!("{label}.log"))).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut child = Command::new(subject)
        .current_dir(root)
        .arg(&request)
        .arg(&response)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| e.to_string())?;
    let timeout = Duration::from_secs(number(m, "timeout_seconds")?);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if start.elapsed() > timeout {
            child.kill().map_err(|e| e.to_string())?;
            child.wait().map_err(|e| e.to_string())?;
            return Ok(json!({"status":"timeout"}));
        }
        thread::sleep(Duration::from_millis(1));
    };
    let process_ms = start.elapsed().as_secs_f64() * 1000.0;
    // A response written before a fatal exit is not a successful measurement.
    // Preserve the process failure even if the child managed to write status=ok.
    if !status.success() {
        return Ok(json!({"status":"crashed","exit_code":status.code()}));
    }
    if !response.exists() {
        return Ok(json!({"status":"crashed","exit_code":status.code()}));
    }
    let mut observation = read(&response)?;
    fs::remove_file(&response).map_err(|e| e.to_string())?;
    if cell.lane == "cold-process" && observation["status"] == "ok" {
        // The child's own reading of the same work stays as diagnostic context,
        // the way an engine's self-reported timing is kept beside the wall clock.
        observation["subject_ms"] = observation["elapsed_ms"].clone();
        observation["elapsed_ms"] = json!(process_ms);
        observation["iterations"] = json!(1);
        observation["per_iteration_ms"] = json!(process_ms);
    }
    if cell.lane == ARTIFACT_LANE && observation["status"] == "ok" {
        observation["artifact"] = artifact_bytes(&artifact)?;
        fs::remove_file(&artifact).map_err(|e| e.to_string())?;
    }
    Ok(observation)
}

/// Configure the platform process-accounting wrapper.
///
/// Both supported implementations read the kernel-maintained high-water mark
/// after the child exits. There is no polling interval that can miss a short
/// allocation spike. Unsupported hosts are recorded as unavailable instead of
/// substituting a sampled or estimated value.
fn memory_sampler(output: &Path) -> Option<(Command, &'static str, &'static str, u64)> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/usr/bin/time");
        command.args(["-l", "-o"]).arg(output);
        Some((command, "/usr/bin/time -l", "maximum resident set size", 1))
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("/usr/bin/time");
        command.args(["-v", "-o"]).arg(output);
        Some((
            command,
            "/usr/bin/time -v",
            "Maximum resident set size (kbytes)",
            1024,
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = output;
        None
    }
}

fn parse_peak_rss(raw: &str, label: &str, bytes_per_unit: u64) -> TaskResult<u64> {
    let units = raw
        .lines()
        .find_map(|line| {
            let line = line.trim();
            if let Some(value) = line
                .strip_prefix(label)
                .and_then(|tail| tail.strip_prefix(':'))
            {
                return Some(value.trim());
            }
            line.strip_suffix(label).map(str::trim)
        })
        .ok_or_else(|| format!("memory sampler output has no `{label}` field"))?
        .parse::<u64>()
        .map_err(|error| format!("invalid peak resident-set value: {error}"))?;
    units
        .checked_mul(bytes_per_unit)
        .ok_or_else(|| "peak resident-set byte count overflowed".into())
}

fn provenance_platform(snapshot: &Value) -> Option<&'static str> {
    let os = snapshot["os"].as_str()?;
    if os == "Darwin" || os.starts_with("Darwin ") {
        Some("macos")
    } else if os == "Linux" || os.starts_with("Linux ") {
        Some("linux")
    } else {
        None
    }
}

fn verified_output_path(folder: &Path, observation: &Value, context: &str) -> TaskResult<PathBuf> {
    let digest = observation["output_sha256"]
        .as_str()
        .ok_or_else(|| format!("{context} output digest missing"))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{context} output digest is not canonical lowercase sha256"
        ));
    }
    let relative = observation["output_path"]
        .as_str()
        .ok_or_else(|| format!("{context} output path missing"))?;
    if relative != format!("outputs/{digest}.json") {
        return Err(format!(
            "{context} output path is not canonical or digest-bound"
        ));
    }
    let path = folder.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{context} output is not a self-contained regular file"
        ));
    }
    if hash(&path)? != digest {
        return Err(format!("{context} output hash mismatch"));
    }
    Ok(path)
}

/// Run the named cold-process boundary under a separate resource sampler.
///
/// This is intentionally not `invoke`: starting `/usr/bin/time` would add the
/// wrapper's startup to a wall-clock reading. The elapsed fields are discarded
/// here and only the child's kernel-accounted high-water resident set is kept.
fn invoke_memory(h: &Harness<'_>, cell: &Cell, folder: &Path, label: &str) -> TaskResult<Value> {
    let (root, m, subject) = (h.root, h.manifest, h.subject);
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let sampler_output = folder.join(format!("{label}.sampler.txt"));
    let Some((mut command, sampler, field, bytes_per_unit)) = memory_sampler(&sampler_output)
    else {
        return Ok(json!({
            "status": "unavailable",
            "message": format!("peak resident-set process accounting is not registered for {}", std::env::consts::OS),
            "platform": std::env::consts::OS,
        }));
    };
    let request = folder.join(format!("{label}.request.json"));
    let response = folder.join(format!("{label}.response.json"));
    write(
        &request,
        &json!({
            "source": format!("benchmarks/workloads/{}.pine", cell.workload),
            "input": target_root(root).join(format!("benchmark-data/{}.json", cell.bars)),
            "lane": "cold-process",
            "warmups": 0,
            "iterations": 1,
        }),
    )?;
    let log = File::create(folder.join(format!("{label}.log"))).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .current_dir(root)
        .arg(subject)
        .arg(&request)
        .arg(&response)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("start memory sampler: {e}"))?;
    let start = Instant::now();
    let timeout = Duration::from_secs(number(m, "timeout_seconds")?);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if start.elapsed() > timeout {
            #[cfg(unix)]
            {
                let group = rustix::process::Pid::from_raw(child.id() as i32)
                    .ok_or("memory sampler returned an invalid process id")?;
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                    .map_err(|e| format!("kill timed-out memory sampler group: {e}"))?;
            }
            #[cfg(not(unix))]
            child.kill().map_err(|e| e.to_string())?;
            child.wait().map_err(|e| e.to_string())?;
            return Ok(json!({"status":"timeout"}));
        }
        thread::sleep(Duration::from_millis(1));
    };
    if !status.success() || !response.exists() {
        return Ok(json!({"status":"crashed","exit_code":status.code()}));
    }
    let raw = fs::read_to_string(&sampler_output).map_err(|e| e.to_string())?;
    let peak_rss_bytes = parse_peak_rss(&raw, field, bytes_per_unit)?;
    let mut observation = read(&response)?;
    fs::remove_file(&response).map_err(|e| e.to_string())?;
    if let Some(object) = observation.as_object_mut() {
        object.remove("elapsed_ms");
        object.remove("iterations");
        object.remove("per_iteration_ms");
    }
    if observation["status"] == "ok" {
        observation["memory"] = json!({
            "peak_rss_bytes": peak_rss_bytes,
            "platform": std::env::consts::OS,
            "sampler": sampler,
            "field": field,
            "source_units_to_bytes": bytes_per_unit,
            "cadence": MEMORY_CADENCE,
            "raw_output": raw,
            "raw_output_sha256": sha256(raw.as_bytes()),
        });
    }
    Ok(observation)
}

/// Exact uncompressed bytes, the named compression method, and the compressed
/// bytes for the artifact the subject actually emitted, plus its digest so a
/// reviewer can confirm two runs measured the same artifact.
fn artifact_bytes(path: &Path) -> TaskResult<Value> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&bytes).map_err(|e| e.to_string())?;
    let compressed = encoder.finish().map_err(|e| e.to_string())?;
    Ok(json!({
        "uncompressed_bytes": bytes.len(),
        "compression": "gzip-deflate-default",
        "compressed_bytes": compressed.len(),
        "sha256": sha256_file(path)?,
    }))
}

/// Read the expected series for one cell straight out of the prepared input,
/// so the comparison and the measured run always see one generator output.
fn expected(h: &Harness<'_>, cell: &Cell) -> TaskResult<Value> {
    let input = read(&target_root(h.root).join(format!("benchmark-data/{}.json", cell.bars)))?;
    Ok(input["expected"][&cell.workload].clone())
}

/// Untimed correctness pass over every cell, including the diagnostic
/// workload. A cell becomes eligible for timing only here.
fn probe(h: &Harness<'_>, folder: &Path) -> TaskResult<Vec<Value>> {
    let m = h.manifest;
    let diagnostics = strings(m, "diagnostic_workloads")?;
    let work = folder.join("invocations");
    let mut gates = Vec::new();
    for cell in cells(m)? {
        let mut row = cell.info();
        let mut observation = invoke(h, &cell, &work, &format!("gate-{}", cell.id()), 0, 1)?;
        // This pass records correctness, never a time: keeping the reading would
        // invite someone to quote a one-shot unwarmed number as a result.
        if let Some(object) = observation.as_object_mut() {
            object.remove("elapsed_ms");
            object.remove("per_iteration_ms");
            object.remove("subject_ms");
        }
        if observation["status"] == "ok" {
            observation["correctness"] =
                check_result(m, &observation["result"], &expected(h, &cell)?);
        }
        row["eligible"] = json!(
            observation["status"] == "ok"
                && observation["correctness"]["pass"] == true
                && !diagnostics.contains(&cell.workload)
        );
        preserve_output(folder, &mut observation)?;
        println!(
            "gate {}: {}",
            cell.id(),
            if row["eligible"] == true {
                "pass"
            } else if observation["correctness"]["pass"] == true {
                "correct, not timed"
            } else {
                "FAILED"
            }
        );
        row["observation"] = observation;
        gates.push(row);
        write(&folder.join("gates.json"), &json!(gates))?;
    }
    Ok(gates)
}

/// Exact artifact sizes for every workload. Independent of bar count, so this
/// runs once per workload against the smallest declared feed.
fn measure_artifacts(h: &Harness<'_>, folder: &Path) -> TaskResult<Value> {
    let m = h.manifest;
    let bars = *sizes(m)?.first().ok_or("no declared bar counts")?;
    let mut rows = Vec::new();
    for workload in strings(m, "workloads")? {
        let cell = Cell {
            workload: workload.clone(),
            bars,
            lane: ARTIFACT_LANE.to_string(),
        };
        let mut observation = invoke(
            h,
            &cell,
            &folder.join("invocations"),
            &format!("artifact-{workload}"),
            0,
            1,
        )?;
        if observation["status"] == "ok" {
            observation["correctness"] =
                check_result(m, &observation["result"], &expected(h, &cell)?);
        }
        preserve_output(folder, &mut observation)?;
        // The artifact pass is otherwise silent, so a failed invocation would
        // only surface once the report was written. Say it while the run is
        // still on screen.
        println!(
            "artifact {workload}: {}",
            if observation["status"] == "ok" {
                observation["artifact"]["uncompressed_bytes"].to_string()
            } else {
                format!("FAILED — {}", artifact_failure(&observation))
            }
        );
        rows.push(json!({"workload":workload,"observation":observation}));
    }
    let value = json!(rows);
    write(&folder.join("artifacts.json"), &value)?;
    Ok(value)
}

/// Independent peak-RSS observations for the named cold-process boundary.
///
/// The `probe` run takes one observation per cell to exercise the sampler and retain a
/// correctness record. A full run takes the manifest's 30 independent samples
/// in seeded randomized blocks; resource observations happen after every
/// timing block so the sampler wrapper cannot perturb the timed experiment.
fn measure_memory(h: &Harness<'_>, folder: &Path, samples: u64) -> TaskResult<bool> {
    let m = h.manifest;
    let mut cells = Vec::new();
    for workload in strings(m, "workloads")? {
        for bars in sizes(m)? {
            cells.push(Cell {
                workload: workload.clone(),
                bars,
                lane: MEMORY_LANE.to_string(),
            });
        }
    }
    let mut rng = ChaCha8Rng::seed_from_u64(number(m, "order_seed")? ^ 0x0050_4541_4b52_5353);
    let mut rows = Vec::new();
    write(&folder.join("memory.json"), &json!(rows))?;
    for block in 0..samples {
        shuffled(&mut cells, &mut rng);
        for cell in &cells {
            if folder.join("stop-request.json").exists() {
                return Ok(false);
            }
            let mut observation = invoke_memory(
                h,
                cell,
                &folder.join("invocations"),
                &format!("memory-block-{block}-{}-{}", cell.workload, cell.bars),
            )?;
            if observation["status"] == "ok" {
                observation["correctness"] =
                    check_result(m, &observation["result"], &expected(h, cell)?);
            }
            preserve_output(folder, &mut observation)?;
            rows.push(json!({"block":block,"cell":cell.info(),"observation":observation}));
            write(&folder.join("memory.json"), &json!(rows))?;
        }
        println!(
            "Completed peak-memory block {}/{} ({} cells)",
            block + 1,
            samples,
            cells.len()
        );
    }
    Ok(true)
}

/// Percentile bootstrap interval for the median of one sample set.
///
/// The comparison tier bootstraps a paired ratio because it compares two
/// implementations on the same blocks. A single-subject cell has no partner, so the
/// honest interval is around its own median; resampling with the manifest's
/// seed keeps it regenerable from the raw samples.
fn median_ci(xs: &[f64], seed: u64, count: usize) -> [f64; 2] {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(count);
    for _ in 0..count {
        let mut sample = Vec::with_capacity(xs.len());
        for _ in 0..xs.len() {
            sample.push(xs[(rng.next_u64() % xs.len() as u64) as usize]);
        }
        estimates.push(median(&sample));
    }
    estimates.sort_by(f64::total_cmp);
    [
        estimates[(25 * count).div_ceil(1000) - 1],
        estimates[(975 * count).div_ceil(1000) - 1],
    ]
}

/// Why a cell carries no timing row.
///
/// The recorded correctness outranks the diagnostic flag on purpose. A
/// diagnostic workload is ineligible whether or not it agreed with the
/// expected series, so labelling every ineligible diagnostic cell "diagnostic only"
/// would render a real numeric divergence as an intentional exclusion — and
/// the bundle's own narrative is what a reader of `report.md` sees. `gates.json`
/// would still hold the first differences, but nobody reading `report.md` or
/// `summary.json` would know to look.
fn untimed_reason(gate: &Value, diagnostic: bool) -> &'static str {
    let observation = &gate["observation"];
    let correct = observation["status"] == "ok" && observation["correctness"]["pass"] == true;
    match (correct, diagnostic) {
        (false, true) => "diagnostic, numeric mismatch or failed invocation",
        (false, false) => "numeric mismatch or failed invocation",
        (true, _) if gate["eligible"] == true => "incomplete or below-floor samples",
        (true, true) => "diagnostic only",
        // `probe` makes a correct non-diagnostic cell eligible, so this is
        // unreachable through the runner; it stays an honest label rather than
        // an unexplained blank if a bundle is ever assembled another way.
        (true, false) => "correct but not marked eligible",
    }
}

fn report(folder: &Path) -> TaskResult {
    let m = read(&folder.join("manifest.json"))?;
    let raw = read(&folder.join("samples.json"))?;
    let gates = read(&folder.join("gates.json"))?;
    if folder.join("disposition.json").exists() {
        return report_interrupted(folder, &raw);
    }
    let before = read(&folder.join("environment-before.json"))?;
    let after = read(&folder.join("environment-after.json"))?;
    if before["sha256"] != after["sha256"]
        || before["runner_commit"] != after["runner_commit"]
        || before["dirty"] != ""
        || after["dirty"] != ""
    {
        return Err("run provenance is inconsistent or the source tree was dirty".into());
    }
    // Two equally blank hardware fields compare equal, so the before/after
    // check above cannot notice that neither snapshot states a CPU or a power
    // mode. A timing report is refused outright rather than published with the
    // hardware provenance left empty.
    for snapshot in [&before, &after] {
        if snapshot["provenance_complete"] != true {
            let stated = |key: &str| snapshot[key].as_str().unwrap_or_default().to_owned();
            return Err(format!(
                "provenance is incomplete on this host and no timing is reported from it \
                 (cpu `{}`, memory_bytes `{}`, logical_cpus `{}`, power `{}`); record the \
                 missing hardware facts, or declare the power mode in \
                 PINEWASM_BENCHMARK_POWER_MODE, and run again",
                stated("cpu"),
                stated("memory_bytes"),
                stated("logical_cpus"),
                stated("power"),
            ));
        }
    }
    let floor = m["precision_floor_ms"]
        .as_f64()
        .ok_or("precision floor missing")?;
    let mut inputs = BTreeMap::new();
    for bars in sizes(&m)? {
        let path = folder.join(format!("inputs/{bars}.json"));
        if json!(hash(&path)?) != before["sha256"][format!("benchmark-data/{bars}.json")] {
            return Err("archived input hash differs from run provenance".into());
        }
        inputs.insert(bars, read(&path)?);
    }
    let mut samples: BTreeMap<String, BTreeMap<u64, f64>> = BTreeMap::new();
    let mut failed = BTreeSet::new();
    for record in raw.as_array().ok_or("samples must be an array")? {
        let id = record["cell"]["id"]
            .as_str()
            .ok_or("sample id missing")?
            .to_owned();
        let observation = &record["observation"];
        if observation["status"] != "ok" || observation["correctness"]["pass"] != true {
            failed.insert(id);
            continue;
        }
        let path = folder.join(
            observation["output_path"]
                .as_str()
                .ok_or("output path missing")?,
        );
        if json!(hash(&path)?) != observation["output_sha256"] {
            return Err("measured output hash mismatch".into());
        }
        let bars = number(&record["cell"], "bars")? as usize;
        let workload = record["cell"]["workload"]
            .as_str()
            .ok_or("workload missing")?;
        if check_result(
            &m,
            &read(&path)?,
            &inputs.get(&bars).ok_or("input missing")?["expected"][workload],
        )["pass"]
            != true
        {
            return Err("a stored passing correctness result is not reproducible".into());
        }
        // The precision floor applies to the measured block, not to the
        // per-iteration quotient: dividing a block that the clock could barely
        // resolve does not make the quotient trustworthy.
        let block_ms = observation["elapsed_ms"]
            .as_f64()
            .ok_or("elapsed missing")?;
        let per_iteration = observation["per_iteration_ms"]
            .as_f64()
            .ok_or("per-iteration reading missing")?;
        if !block_ms.is_finite() || !per_iteration.is_finite() || block_ms < floor {
            failed.insert(id.clone());
        }
        let block = number(record, "block")?;
        if samples
            .entry(id)
            .or_default()
            .insert(block, per_iteration)
            .is_some()
        {
            return Err("duplicate sample block".into());
        }
    }
    let before_platform = provenance_platform(&before);
    let after_platform = provenance_platform(&after);
    if before_platform != after_platform {
        return Err("peak-memory platform changed across environment snapshots".into());
    }
    write_report(folder, &m, &gates, &samples, &failed, before_platform)
}

fn report_interrupted(folder: &Path, raw: &Value) -> TaskResult {
    let disposition = read(&folder.join("disposition.json"))?;
    if disposition["status"] != "interrupted" {
        return Err("unrecognized run disposition".into());
    }
    let completed = raw.as_array().ok_or("samples missing")?.len();
    let reason = disposition["reason"]
        .as_str()
        .ok_or("interruption reason missing")?;
    fs::write(
        folder.join("report.md"),
        format!("# Interrupted subject-tier run\n\nThis run was interrupted after {completed} recorded samples. No medians, intervals, or comparisons are published from it.\n\n{reason}\n\nThe bundle keeps the manifest, the initial environment, the complete correctness matrix, the completed samples, and the deduplicated outputs. Missing samples are not substituted.\n"),
    )
    .map_err(|e| e.to_string())?;
    write(
        &folder.join("summary.json"),
        &json!({"status":"interrupted","completed_samples":completed,"reason":disposition["reason"]}),
    )
}

fn write_report(
    folder: &Path,
    m: &Value,
    gates: &Value,
    samples: &BTreeMap<String, BTreeMap<u64, f64>>,
    failed: &BTreeSet<String>,
    provenance_platform: Option<&str>,
) -> TaskResult {
    let diagnostics = strings(m, "diagnostic_workloads")?;
    let required = number(m, "samples")? as usize;
    let mut md = String::from("# Subject-tier boundary measurements\n\nThese are not accepted public benchmark results. Each row is one PineWASM timing boundary measured on one workload and feed size; no row is comparable to another row's boundary. Intervals are seeded percentile bootstrap 95% intervals for the median. p95 is descriptive at this sample count. Failed and diagnostic cells stay in the matrix.\n\n| Workload | Bars | Lane | Samples | Iterations | Block median ms | Per-iteration median ms | p95 ms | 95% interval |\n|---|---:|---|---:|---:|---:|---:|---:|---|\n");
    let mut summary = Vec::new();
    for cell in cells(m)? {
        let id = cell.id();
        let gate = gates
            .as_array()
            .ok_or("gates missing")?
            .iter()
            .find(|g| g["id"] == id)
            .ok_or("gate missing")?;
        let xs: Vec<f64> = samples
            .get(&id)
            .map(|blocks| blocks.values().copied().collect())
            .unwrap_or_default();
        let iterations = number(&m["iterations"], &cell.lane)?;
        let valid = xs.len() == required && !failed.contains(&id) && gate["eligible"] == true;
        let mut row = cell.info();
        row["samples"] = json!(xs.len());
        row["iterations"] = json!(iterations);
        row["valid_exploratory_samples"] = json!(valid);
        if valid {
            let ci = median_ci(
                &xs,
                number(m, "bootstrap_seed")?,
                number(m, "bootstrap_resamples")? as usize,
            );
            let per_iteration = median(&xs);
            let block = per_iteration * iterations as f64;
            row["per_iteration_median_ms"] = json!(per_iteration);
            row["block_median_ms"] = json!(block);
            row["per_iteration_p95_ms"] = json!(p95(&xs));
            row["per_iteration_ci95"] = json!(ci);
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.4} | {:.5} | {:.5} | [{:.5}, {:.5}] |\n",
                cell.workload,
                cell.bars,
                cell.lane,
                xs.len(),
                iterations,
                block,
                per_iteration,
                p95(&xs),
                ci[0],
                ci[1]
            ));
        } else {
            let reason = untimed_reason(gate, diagnostics.contains(&cell.workload));
            row["reason"] = json!(reason);
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | — | — | — | {} |\n",
                cell.workload,
                cell.bars,
                cell.lane,
                xs.len(),
                iterations,
                reason
            ));
        }
        summary.push(row);
    }
    if folder.join("artifacts.json").exists() {
        md.push_str("\n## Artifact size\n\n| Workload | Uncompressed bytes | Compressed bytes | Compression |\n|---|---:|---:|---|\n");
        for record in read(&folder.join("artifacts.json"))?
            .as_array()
            .ok_or("artifact rows missing")?
        {
            let workload = record["workload"].as_str().unwrap_or("?");
            let observation = &record["observation"];
            let artifact = &observation["artifact"];
            // A timed-out or crashed invocation never sets `artifact`, so
            // formatting its byte counts unconditionally would print the word
            // `null` twice and read as a malformed row instead of a recorded
            // failure. The row states what happened instead.
            if observation["status"] == "ok" && artifact.is_object() {
                md.push_str(&format!(
                    "| {workload} | {} | {} | {} |\n",
                    artifact["uncompressed_bytes"],
                    artifact["compressed_bytes"],
                    artifact["compression"].as_str().unwrap_or("—"),
                ));
            } else {
                md.push_str(&format!(
                    "| {workload} | — | — | not measured: {} |\n",
                    artifact_failure(observation)
                ));
            }
        }
    }
    if folder.join("memory.json").exists() {
        let mut memory_samples: BTreeMap<String, BTreeMap<u64, u64>> = BTreeMap::new();
        let mut memory_blocks: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        let mut memory_failures: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let records = read(&folder.join("memory.json"))?;
        for record in records.as_array().ok_or("peak-memory rows missing")? {
            let id = record["cell"]["id"]
                .as_str()
                .ok_or("peak-memory cell id missing")?;
            if record["cell"]["lane"] != MEMORY_LANE {
                return Err("peak-memory record has the wrong lane".into());
            }
            let bars = number(&record["cell"], "bars")? as usize;
            let workload = record["cell"]["workload"]
                .as_str()
                .ok_or("peak-memory workload missing")?;
            if !strings(m, "workloads")?
                .iter()
                .any(|declared| declared == workload)
                || !sizes(m)?.contains(&bars)
            {
                return Err("peak-memory record names an undeclared workload or bar count".into());
            }
            let expected_id = Cell {
                workload: workload.to_owned(),
                bars,
                lane: MEMORY_LANE.to_owned(),
            }
            .id();
            if id != expected_id {
                return Err("peak-memory cell id differs from its workload, bars, or lane".into());
            }
            let block = number(record, "block")?;
            if block >= required as u64 {
                return Err("peak-memory sample block is outside the declared domain".into());
            }
            if !memory_blocks
                .entry(id.to_owned())
                .or_default()
                .insert(block)
            {
                return Err("duplicate peak-memory sample block".into());
            }
            let observation = &record["observation"];
            if observation["status"] != "ok" || observation["correctness"]["pass"] != true {
                let status = observation["status"].as_str().unwrap_or("unknown status");
                let reason = observation["message"].as_str().map_or_else(
                    || {
                        if observation["status"] == "ok" {
                            "numeric mismatch".to_owned()
                        } else {
                            status.to_owned()
                        }
                    },
                    |message| format!("{status}: {message}"),
                );
                memory_failures
                    .entry(id.to_owned())
                    .or_default()
                    .insert(reason);
                continue;
            }
            let path = verified_output_path(folder, observation, "peak-memory")?;
            if check_result(
                m,
                &read(&path)?,
                &read(&folder.join(format!("inputs/{bars}.json")))?["expected"][workload],
            )["pass"]
                != true
            {
                return Err("a stored passing peak-memory result is not reproducible".into());
            }
            let platform = observation["memory"]["platform"]
                .as_str()
                .ok_or("peak-memory platform missing")?;
            if Some(platform) != provenance_platform {
                return Err(
                    "peak-memory platform differs from the captured host provenance".into(),
                );
            }
            let registered = &m["memory_lane"]["platforms"][platform];
            if !registered.is_object()
                || observation["memory"]["sampler"] != registered["sampler"]
                || observation["memory"]["field"] != registered["field"]
                || observation["memory"]["source_units_to_bytes"] != registered["bytes_per_unit"]
                || observation["memory"]["cadence"] != m["memory_lane"]["cadence"]
            {
                return Err("peak-memory sampler metadata differs from the manifest".into());
            }
            let raw = observation["memory"]["raw_output"]
                .as_str()
                .ok_or("peak-memory raw sampler output missing")?;
            if observation["memory"]["raw_output_sha256"] != sha256(raw.as_bytes()) {
                return Err("peak-memory raw sampler output hash mismatch".into());
            }
            let bytes = observation["memory"]["peak_rss_bytes"]
                .as_u64()
                .ok_or("peak resident-set byte count missing")?;
            let multiplier = observation["memory"]["source_units_to_bytes"]
                .as_u64()
                .ok_or("peak resident-set unit multiplier missing")?;
            let field = observation["memory"]["field"]
                .as_str()
                .ok_or("peak resident-set source field missing")?;
            if parse_peak_rss(raw, field, multiplier)? != bytes {
                return Err("peak resident-set byte count differs from raw sampler output".into());
            }
            if memory_samples
                .entry(id.to_owned())
                .or_default()
                .insert(block, bytes)
                .is_some()
            {
                return Err("duplicate peak-memory sample block".into());
            }
        }
        md.push_str("\n## Peak process memory\n\nEach observation is the operating system's high-water resident set for the named cold-process boundary. The kernel updates the counter as the process runs; there is no polling interval.\n\n| Workload | Bars | Samples | Median bytes | p95 bytes | Maximum bytes | Status |\n|---|---:|---:|---:|---:|---:|---|\n");
        let mut resource_summary = Vec::new();
        for workload in strings(m, "workloads")? {
            for bars in sizes(m)? {
                let id = format!("{workload}-{bars}-{MEMORY_LANE}");
                let expected_blocks: BTreeSet<u64> = (0..required as u64).collect();
                if memory_blocks.get(&id) != Some(&expected_blocks) {
                    return Err(format!(
                        "peak-memory cell {id} does not contain the exact declared block domain"
                    ));
                }
                let values: Vec<u64> = memory_samples
                    .get(&id)
                    .map(|blocks| blocks.values().copied().collect())
                    .unwrap_or_default();
                let cold_id = format!("{workload}-{bars}-cold-process");
                let eligible = gates
                    .as_array()
                    .ok_or("gates missing")?
                    .iter()
                    .find(|gate| gate["id"] == cold_id)
                    .is_some_and(|gate| gate["eligible"] == true);
                let valid = values.len() == required && eligible;
                let mut row = json!({
                    "id": id,
                    "workload": workload,
                    "bars": bars,
                    "samples": values.len(),
                    "valid_exploratory_samples": valid,
                });
                if valid {
                    let readings: Vec<f64> = values.iter().map(|value| *value as f64).collect();
                    let median_bytes = median(&readings) as u64;
                    let p95_bytes = p95(&readings) as u64;
                    let maximum_bytes = values.iter().copied().max().unwrap_or_default();
                    row["median_bytes"] = json!(median_bytes);
                    row["p95_bytes"] = json!(p95_bytes);
                    row["maximum_bytes"] = json!(maximum_bytes);
                    md.push_str(&format!(
                        "| {workload} | {bars} | {} | {median_bytes} | {p95_bytes} | {maximum_bytes} | exploratory |\n",
                        values.len()
                    ));
                } else {
                    let status = if !eligible {
                        "not measured: correctness gate ineligible".to_owned()
                    } else if let Some(reasons) = memory_failures.get(&id) {
                        format!(
                            "not measured: {}",
                            reasons.iter().cloned().collect::<Vec<_>>().join("; ")
                        )
                    } else {
                        "not measured: incomplete samples".to_owned()
                    };
                    row["reason"] = json!(status);
                    md.push_str(&format!(
                        "| {workload} | {bars} | {} | — | — | — | {status} |\n",
                        values.len()
                    ));
                }
                resource_summary.push(row);
            }
        }
        write(
            &folder.join("memory-summary.json"),
            &json!(resource_summary),
        )?;
    }
    md.push_str("\n## Scope and limitations\n\n");
    for limitation in strings(m, "limitations")? {
        md.push_str(&format!("- {limitation}\n"));
    }
    write(&folder.join("summary.json"), &json!(summary))?;
    fs::write(folder.join("report.md"), md).map_err(|e| e.to_string())?;
    println!("Report: {}", folder.join("report.md").display());
    Ok(())
}

/// How an artifact measurement failed, in one phrase a report row can carry.
fn artifact_failure(observation: &Value) -> String {
    let status = observation["status"].as_str().unwrap_or("unknown status");
    match observation["message"].as_str() {
        Some(message) => format!("{status}: {message}"),
        None => status.to_owned(),
    }
}

fn provenance(h: &Harness<'_>) -> TaskResult<Value> {
    let (root, m, subject) = (h.root, h.manifest, h.subject);
    let mut files = BTreeMap::new();
    for relative in [
        MANIFEST,
        "crates/pinewasm_xtask/src/benchmark_subject.rs",
        "crates/pinewasm_xtask/src/benchmark_run.rs",
        "crates/pinewasm_compiler/src/bin/pinewasm_benchmark_subject.rs",
        "Cargo.lock",
    ] {
        files.insert(relative.to_owned(), hash(&root.join(relative))?);
    }
    files.insert("subject_binary".into(), hash(subject)?);
    for workload in strings(m, "workloads")? {
        let relative = format!("benchmarks/workloads/{workload}.pine");
        files.insert(relative.clone(), hash(&root.join(relative))?);
    }
    for bars in sizes(m)? {
        let relative = format!("benchmark-data/{bars}.json");
        files.insert(relative.clone(), hash(&target_root(root).join(&relative))?);
    }
    // The host identity comes from the shared helper rather than from a local
    // set of macOS-only checks. A reproduction on a stated dedicated runner needs
    // the CPU and power mode recorded, and a bundle whose CPU and power mode are
    // blank because the run happened on Linux cannot support that. `report`
    // refuses a timing run whose snapshot is incomplete.
    let mut snapshot = json!({
        "epoch": epoch(),
        "runner_commit": git(root, &["rev-parse", "HEAD"])?.trim(),
        "dirty": git(root, &["status", "--porcelain"])?,
        "sha256": files,
        "rustc": command_text("rustc", &["--version"]),
        "cargo": command_text("cargo", &["--version"]),
        "profile": "release",
    });
    merge_hardware(&mut snapshot, hardware_profile());
    Ok(snapshot)
}

/// The correctness check ordinary shared runners are allowed to run.
///
/// It builds an unoptimized subject on purpose. Nothing here is timed, so a
/// release build would only buy the runner a slower job, and timing on a
/// shared runner would not be a valid measurement.
fn smoke(root: &Path, m: &Value) -> TaskResult {
    let bars = number(m, "smoke_bars")? as usize;
    prepare(root, &[bars])?;
    let subject = subject_binary(root, false)?;
    let harness = Harness {
        root,
        manifest: m,
        subject: &subject,
    };
    let temp = crate::command::temp_dir("benchmark-subject-smoke")?;
    let folder = temp.path();
    let mut failures = Vec::new();
    for workload in strings(m, "workloads")? {
        for lane in strings(m, "lanes")?
            .into_iter()
            .chain([ARTIFACT_LANE.to_string()])
        {
            let cell = Cell {
                workload: workload.clone(),
                bars,
                lane: lane.clone(),
            };
            let observation = invoke(
                &harness,
                &cell,
                &folder.join("invocations"),
                &format!("smoke-{}", cell.id()),
                0,
                1,
            )?;
            if observation["status"] != "ok" {
                failures.push(format!("{}: {}", cell.id(), observation));
                continue;
            }
            let correctness = check_result(m, &observation["result"], &expected(&harness, &cell)?);
            if correctness["pass"] != true {
                // A divergence usually repeats on every later bar. Print enough
                // to identify where the series parted from the expected series and
                // leave the complete matrix to `probe`, so one broken workload
                // cannot bury the rest of the gate's output.
                let differences: Vec<&str> = correctness["first_differences"]
                    .as_array()
                    .map(|items| items.iter().filter_map(Value::as_str).take(3).collect())
                    .unwrap_or_default();
                failures.push(format!("{}: {}", cell.id(), differences.join("; ")));
            }
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "subject-tier correctness gate failed for {} cell(s):\n  {}",
            failures.len(),
            failures.join("\n  ")
        ));
    }
    println!(
        "Subject-tier correctness gate: {} workloads x {} boundaries at {bars} bars agree with the expected series.",
        strings(m, "workloads")?.len(),
        strings(m, "lanes")?.len() + 1
    );
    Ok(())
}

pub(crate) fn run(args: &[String]) -> TaskResult {
    let root = repo_root()?;
    let m = manifest(&root)?;
    let command = args.first().map_or("help", String::as_str);
    match command {
        "smoke" => return smoke(&root, &m),
        "prepare" => {
            prepare(&root, &sizes(&m)?)?;
            println!("Prepared deterministic inputs and expected series.");
            return Ok(());
        }
        "archive" => {
            let source = args.get(1).ok_or("run directory required")?;
            let destination = args.get(2).ok_or("archive destination required")?;
            return archive(Path::new(source), Path::new(destination));
        }
        "unpack" => {
            let source = args.get(1).ok_or("archive required")?;
            let destination = args.get(2).ok_or("destination directory required")?;
            unpack_archive(Path::new(source), Path::new(destination))?;
            return report(Path::new(destination));
        }
        _ => {}
    }
    let folder = args.get(1).map_or_else(
        || target_root(&root).join(format!("benchmark-subject-runs/{}", epoch())),
        PathBuf::from,
    );
    if command == "report" {
        return report(&folder);
    }
    if !["probe", "run"].contains(&command) {
        return Err(
            "usage: cargo xtask benchmark-subject smoke | prepare | probe DIR | run DIR | report DIR | archive DIR FILE | unpack FILE DIR"
                .into(),
        );
    }
    if folder.exists() {
        return Err(format!("refusing to overwrite {}", folder.display()));
    }
    // A timing run has to be attributable to an exact committed revision.
    if command == "run" && !git(&root, &["status", "--porcelain"])?.trim().is_empty() {
        return Err("commit the manifest, runner, and subject before timing".into());
    }
    prepare(&root, &sizes(&m)?)?;
    let subject = subject_binary(&root, true)?;
    let harness = Harness {
        root: &root,
        manifest: &m,
        subject: &subject,
    };
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    write(&folder.join("manifest.json"), &m)?;
    write(&folder.join("samples.json"), &json!([]))?;
    let before = provenance(&harness)?;
    write(&folder.join("environment-before.json"), &before)?;
    for bars in sizes(&m)? {
        let input = read(&target_root(&root).join(format!("benchmark-data/{bars}.json")))?;
        write(&folder.join(format!("inputs/{bars}.json")), &input)?;
    }
    let gates = probe(&harness, &folder)?;
    write(&folder.join("gates.json"), &json!(gates))?;
    measure_artifacts(&harness, &folder)?;
    if command == "probe" {
        measure_memory(&harness, &folder, 1)?;
        return Ok(());
    }
    let mut selected: Vec<_> = cells(&m)?
        .into_iter()
        .filter(|cell| {
            gates
                .iter()
                .any(|gate| gate["id"] == cell.id() && gate["eligible"] == true)
        })
        .collect();
    let mut rng = ChaCha8Rng::seed_from_u64(number(&m, "order_seed")?);
    let mut raw = Vec::new();
    for block in 0..number(&m, "samples")? {
        shuffled(&mut selected, &mut rng);
        for cell in &selected {
            if folder.join("stop-request.json").exists() {
                let request = read(&folder.join("stop-request.json"))?;
                write(
                    &folder.join("disposition.json"),
                    &json!({"status":"interrupted","reason":request["reason"],"epoch":epoch(),"completed_samples":raw.len()}),
                )?;
                write(
                    &folder.join("environment-after.json"),
                    &provenance(&harness)?,
                )?;
                return report(&folder);
            }
            let warmups = number(&m["warmups"], &cell.lane)?;
            let iterations = number(&m["iterations"], &cell.lane)?;
            let mut observation = invoke(
                &harness,
                cell,
                &folder.join("invocations"),
                &format!("block-{block}-{}", cell.id()),
                warmups,
                iterations,
            )?;
            if observation["status"] == "ok" {
                observation["correctness"] =
                    check_result(&m, &observation["result"], &expected(&harness, cell)?);
            }
            preserve_output(&folder, &mut observation)?;
            raw.push(json!({"block":block,"cell":cell.info(),"observation":observation}));
            write(&folder.join("samples.json"), &json!(raw))?;
        }
        println!(
            "Completed randomized block {}/{} ({} cells)",
            block + 1,
            number(&m, "samples")?,
            selected.len()
        );
    }
    if !measure_memory(&harness, &folder, number(&m["memory_lane"], "samples")?)? {
        let request = read(&folder.join("stop-request.json"))?;
        let completed_memory_samples = read(&folder.join("memory.json"))?
            .as_array()
            .ok_or("peak-memory rows missing")?
            .len();
        write(
            &folder.join("disposition.json"),
            &json!({
                "status":"interrupted",
                "reason":request["reason"],
                "epoch":epoch(),
                "completed_samples":raw.len(),
                "completed_memory_samples":completed_memory_samples,
            }),
        )?;
        write(
            &folder.join("environment-after.json"),
            &provenance(&harness)?,
        )?;
        return report(&folder);
    }
    let after = provenance(&harness)?;
    write(&folder.join("environment-after.json"), &after)?;
    if before["sha256"] != after["sha256"] {
        return Err("an input, workload, or subject binary changed during the run".into());
    }
    report(&folder)
}

/// Validate the manifest without building or measuring anything. Wired into
/// `cargo xtask check` so a manifest that no longer closes over the
/// implemented lanes fails locally and in CI, not during a measurement window.
pub(crate) fn check(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("usage: cargo xtask benchmark-subject-check".into());
    }
    let root = repo_root()?;
    let m = manifest(&root)?;
    for workload in strings(&m, "workloads")? {
        let path = root.join(format!("benchmarks/workloads/{workload}.pine"));
        if !path.is_file() {
            return Err(format!(
                "declared workload has no source: {}",
                path.display()
            ));
        }
    }
    println!(
        "Subject-tier manifest closes over {} workloads, {} timing lanes, one artifact measurement, and one peak-memory measurement.",
        strings(&m, "workloads")?.len(),
        strings(&m, "lanes")?.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These positive fixtures test exit-status and memory-report correctness,
    // not startup performance. Cold executable checks on macOS can take more
    // than two seconds even for a tiny shell script. Keep a bounded allowance
    // here without changing any benchmark manifest or production timeout.
    const FIXTURE_PROCESS_TIMEOUT_SECONDS: u64 = 30;

    /// Validate a hypothetical manifest through the real entry point, from a
    /// throwaway checkout root, so a test never rewrites the committed file.
    fn folder_backed(value: &Value) -> TaskResult<Value> {
        let temp = crate::command::temp_dir("subject-manifest").unwrap();
        let path = temp.path().join(MANIFEST);
        write(&path, value)?;
        manifest(temp.path())
    }

    /// Write an executable shell fixture without ever holding a writable
    /// descriptor to it in this process. Tests run on parallel threads, and a
    /// sibling's `fork` inherits every open descriptor until its `exec`; if the
    /// fixture's write handle is among them, executing the fixture fails with
    /// ETXTBSY ("Text file busy") on Linux. A child `sh` owns the write instead.
    #[cfg(unix)]
    fn write_executable_fixture(path: &Path, contents: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "failed to write {path:?}");
    }

    #[cfg(unix)]
    #[test]
    fn failed_process_cannot_publish_a_successful_response() {
        let temp = crate::command::temp_dir("subject-exit-status").unwrap();
        let root = temp.path();
        let subject = root.join("fake-subject");
        let m = json!({"timeout_seconds": FIXTURE_PROCESS_TIMEOUT_SECONDS});
        let cell = Cell {
            workload: "history".into(),
            bars: 2000,
            lane: "cached".into(),
        };
        for code in [0, 7] {
            write_executable_fixture(&subject, &format!("#!/bin/sh\nprintf '%s' '{{\"status\":\"ok\",\"elapsed_ms\":1}}' > \"$2\"\nexit {code}\n"));
            let harness = Harness {
                root,
                manifest: &m,
                subject: &subject,
            };
            let observation = invoke(
                &harness,
                &cell,
                &root.join("invocations"),
                &format!("exit-{code}"),
                0,
                1,
            )
            .unwrap();
            if code == 0 {
                assert_eq!(observation["status"], "ok");
            } else {
                assert_eq!(observation["status"], "crashed");
                assert_eq!(observation["exit_code"], 7);
                assert!(observation.get("elapsed_ms").is_none());
            }
        }
    }

    #[test]
    fn manifest_rejects_lanes_the_runner_cannot_enter() {
        let root = repo_root().expect("repository root");
        let committed = manifest(&root).expect("committed manifest is valid");
        // Every declared lane must be one the runner implements, or a run
        // would silently skip cells the manifest declares.
        let mut drifted = committed.clone();
        drifted["lanes"] = json!(["cold-runtime", "hosted-aot"]);
        assert!(folder_backed(&drifted).is_err());
        let mut missing_iterations = committed.clone();
        missing_iterations["iterations"] = json!({"cold-runtime": 1});
        assert!(folder_backed(&missing_iterations).is_err());
        let mut repeated_process = committed.clone();
        repeated_process["iterations"]["cold-process"] = json!(4);
        assert!(folder_backed(&repeated_process).is_err());
        let mut implemented_but_denied = committed.clone();
        implemented_but_denied["unimplemented_lanes"] = json!({"cached": "reason"});
        assert!(folder_backed(&implemented_but_denied).is_err());
        // The register is closed the other way too. `docs/benchmarks.md` tells
        // a reader that the three boundaries this tier does not measure are
        // named here with their reasons, so emptying it — or quietly dropping
        // one, or leaving a reason blank — has to fail rather than let the
        // methodology describe a register that no longer exists.
        let mut emptied = committed.clone();
        emptied["unimplemented_lanes"] = json!({});
        assert!(folder_backed(&emptied).is_err());
        let mut dropped = committed.clone();
        dropped["unimplemented_lanes"]
            .as_object_mut()
            .expect("register is an object")
            .remove("hosted-aot");
        assert!(folder_backed(&dropped).is_err());
        let mut unexplained = committed;
        unexplained["unimplemented_lanes"]["standalone-aot"] = json!("");
        assert!(folder_backed(&unexplained).is_err());
    }

    #[test]
    fn manifest_pins_the_peak_memory_boundary_and_method() {
        let root = repo_root().expect("repository root");
        let committed = manifest(&root).expect("committed manifest is valid");
        for (key, value) in [
            ("measured_boundary", json!("cached")),
            ("samples", json!(1)),
            ("sampler", json!("")),
            ("cadence", json!("")),
        ] {
            let mut drifted = committed.clone();
            drifted["memory_lane"][key] = value;
            assert!(folder_backed(&drifted).is_err(), "drifted key {key}");
        }
    }

    #[test]
    fn peak_rss_parser_normalizes_macos_and_linux_units() {
        let macos = "  0.01 real  0.00 user  0.00 sys\n  1277952  maximum resident set size\n";
        assert_eq!(
            parse_peak_rss(macos, "maximum resident set size", 1).unwrap(),
            1_277_952
        );
        let linux = "Command being timed: x\nMaximum resident set size (kbytes): 12345\n";
        assert_eq!(
            parse_peak_rss(linux, "Maximum resident set size (kbytes)", 1024).unwrap(),
            12_641_280
        );
        assert!(parse_peak_rss("no resource field", "maximum resident set size", 1).is_err());
    }

    #[test]
    fn manifest_rejects_bar_counts_the_runner_or_the_archive_cannot_use() {
        let root = repo_root().expect("repository root");
        let committed = manifest(&root).expect("committed manifest is valid");
        // Each of these passed every other check, so a run would spend a full
        // measurement window before the problem surfaced: the first two cannot
        // be archived at all, the second also exceeds the incremental
        // session's resident-bar ceiling, the third measures nothing while
        // still reporting success, and the fourth would record two blocks per
        // sample for one cell.
        for bars in [
            json!([1000, 5000]),
            json!([2000, 50000]),
            json!([]),
            json!([2000, 2000]),
        ] {
            let mut drifted = committed.clone();
            drifted["bars"] = bars.clone();
            assert!(
                folder_backed(&drifted).is_err(),
                "bar counts {bars} must be rejected"
            );
        }
        assert!(folder_backed(&committed).is_ok());
    }

    #[test]
    fn median_interval_brackets_the_point_estimate() {
        // A constant sample set has no spread to resample, so the interval
        // must collapse onto the median rather than drift.
        assert_eq!(median_ci(&[4.0; 30], 11, 1000), [4.0, 4.0]);
        let spread: Vec<f64> = (1..=30).map(f64::from).collect();
        let interval = median_ci(&spread, 11, 1000);
        let point = median(&spread);
        assert!(interval[0] <= point && point <= interval[1]);
        assert!(interval[0] >= 1.0 && interval[1] <= 30.0);
    }

    #[test]
    fn untimed_reasons_separate_diagnostic_from_failure() {
        let gate = |eligible: bool, status: &str, pass: bool| {
            json!({
                "eligible": eligible,
                "observation": {"status": status, "correctness": {"pass": pass}},
            })
        };
        assert_eq!(
            untimed_reason(&gate(true, "ok", true), false),
            "incomplete or below-floor samples"
        );
        assert_eq!(
            untimed_reason(&gate(false, "ok", true), true),
            "diagnostic only"
        );
        assert_eq!(
            untimed_reason(&gate(false, "ok", false), false),
            "numeric mismatch or failed invocation"
        );
        // A case the report could render as an intentional exclusion: a
        // diagnostic workload is ineligible either way, so its divergence has
        // to be named or the bundle's narrative loses a failure `gates.json`
        // still holds.
        assert_eq!(
            untimed_reason(&gate(false, "ok", false), true),
            "diagnostic, numeric mismatch or failed invocation"
        );
        // A timed-out invocation never reaches the comparison, so no
        // correctness verdict exists to read.
        assert_eq!(
            untimed_reason(
                &json!({"eligible": false, "observation": {"status": "timeout"}}),
                true
            ),
            "diagnostic, numeric mismatch or failed invocation"
        );
    }

    #[test]
    fn artifact_rows_state_a_failed_measurement_instead_of_printing_null() {
        let temp = crate::command::temp_dir("subject-artifact-rows").unwrap();
        let folder = temp.path();
        let m = json!({
            "workloads": ["history", "collections"],
            "bars": [2000],
            "lanes": ["cached"],
            "iterations": {"cached": 10},
            "samples": 30,
            "bootstrap_seed": 1,
            "bootstrap_resamples": 1000,
            "diagnostic_workloads": [],
            "limitations": ["Exploratory only."],
        });
        let gates = json!([
            {"id": "history-2000-cached", "eligible": true, "observation": {"status": "ok", "correctness": {"pass": true}}},
            {"id": "collections-2000-cached", "eligible": true, "observation": {"status": "ok", "correctness": {"pass": true}}},
        ]);
        write(
            &folder.join("artifacts.json"),
            &json!([
                {"workload": "history", "observation": {"status": "ok", "artifact": {
                    "uncompressed_bytes": 4096,
                    "compressed_bytes": 512,
                    "compression": "gzip-deflate-default",
                }}},
                // A stalled child is killed without ever writing a response, so
                // its record has no `artifact` object at all.
                {"workload": "collections", "observation": {"status": "timeout"}},
            ]),
        )
        .unwrap();
        write_report(folder, &m, &gates, &BTreeMap::new(), &BTreeSet::new(), None).unwrap();
        let report = fs::read_to_string(folder.join("report.md")).unwrap();
        assert!(
            report.contains("| history | 4096 | 512 | gzip-deflate-default |"),
            "{report}"
        );
        assert!(
            report.contains("| collections | — | — | not measured: timeout |"),
            "{report}"
        );
        // A row that formatted an absent measurement would print this instead.
        assert!(!report.contains("null"), "{report}");
    }

    #[test]
    fn peak_memory_report_requires_distinct_blocks_and_registered_metadata() {
        let temp = crate::command::temp_dir("subject-memory-rows").unwrap();
        let folder = temp.path();
        let result = json!({"bars": 1, "plots": []});
        let m = json!({
            "workloads": ["history"],
            "bars": [2000],
            "lanes": ["cached"],
            "iterations": {"cached": 1},
            "samples": 30,
            "bootstrap_seed": 1,
            "bootstrap_resamples": 1000,
            "diagnostic_workloads": [],
            "limitations": ["Exploratory only."],
            "absolute_tolerance": 0.0,
            "relative_tolerance": 0.0,
            "memory_lane": {
                "cadence": MEMORY_CADENCE,
                "platforms": {"macos": {
                    "sampler": "/usr/bin/time -l",
                    "field": "maximum resident set size",
                    "bytes_per_unit": 1,
                }},
            },
        });
        let gates = json!([
            {"id":"history-2000-cached","eligible":true,"observation":{"status":"ok","correctness":{"pass":true}}},
            {"id":"history-2000-cold-process","eligible":true,"observation":{"status":"ok","correctness":{"pass":true}}},
        ]);
        write(
            &folder.join("inputs/2000.json"),
            &json!({"expected":{"history":result}}),
        )
        .unwrap();
        let mut rows = Vec::new();
        for block in 0..30 {
            let peak_rss_bytes = 1_000_000 + block;
            let raw = format!("{peak_rss_bytes} maximum resident set size\n");
            let mut observation = json!({
                "status":"ok",
                "correctness":{"pass":true},
                "result":result,
                "memory":{
                    "peak_rss_bytes": peak_rss_bytes,
                    "platform":"macos",
                    "sampler":"/usr/bin/time -l",
                    "field":"maximum resident set size",
                    "source_units_to_bytes":1,
                    "cadence":MEMORY_CADENCE,
                    "raw_output":raw,
                    "raw_output_sha256":sha256(raw.as_bytes()),
                },
            });
            preserve_output(folder, &mut observation).unwrap();
            rows.push(json!({
                "block":block,
                "cell":{"id":"history-2000-peak-memory","workload":"history","bars":2000,"lane":"peak-memory"},
                "observation":observation,
            }));
        }
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos"),
        )
        .unwrap();
        let summary = read(&folder.join("memory-summary.json")).unwrap();
        assert_eq!(summary[0]["samples"], 30);
        assert_eq!(summary[0]["valid_exploratory_samples"], true);
        assert_eq!(summary[0]["maximum_bytes"], 1_000_029);

        rows[0]["observation"]["memory"]["peak_rss_bytes"] = json!(9_999_999);
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("differs from raw sampler output"));
        rows[0]["observation"]["memory"]["peak_rss_bytes"] = json!(1_000_000);
        rows[0]["observation"]["memory"]["raw_output"] =
            json!("1000001 maximum resident set size\n");
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("raw sampler output hash mismatch"));
        rows[0]["observation"]["memory"]["raw_output"] =
            json!("1000000 maximum resident set size\n");

        rows[0]["cell"]["id"] = json!("another-2000-peak-memory");
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("cell id differs"));
        rows[0]["cell"]["id"] = json!("history-2000-peak-memory");

        rows[29]["block"] = json!(30);
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("outside the declared domain"));
        rows[29]["block"] = json!(29);

        let original_path = rows[0]["observation"]["output_path"].clone();
        rows[0]["observation"]["output_path"] = json!("/tmp/external-output.json");
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("output path is not canonical"));
        rows[0]["observation"]["output_path"] = original_path;

        let original_digest = rows[0]["observation"]["output_sha256"].clone();
        rows[0]["observation"]["output_sha256"] = json!("0".repeat(64));
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("output path is not canonical or digest-bound"));
        rows[0]["observation"]["output_sha256"] = original_digest;

        rows[0]["observation"]["memory"]["platform"] = json!("linux");
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("differs from the captured host provenance"));
        rows[0]["observation"]["memory"]["platform"] = json!("macos");

        rows[29]["block"] = json!(0);
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        assert!(write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos")
        )
        .unwrap_err()
        .contains("duplicate peak-memory sample block"));

        rows[29]["block"] = json!(29);
        rows[29]["observation"] = json!({
            "status":"unavailable",
            "message":"process accounting is not registered for windows",
        });
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        write_report(
            folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some("macos"),
        )
        .unwrap();
        let report = fs::read_to_string(folder.join("report.md")).unwrap();
        assert!(
            report.contains("unavailable: process accounting is not registered for windows"),
            "{report}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn supported_peak_memory_collection_produces_a_full_report() {
        let temp = crate::command::temp_dir("subject-memory-full-report").unwrap();
        let root = temp.path();
        let folder = root.join("run");
        // This fixture must be executed by `/usr/bin/time`. Keep it beside
        // Cargo's executable test binary so it does not depend on the system
        // temporary directory's mount policy.
        let test_binary_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .expect("test binary has a parent directory")
            .to_path_buf();
        let executable_temp =
            crate::command::temp_dir_in(&test_binary_dir, "subject-memory-executable").unwrap();
        let subject = executable_temp.path().join("fake-subject");
        write_executable_fixture(
            &subject,
            "#!/usr/bin/env sh\nprintf '%s' '{\"status\":\"ok\",\"result\":{\"bars\":1,\"plots\":[]}}' > \"$2\"\n",
        );
        let committed = manifest(&repo_root().unwrap()).unwrap();
        let m = json!({
            "workloads": ["history"],
            "bars": [2000],
            "lanes": ["cached"],
            "iterations": {"cached": 1},
            "samples": 30,
            "bootstrap_seed": 1,
            "bootstrap_resamples": 1000,
            "diagnostic_workloads": [],
            "limitations": ["Exploratory only."],
            "absolute_tolerance": 0.0,
            "relative_tolerance": 0.0,
            "timeout_seconds": FIXTURE_PROCESS_TIMEOUT_SECONDS,
            "memory_lane": committed["memory_lane"],
        });
        let harness = Harness {
            root,
            manifest: &m,
            subject: &subject,
        };
        let cell = Cell {
            workload: "history".into(),
            bars: 2000,
            lane: MEMORY_LANE.into(),
        };
        let result = json!({"bars":1,"plots":[]});
        write(
            &folder.join("inputs/2000.json"),
            &json!({"expected":{"history":result}}),
        )
        .unwrap();
        let mut rows = Vec::new();
        for block in 0..30 {
            let block_label = format!("block-{block}");
            let mut observation =
                invoke_memory(&harness, &cell, &folder.join("invocations"), &block_label).unwrap();
            assert_eq!(
                observation["status"],
                "ok",
                "{observation}\ninvocation log: {}\nsampler output: {}",
                fs::read_to_string(
                    folder
                        .join("invocations")
                        .join(format!("{block_label}.log"))
                )
                .unwrap_or_else(|error| format!("<unavailable: {error}>")),
                fs::read_to_string(
                    folder
                        .join("invocations")
                        .join(format!("{block_label}.sampler.txt"))
                )
                .unwrap_or_else(|error| format!("<unavailable: {error}>")),
            );
            observation["correctness"] = json!({"pass":true});
            preserve_output(&folder, &mut observation).unwrap();
            rows.push(json!({"block":block,"cell":cell.info(),"observation":observation}));
        }
        write(&folder.join("memory.json"), &json!(rows)).unwrap();
        let gates = json!([
            {"id":"history-2000-cached","eligible":true,"observation":{"status":"ok","correctness":{"pass":true}}},
            {"id":"history-2000-cold-process","eligible":true,"observation":{"status":"ok","correctness":{"pass":true}}},
        ]);
        write_report(
            &folder,
            &m,
            &gates,
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some(std::env::consts::OS),
        )
        .unwrap();
        let summary = read(&folder.join("memory-summary.json")).unwrap();
        assert_eq!(summary[0]["samples"], 30);
        assert_eq!(summary[0]["valid_exploratory_samples"], true);
        assert!(summary[0]["median_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn a_timing_report_refuses_a_provenance_snapshot_with_blank_hardware() {
        let temp = crate::command::temp_dir("subject-provenance").unwrap();
        let folder = temp.path();
        // Two snapshots whose hardware fields are equally blank compare equal,
        // so the before/after consistency check cannot notice them. Without a
        // completeness flag this run would be reported and archived with no
        // CPU and no power mode, so the report would not say what it measured.
        let snapshot = |complete: bool| {
            json!({
                "sha256": {"Cargo.lock": "abc"},
                "runner_commit": "0123456789abcdef0123456789abcdef01234567",
                "dirty": "",
                "cpu": "",
                "memory_bytes": "",
                "logical_cpus": "",
                "power": "",
                "provenance_complete": complete,
            })
        };
        for (name, value) in [
            ("manifest.json", json!({})),
            ("samples.json", json!([])),
            ("gates.json", json!([])),
        ] {
            write(&folder.join(name), &value).unwrap();
        }
        for complete in [false, true] {
            write(&folder.join("environment-before.json"), &snapshot(complete)).unwrap();
            write(&folder.join("environment-after.json"), &snapshot(complete)).unwrap();
            let error = report(folder).expect_err("this fixture never reaches a report");
            assert_eq!(
                error.contains("provenance is incomplete"),
                !complete,
                "complete={complete}: {error}"
            );
        }
    }

    #[test]
    fn interrupted_run_publishes_no_medians() {
        let temp = crate::command::temp_dir("subject-interruption").unwrap();
        let folder = temp.path();
        for (name, value) in [
            ("manifest.json", json!({})),
            ("samples.json", json!([{"observation": {"elapsed_ms": 1}}])),
            ("gates.json", json!([])),
            (
                "disposition.json",
                json!({"status": "interrupted", "reason": "Concurrent build"}),
            ),
        ] {
            write(&folder.join(name), &value).unwrap();
        }
        report(folder).unwrap();
        let summary = read(&folder.join("summary.json")).unwrap();
        assert_eq!(summary["status"], "interrupted");
        assert_eq!(summary["completed_samples"], 1);
        assert!(summary.get("per_iteration_median_ms").is_none());
    }

    #[test]
    fn artifact_measurement_records_both_sizes_and_a_digest() {
        let temp = crate::command::temp_dir("subject-artifact").unwrap();
        let path = temp.path().join("artifact.bin");
        // Highly compressible bytes make the two counts distinguishable, so a
        // report that quoted one number for both would fail here.
        fs::write(&path, vec![b'a'; 4096]).unwrap();
        let record = artifact_bytes(&path).unwrap();
        assert_eq!(record["uncompressed_bytes"], 4096);
        assert!(record["compressed_bytes"].as_u64().unwrap() < 4096);
        assert_eq!(record["compression"], "gzip-deflate-default");
        assert_eq!(record["sha256"], json!(sha256_file(&path).unwrap()));
    }

    #[test]
    fn cells_cover_every_declared_workload_bar_count_and_lane() {
        let m = json!({
            "workloads": ["sma", "history"],
            "bars": [2000, 10000],
            "lanes": ["cached", "compile"],
        });
        let ids: Vec<String> = cells(&m).unwrap().iter().map(Cell::id).collect();
        assert_eq!(ids.len(), 8);
        assert!(ids.contains(&"sma-10000-compile".to_string()));
        assert!(ids.contains(&"history-2000-cached".to_string()));
    }
}
