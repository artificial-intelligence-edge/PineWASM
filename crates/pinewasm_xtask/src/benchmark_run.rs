//! Local, exploratory measurements of PineWASM and other engines.
//!
//! Its deterministic input generator, computed reference series,
//! tolerant comparison, statistics, and archive schema are shared with the
//! subject tier in `benchmark_subject`. Both tiers therefore check correctness
//! against the same reference values and tolerance rule; only the workloads differ.
use crate::command::{git, repo_root, sha256, sha256_file};
use crate::TaskResult;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MANIFEST: &str = "benchmarks/manifests/node-indicators-v1.json";
const ADAPTER_ENV: &str = "PINEWASM_BENCHMARK_ADAPTER";

fn external_adapter_path(root: &Path, supplied: &Path) -> TaskResult<PathBuf> {
    if !supplied.is_absolute() {
        return Err(format!(
            "{ADAPTER_ENV} must be an absolute path outside this repository"
        ));
    }
    let path = supplied.canonicalize().map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if path.starts_with(&root) || !path.is_file() {
        return Err(format!(
            "{ADAPTER_ENV} must name a file outside this repository"
        ));
    }
    Ok(path)
}

fn external_adapter(root: &Path) -> TaskResult<PathBuf> {
    let supplied = std::env::var_os(ADAPTER_ENV)
        .ok_or_else(|| format!("set {ADAPTER_ENV} to a local benchmark adapter file"))?;
    external_adapter_path(root, Path::new(&supplied))
}

pub(crate) fn read(path: &Path) -> TaskResult<Value> {
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
pub(crate) fn write(path: &Path, value: &Value) -> TaskResult {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, serde_json::to_vec(value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
pub(crate) fn hash(path: &Path) -> TaskResult<String> {
    sha256_file(path)
}
pub(crate) fn number(v: &Value, key: &str) -> TaskResult<u64> {
    v[key]
        .as_u64()
        .ok_or_else(|| format!("missing integer {key}"))
}
pub(crate) fn strings(v: &Value, key: &str) -> TaskResult<Vec<String>> {
    v[key]
        .as_array()
        .ok_or_else(|| format!("missing array {key}"))?
        .iter()
        .map(|x| {
            x.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("non-string {key}"))
        })
        .collect()
}
pub(crate) fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_secs())
}
fn manifest(root: &Path) -> TaskResult<Value> {
    let m = read(&root.join(MANIFEST))?;
    if number(&m, "schema_version")? != 1
        || m["status"] != "exploratory-not-accepted"
        || number(&m, "samples")? < 30
        || number(&m, "samples")? > 1000
        || number(&m, "iterations")? != 1
    {
        return Err("invalid exploratory manifest or sample bounds".into());
    }
    for k in ["engines", "workloads", "lanes"] {
        let names = strings(&m, k)?;
        let set: std::collections::BTreeSet<_> = names.iter().collect();
        if names.is_empty()
            || names.len() != set.len()
            || names
                .iter()
                .any(|n| !n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        {
            return Err(format!("invalid or duplicate identifiers in {k}"));
        }
    }
    for k in [
        "absolute_tolerance",
        "relative_tolerance",
        "precision_floor_ms",
    ] {
        if !m[k].as_f64().is_some_and(|x| x.is_finite() && x >= 0.0) {
            return Err(format!("invalid {k}"));
        }
    }
    if number(&m, "timeout_seconds")? == 0 || number(&m, "timeout_seconds")? > 600 {
        return Err("invalid timeout".into());
    }
    for (key, allowed) in [
        (
            "engines",
            &[
                "pinewasm",
                "pinets",
                "piner-js",
                "piner-interp",
                "resin",
                "pine-a-script",
                "openpine",
            ][..],
        ),
        (
            "workloads",
            &[
                "history",
                "arithmetic",
                "sma",
                "control",
                "persistent",
                "collections",
                "legacy-mixed",
            ][..],
        ),
        ("lanes", &["cold-runtime", "cached"][..]),
    ] {
        if strings(&m, key)?
            .iter()
            .any(|n| !allowed.contains(&n.as_str()))
        {
            return Err(format!("unimplemented {key}"));
        }
    }
    if m["bars"] != json!(ARCHIVED_BAR_COUNTS)
        || m["diagnostic_workloads"] != json!(["legacy-mixed"])
        || number(&m, "bootstrap_resamples")? != 10000
    {
        return Err("invalid fixed suite configuration".into());
    }
    for lane in strings(&m, "lanes")? {
        if m["warmups"][&lane].as_u64().is_none_or(|n| n > 100) {
            return Err("invalid warmup count".into());
        }
    }
    Ok(m)
}

fn setup(root: &Path, m: &Value) -> TaskResult {
    let dir = root.join("target/benchmark-engines");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write(
        &dir.join("package.json"),
        &json!({"dependencies":m["packages"]}),
    )?;
    write(&dir.join("package-lock.json"), &m["npm_lock"])?;
    crate::command::run(Command::new("npm").current_dir(&dir).args([
        "ci",
        "--ignore-scripts",
        "--no-audit",
        "--no-fund",
    ]))?;
    for (name, pin) in m["source_engines"]
        .as_object()
        .ok_or("source pins missing")?
    {
        let checkout = dir.join(name);
        let revision = pin["revision"].as_str().ok_or("revision missing")?;
        if checkout.exists() {
            if git(&checkout, &["rev-parse", "HEAD"])?.trim() != revision
                || !git(&checkout, &["status", "--porcelain"])?
                    .trim()
                    .is_empty()
            {
                return Err(format!("existing source checkout differs: {name}"));
            }
        } else {
            let repository = pin["repository"].as_str().ok_or("repository missing")?;
            crate::command::run(
                Command::new("git")
                    .args([
                        "clone",
                        "--quiet",
                        &format!("https://github.com/{repository}.git"),
                    ])
                    .arg(&checkout),
            )?;
            git(&checkout, &["checkout", "--detach", revision])?;
        }
    }
    Ok(())
}

// Reference calculations computed directly from the formulas. No engine is linked.
fn sma(xs: &[Option<f64>], length: usize) -> Vec<Option<f64>> {
    (0..xs.len())
        .map(|i| {
            if i + 1 < length {
                return None;
            }
            xs[i + 1 - length..=i]
                .iter()
                .copied()
                .collect::<Option<Vec<_>>>()
                .map(|window| window.iter().sum::<f64>() / length as f64)
        })
        .collect()
}
fn smooth(xs: &[Option<f64>], length: usize, alpha: f64) -> Vec<Option<f64>> {
    let mut seed = Vec::new();
    let mut previous = None;
    xs.iter()
        .map(|x| match x {
            None => None,
            Some(x) => {
                if let Some(p) = previous {
                    previous = Some(alpha * x + (1.0 - alpha) * p);
                } else {
                    seed.push(*x);
                    if seed.len() == length {
                        previous = Some(seed.iter().sum::<f64>() / length as f64);
                    }
                }
                previous
            }
        })
        .collect()
}
pub(crate) fn generated(bars: usize) -> Value {
    let (mut open, mut high, mut low, mut close, mut volume, mut time) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let mut prev: f64 = 100.0;
    for i in 0..bars {
        let c = prev + 0.012 + (i as f64 / 29.0).sin() * 0.18 + (i as f64 / 71.0).cos() * 0.09;
        open.push(prev);
        high.push(prev.max(c) + 0.25 + (i % 7) as f64 * 0.01);
        low.push(prev.min(c) - 0.22 - (i % 5) as f64 * 0.01);
        close.push(c);
        volume.push(1000 + (i * 37) % 8000);
        time.push(1_577_836_800_000_u64 + i as u64 * 60_000);
        prev = c;
    }
    let xs: Vec<_> = close.iter().map(|&x| Some(x)).collect();
    let p = |title: &str, values: Vec<Option<f64>>| json!({"title":title,"values":values});
    let out = |plots: Vec<Value>| json!({"bars":bars,"plots":plots});
    let mut cumulative = 0.0;
    let ema12 = smooth(&xs, 12, 2.0 / 13.0);
    let ema26 = smooth(&xs, 26, 2.0 / 27.0);
    let macd: Vec<_> = ema12
        .iter()
        .zip(&ema26)
        .map(|(a, b)| a.zip(*b).map(|(a, b)| a - b))
        .collect();
    let signal = smooth(&macd, 9, 0.2);
    let up: Vec<_> = (0..bars)
        .map(|i| {
            if i == 0 {
                None
            } else {
                Some((close[i] - close[i - 1]).max(0.0))
            }
        })
        .collect();
    let down: Vec<_> = (0..bars)
        .map(|i| {
            if i == 0 {
                None
            } else {
                Some((close[i - 1] - close[i]).max(0.0))
            }
        })
        .collect();
    let rsi = smooth(&up, 14, 1.0 / 14.0)
        .iter()
        .zip(smooth(&down, 14, 1.0 / 14.0))
        .map(|(a, b)| {
            a.zip(b).map(|(a, b)| {
                if b == 0.0 {
                    100.0
                } else {
                    100.0 - 100.0 / (1.0 + a / b)
                }
            })
        })
        .collect();
    let expected = json!({
        "history":out(vec![p("Change",(0..bars).map(|i|if i==0{None}else{Some(close[i]-close[i-1])}).collect()),p("Lag 2",(0..bars).map(|i|Some(if i<2{0.0}else{close[i-2]})).collect())]),
        "arithmetic":out(vec![p("Typical",(0..bars).map(|i|Some((high[i]+low[i]+close[i])/3.0)).collect()),p("Signed volume",(0..bars).map(|i|Some((close[i]-open[i])*volume[i] as f64)).collect())]),
        "sma":out(vec![p("SMA 10",sma(&xs,10)),p("SMA 50",sma(&xs,50))]),
        "control":out(vec![p("Result",(0..bars).map(|i|{let total=(1..=16).map(|j|close[i]/j as f64).sum::<f64>();Some(if close[i]>open[i]{total}else{-total})}).collect())]),
        "persistent":out(vec![p("Cumulative",close.iter().map(|c|{cumulative+=c;Some(cumulative)}).collect())]),
        "collections":out(vec![p("OHLC mean",(0..bars).map(|i|Some((open[i]+high[i]+low[i]+close[i])/4.0)).collect()),p("Diagonal change",(0..bars).map(|i|Some(close[i]-open[i])).collect())]),
        "legacy-mixed":out(vec![p("MACD",macd),p("Signal",signal),p("SMA 50",sma(&xs,50)),p("RSI",rsi)])
    });
    json!({"data":{"open":open,"high":high,"low":low,"close":close,"volume":volume,"time":time,"timeframe":"1","mintick":0.01},"expected":expected})
}
/// The bar counts a bundle can actually carry.
///
/// [`archive_key`] admits `inputs/<bars>.json` for these and nothing else, so a
/// manifest that names a different feed size measures for a full window and
/// then cannot be archived. Both tiers validate their declared `bars` against
/// this list rather than against a literal copy in each tier, which is what keeps the
/// runner, the manifest, and the bundle writer from drifting apart.
pub(crate) const ARCHIVED_BAR_COUNTS: [usize; 2] = [2000, 10000];

pub(crate) fn sizes(m: &Value) -> TaskResult<Vec<usize>> {
    m["bars"]
        .as_array()
        .ok_or("missing bars")?
        .iter()
        .map(|v| {
            let n = v.as_u64().ok_or("invalid bar count")?;
            if !(50..=100_000).contains(&n) {
                return Err("bar count out of bounds".into());
            }
            Ok(n as usize)
        })
        .collect()
}
fn prepare(root: &Path, m: &Value) -> TaskResult {
    for bars in sizes(m)? {
        write(
            &root.join(format!("target/benchmark-data/{bars}.json")),
            &generated(bars),
        )?;
    }
    println!("Prepared immutable shared inputs and numeric reference values.");
    Ok(())
}

fn diff(
    a: &Value,
    b: &Value,
    at: &str,
    abs: f64,
    rel: f64,
    errors: &mut Vec<String>,
    max_error: &mut f64,
) {
    if errors.len() >= 20 {
        return;
    }
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            if let (Some(a), Some(b)) = (a.as_f64(), b.as_f64()) {
                let delta = (a - b).abs();
                *max_error = max_error.max(delta);
                if delta > abs + rel * b.abs() {
                    errors.push(format!("{at}: got {a}, expected {b}"));
                }
            } else if a != b {
                errors.push(format!("{at}: number mismatch"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                errors.push(format!("{at}: length {} != {}", a.len(), b.len()));
            }
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                diff(a, b, &format!("{at}[{i}]"), abs, rel, errors, max_error);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.keys().collect::<Vec<_>>() != b.keys().collect::<Vec<_>>() {
                errors.push(format!("{at}: keys differ"));
            }
            for (k, b) in b {
                diff(
                    a.get(k).unwrap_or(&Value::Null),
                    b,
                    &format!("{at}.{k}"),
                    abs,
                    rel,
                    errors,
                    max_error,
                );
            }
        }
        _ => {
            if a != b {
                errors.push(format!("{at}: got {a}, expected {b}"));
            }
        }
    }
}

#[derive(Clone)]
struct Cell {
    engine: String,
    workload: String,
    bars: usize,
    lane: String,
}
impl Cell {
    fn id(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.engine, self.workload, self.bars, self.lane
        )
    }
    fn info(&self) -> Value {
        json!({"id":self.id(),"engine":self.engine,"workload":self.workload,"bars":self.bars,"lane":self.lane})
    }
}
fn cells(m: &Value) -> TaskResult<Vec<Cell>> {
    let mut out = Vec::new();
    for workload in strings(m, "workloads")? {
        for bars in sizes(m)? {
            for engine in strings(m, "engines")? {
                for lane in strings(m, "lanes")? {
                    out.push(Cell {
                        engine: engine.clone(),
                        workload: workload.clone(),
                        bars,
                        lane,
                    });
                }
            }
        }
    }
    Ok(out)
}
fn invoke(
    root: &Path,
    m: &Value,
    cell: &Cell,
    folder: &Path,
    label: &str,
    warmups: u64,
) -> TaskResult<Value> {
    let request = folder.join(format!("{label}.request.json"));
    let result = folder.join(format!("{label}.response.json"));
    let req = json!({"engine":cell.engine,"source":format!("benchmarks/workloads/{}.pine",cell.workload),"input":root.join(format!("target/benchmark-data/{}.json",cell.bars)),"lane":cell.lane,"warmups":warmups,"iterations":number(m,"iterations")?});
    write(&request, &req)?;
    let log = File::create(folder.join(format!("{label}.log"))).map_err(|e| e.to_string())?;
    let mut child = Command::new("node")
        .current_dir(root)
        .env_remove("NODE_OPTIONS")
        .args([
            "--import",
            "./target/benchmark-engines/node_modules/@nullarch/resin/register.mjs",
        ])
        .arg(external_adapter(root)?)
        .arg(&request)
        .arg(&result)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let timeout = Duration::from_secs(number(m, "timeout_seconds")?);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if result.exists() {
                let response = read(&result)?;
                fs::remove_file(&result).map_err(|e| e.to_string())?;
                return Ok(response);
            }
            return Ok(json!({"status":"crashed","exit_code":status.code()}));
        }
        if start.elapsed() > timeout {
            child.kill().map_err(|e| e.to_string())?;
            child.wait().map_err(|e| e.to_string())?;
            return Ok(json!({"status":"timeout"}));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
pub(crate) fn command_text(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        Err(e) => e.to_string(),
    }
}
/// The host identity a run records: OS, CPU, memory,
/// logical CPU count, power mode, and thermal state.
///
/// [`command_text`] folds a spawn failure into the value it returns, so a probe
/// that is unavailable on a platform could record an error string or an empty
/// string and nothing would notice. A timing record needs the exact CPU and
/// power mode of the host it ran on, so each field is resolved per platform
/// here and the snapshot carries `provenance_complete`.
/// A caller that publishes timing is expected to refuse a snapshot where that
/// is false rather than archive a bundle with blank hardware fields.
///
/// `PINEWASM_BENCHMARK_POWER_MODE` is the operator's escape hatch: a host that
/// exposes no machine-readable power mode is not thereby exempt from stating
/// one, so the declared string is recorded verbatim and completeness is
/// satisfied by the operator's own claim.
pub(crate) fn hardware_profile() -> Value {
    let os = command_text("uname", &["-a"]);
    let (cpu, memory_bytes, logical_cpus, thermal) = if cfg!(target_os = "macos") {
        (
            command_text("sysctl", &["-n", "machdep.cpu.brand_string"]),
            command_text("sysctl", &["-n", "hw.memsize"]),
            command_text("sysctl", &["-n", "hw.logicalcpu"]),
            command_text("pmset", &["-g", "therm"]),
        )
    } else {
        (
            proc_field(&read_file("/proc/cpuinfo"), "model name"),
            meminfo_bytes(&read_file("/proc/meminfo")),
            command_text("nproc", &[]),
            thermal_zones(),
        )
    };
    let power = match std::env::var("PINEWASM_BENCHMARK_POWER_MODE") {
        Ok(declared) if !declared.trim().is_empty() => format!("declared: {}", declared.trim()),
        _ if cfg!(target_os = "macos") => command_text("pmset", &["-g", "batt"]),
        _ => read_text("/sys/firmware/acpi/platform_profile")
            .or_else(|| read_text("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"))
            .unwrap_or_default(),
    };
    let mut hardware = json!({
        "os": os,
        "cpu": cpu,
        "memory_bytes": memory_bytes,
        "logical_cpus": logical_cpus,
        "power": power,
        "thermal": thermal,
        "load": command_text("uptime", &[]),
        "process_load": process_load(),
    });
    hardware["provenance_complete"] = json!(hardware_complete(&hardware));
    hardware
}

/// The fields a publishable run has to state about its host.
///
/// Thermal state, load and process load are recorded but not required: many
/// hosts expose no thermal zones, and load is context rather than identity.
const REQUIRED_HARDWARE_FIELDS: [&str; 5] = ["os", "cpu", "memory_bytes", "logical_cpus", "power"];

/// Whether every required field carries a reading.
///
/// [`command_text`] reports a spawn failure as the error's own message, so a
/// non-empty value is not by itself evidence that the probe answered. Every
/// `std::io::Error` renders its raw code as `(os error N)`, which is the one
/// marker that separates a recorded failure from a real reading.
///
/// A Linux virtual machine with no ACPI platform profile and no cpufreq
/// governor states no power mode and is incomplete here on purpose. That is
/// the case the flag exists to catch, and the operator closes it by declaring
/// the mode in `PINEWASM_BENCHMARK_POWER_MODE`.
fn hardware_complete(hardware: &Value) -> bool {
    REQUIRED_HARDWARE_FIELDS.iter().all(|key| {
        let value = hardware[key].as_str().unwrap_or_default().trim();
        !value.is_empty() && !value.contains("(os error ")
    })
}

/// First `key: value` line of a `/proc` table, or an empty string.
fn proc_field(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == key).then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
}

/// `/proc/meminfo` states `MemTotal` in kilobytes; the macOS probe reports
/// bytes, so both platforms record one unit.
fn meminfo_bytes(text: &str) -> String {
    proc_field(text, "MemTotal")
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
        .map(|bytes| bytes.to_string())
        .unwrap_or_default()
}

fn read_file(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn read_text(path: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// `type=millidegrees` for every exposed thermal zone. Many Linux hosts expose
/// none, which is why this is recorded but not required for completeness.
fn thermal_zones() -> String {
    let Ok(entries) = fs::read_dir("/sys/class/thermal") else {
        return String::new();
    };
    let mut zones: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("thermal_zone"))
        })
        .filter_map(|path| {
            let kind = read_text(path.join("type").to_str()?)?;
            let temperature = read_text(path.join("temp").to_str()?)?;
            Some(format!("{kind}={temperature}"))
        })
        .collect();
    zones.sort();
    zones.join(" ")
}

pub(crate) fn process_load() -> Vec<f64> {
    // Record CPU utilization values only, without process names, arguments, or user paths.
    let mut values: Vec<_> = command_text("ps", &["-A", "-o", "pcpu="])
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .collect();
    values.sort_by(|a, b| b.total_cmp(a));
    values
}
fn tree_hash(path: &Path) -> TaskResult<String> {
    fn visit(base: &Path, dir: &Path, entries: &mut BTreeMap<String, String>) -> TaskResult {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                visit(base, &path, entries)?;
            } else {
                let name = path
                    .strip_prefix(base)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                let digest = if kind.is_symlink() {
                    format!(
                        "symlink:{}",
                        fs::read_link(&path).map_err(|e| e.to_string())?.display()
                    )
                } else {
                    hash(&path)?
                };
                entries.insert(name, digest);
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(path, path, &mut entries)?;
    Ok(sha256(
        &serde_json::to_vec(&entries).map_err(|e| e.to_string())?,
    ))
}
fn provenance(root: &Path, m: &Value) -> TaskResult<Value> {
    if command_text("node", &["--version"]) != "v22.22.3" {
        return Err("this frozen suite requires Node v22.22.3".into());
    }
    let mut files = BTreeMap::new();
    for p in [
        MANIFEST,
        "crates/pinewasm_xtask/src/benchmark_run.rs",
        "Cargo.lock",
        "target/benchmark-engines/package-lock.json",
        "target/wasm-pkg/pinewasm.js",
        "target/wasm-pkg/pinewasm_bg.wasm",
    ] {
        files.insert(p.to_owned(), hash(&root.join(p))?);
    }
    files.insert(
        "external:benchmark-adapter".into(),
        hash(&external_adapter(root)?)?,
    );
    for w in strings(m, "workloads")? {
        let p = format!("benchmarks/workloads/{w}.pine");
        files.insert(p.clone(), hash(&root.join(p))?);
    }
    for bars in sizes(m)? {
        let p = format!("target/benchmark-data/{bars}.json");
        files.insert(p.clone(), hash(&root.join(p))?);
    }
    for (path, expected) in m["subject_artifact_sha256"]
        .as_object()
        .ok_or("artifact pins missing")?
    {
        if files.get(path).map(String::as_str) != expected.as_str() {
            return Err(format!("subject artifact digest differs: {path}"));
        }
    }
    for relative in ["node_modules", "pine-a-script", "openpine"] {
        let p = format!("target/benchmark-engines/{relative}");
        files.insert(format!("tree:{p}"), tree_hash(&root.join(p))?);
    }
    let node_path = command_text("node", &["-p", "process.execPath"]);
    files.insert("node_executable".into(), hash(Path::new(&node_path))?);
    for (name, pin) in m["source_engines"]
        .as_object()
        .ok_or("source pins missing")?
    {
        let checkout = root.join("target/benchmark-engines").join(name);
        if git(&checkout, &["rev-parse", "HEAD"])?.trim()
            != pin["revision"].as_str().ok_or("revision missing")?
            || !git(&checkout, &["status", "--porcelain"])?
                .trim()
                .is_empty()
        {
            return Err(format!("source pin mismatch or dirty source for {name}"));
        }
    }
    for (name, version) in m["packages"].as_object().ok_or("packages missing")? {
        let p = read(
            &root
                .join("target/benchmark-engines/node_modules")
                .join(name)
                .join("package.json"),
        )?;
        if p["version"] != *version {
            return Err(format!("package version mismatch: {name}"));
        }
    }
    let mut snapshot = json!({"epoch":epoch(),"runner_commit":git(root,&["rev-parse","HEAD"])?.trim(),"dirty":git(root,&["status","--porcelain"])?,"sha256":files,"node":command_text("node",&["--version"]),"node_flags":["--import","./target/benchmark-engines/node_modules/@nullarch/resin/register.mjs"]});
    merge_hardware(&mut snapshot, hardware_profile());
    Ok(snapshot)
}

/// Fold the host identity into a provenance snapshot. Both tiers record the
/// same hardware keys, so the fields live in one helper and are merged rather
/// than spelled out twice.
pub(crate) fn merge_hardware(snapshot: &mut Value, hardware: Value) {
    let (Some(target), Value::Object(source)) = (snapshot.as_object_mut(), hardware) else {
        return;
    };
    for (key, value) in source {
        target.insert(key, value);
    }
}

// Deterministic, inspectable JSON archive: no tar paths or executable payloads.
pub(crate) fn archive(folder: &Path, destination: &Path) -> TaskResult {
    if destination.exists() {
        return Err("archive already exists".into());
    }
    let mut files = BTreeMap::new();
    for name in [
        "manifest.json",
        "environment-before.json",
        "environment-after.json",
        "gates.json",
        "samples.json",
        "summary.json",
    ] {
        files.insert(name.to_owned(), read(&folder.join(name))?);
    }
    // Optional top-level records: a run has a disposition only when it was
    // interrupted, and the subject suite additionally measures artifact size
    // and peak process memory.
    for optional in [
        "disposition.json",
        "artifacts.json",
        "memory.json",
        "memory-summary.json",
    ] {
        if folder.join(optional).exists() {
            files.insert(optional.to_owned(), read(&folder.join(optional))?);
        }
    }
    for dir in ["inputs", "outputs"] {
        for entry in fs::read_dir(folder.join(dir)).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or("invalid artifact name")?;
            let key = format!("{dir}/{name}");
            if !archive_key(&key) {
                return Err(format!("invalid archive path {key}"));
            }
            files.insert(key, read(&p)?);
        }
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut encoder = flate2::write::GzEncoder::new(
        File::create(destination).map_err(|e| e.to_string())?,
        flate2::Compression::default(),
    );
    encoder
        .write_all(
            &serde_json::to_vec(&json!({"schema_version":1,"files":files}))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    encoder.finish().map_err(|e| e.to_string())?;
    Ok(())
}
fn archive_key(key: &str) -> bool {
    if [
        "disposition.json",
        "artifacts.json",
        "memory.json",
        "memory-summary.json",
    ]
    .contains(&key)
    {
        return true;
    }
    if [
        "manifest.json",
        "environment-before.json",
        "environment-after.json",
        "gates.json",
        "samples.json",
        "summary.json",
    ]
    .contains(&key)
    {
        return true;
    }
    let Some((dir, file)) = key.split_once('/') else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".json") else {
        return false;
    };
    match dir {
        // Compared as text rather than parsed, so a padded or signed spelling
        // the writer never produces stays out of a bundle.
        "inputs" => ARCHIVED_BAR_COUNTS
            .iter()
            .any(|bars| stem == bars.to_string()),
        "outputs" => stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()),
        _ => false,
    }
}
pub(crate) fn archive_contents(bytes: &[u8]) -> TaskResult<Value> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("compressed benchmark bundle exceeds 16 MiB".into());
    }
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(128 * 1024 * 1024 + 1)
        .read_to_end(&mut decoded)
        .map_err(|e| e.to_string())?;
    if decoded.len() > 128 * 1024 * 1024 {
        return Err("expanded benchmark bundle exceeds 128 MiB".into());
    }
    let value: Value = serde_json::from_slice(&decoded).map_err(|e| e.to_string())?;
    if value["schema_version"] != 1 {
        return Err("invalid archive version".into());
    }
    for (key, content) in value["files"].as_object().ok_or("archive files missing")? {
        if !archive_key(key) {
            return Err(format!("invalid archive path {key}"));
        }
        if let Some(name) = key
            .strip_prefix("outputs/")
            .and_then(|k| k.strip_suffix(".json"))
        {
            if sha256(&serde_json::to_vec(content).map_err(|e| e.to_string())?) != name {
                return Err("output digest mismatch".into());
            }
        }
    }
    Ok(value)
}
pub(crate) fn unpack_archive(source: &Path, folder: &Path) -> TaskResult {
    if folder.exists() {
        return Err("unpack destination already exists".into());
    }
    let value = archive_contents(&fs::read(source).map_err(|e| e.to_string())?)?;
    for (name, content) in value["files"].as_object().ok_or("archive files missing")? {
        write(&folder.join(name), content)?;
    }
    Ok(())
}

fn unpack(source: &Path, folder: &Path) -> TaskResult {
    unpack_archive(source, folder)?;
    report(folder)
}
pub(crate) fn check_result(m: &Value, observed: &Value, expected: &Value) -> Value {
    let mut errors = Vec::new();
    let mut max = 0.0;
    diff(
        observed,
        expected,
        "result",
        m["absolute_tolerance"].as_f64().unwrap_or(0.0),
        m["relative_tolerance"].as_f64().unwrap_or(0.0),
        &mut errors,
        &mut max,
    );
    json!({"pass":errors.is_empty(),"first_differences":errors,"max_observed_numeric_error":max})
}
pub(crate) fn preserve_output(folder: &Path, r: &mut Value) -> TaskResult {
    if let Some(o) = r.as_object_mut().and_then(|r| r.remove("result")) {
        let bytes = serde_json::to_vec(&o).map_err(|e| e.to_string())?;
        let sha = sha256(&bytes);
        let relative = format!("outputs/{sha}.json");
        let path = folder.join(&relative);
        fs::create_dir_all(path.parent().ok_or("output parent missing")?)
            .map_err(|e| e.to_string())?;
        if !path.exists() {
            fs::write(&path, bytes).map_err(|e| e.to_string())?;
        }
        r["output_sha256"] = json!(sha);
        r["output_path"] = json!(relative);
    }
    Ok(())
}
fn probe(root: &Path, m: &Value, folder: &Path) -> TaskResult<Vec<Value>> {
    let work = folder.join("invocations");
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let mut gates = Vec::new();
    for c in cells(m)? {
        let mut row = c.info();
        if m["unavailable_lanes"][&c.engine]
            .as_array()
            .is_some_and(|a| a.contains(&json!(c.lane)))
        {
            row["status"] = json!("unavailable-lane");
            gates.push(row);
            continue;
        }
        let input = read(&root.join(format!("target/benchmark-data/{}.json", c.bars)))?;
        let mut r = invoke(root, m, &c, &work, &format!("gate-{}", c.id()), 0)?;
        // Untimed correctness checks, including the diagnostic workload.
        if let Some(obj) = r.as_object_mut() {
            obj.remove("elapsed_ms");
            obj.remove("warmup_ms");
        }
        if r["status"] == "ok" {
            r["correctness"] = check_result(m, &r["result"], &input["expected"][&c.workload]);
        }
        row["eligible"] = json!(
            r["status"] == "ok"
                && r["correctness"]["pass"] == true
                && !strings(m, "diagnostic_workloads")?.contains(&c.workload)
        );
        preserve_output(folder, &mut r)?;
        row["observation"] = r;
        println!(
            "gate {}: {}",
            c.id(),
            if row["eligible"] == true {
                "pass"
            } else {
                "not timed"
            }
        );
        gates.push(row);
        write(&folder.join("gates.json"), &json!(gates))?;
    }
    Ok(gates)
}
pub(crate) fn shuffled<T>(xs: &mut [T], rng: &mut ChaCha8Rng) {
    for i in (1..xs.len()).rev() {
        let j = (rng.next_u64() % ((i + 1) as u64)) as usize;
        xs.swap(i, j);
    }
}
pub(crate) fn median(xs: &[f64]) -> f64 {
    let mut a = xs.to_vec();
    a.sort_by(f64::total_cmp);
    let n = a.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 0 {
        a[n / 2 - 1].midpoint(a[n / 2])
    } else {
        a[n / 2]
    }
}
pub(crate) fn p95(xs: &[f64]) -> f64 {
    let mut a = xs.to_vec();
    a.sort_by(f64::total_cmp);
    a[(95 * a.len()).div_ceil(100) - 1]
}
fn ratio_ci(a: &[f64], b: &[f64], seed: u64, count: usize) -> [f64; 2] {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(count);
    for _ in 0..count {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for _ in 0..a.len() {
            let i = (rng.next_u64() % a.len() as u64) as usize;
            x.push(a[i]);
            y.push(b[i]);
        }
        estimates.push(median(&x) / median(&y));
    }
    estimates.sort_by(f64::total_cmp);
    [
        estimates[(25 * count).div_ceil(1000) - 1],
        estimates[(975 * count).div_ceil(1000) - 1],
    ]
}
fn untimed_reason(gate: &Value, diagnostic: bool) -> &'static str {
    if gate["status"] == "unavailable-lane" {
        "unavailable API lane"
    } else if gate["eligible"] == true {
        "incomplete/failed samples"
    } else if diagnostic {
        "diagnostic only"
    } else {
        "unsupported or numeric mismatch"
    }
}

fn report(folder: &Path) -> TaskResult {
    let m = read(&folder.join("manifest.json"))?;
    let raw = read(&folder.join("samples.json"))?;
    let gates = read(&folder.join("gates.json"))?;
    if folder.join("disposition.json").exists() {
        let disposition = read(&folder.join("disposition.json"))?;
        if disposition["status"] == "interrupted" {
            let count = raw.as_array().ok_or("samples missing")?.len();
            let md = format!("# Interrupted exploratory run\n\nThis run was interrupted after {count} recorded samples. No speed ratios or performance claims are published from it.\n\n{}\n\nThe raw bundle preserves the manifest, initial environment, full correctness attempt matrix, completed samples, and deduplicated outputs. Missing or in-flight samples are not silently substituted.\n", disposition["reason"].as_str().ok_or("interruption reason missing")?);
            fs::write(folder.join("report.md"), md).map_err(|e| e.to_string())?;
            return write(
                &folder.join("summary.json"),
                &json!({"status":"interrupted","completed_samples":count,"reason":disposition["reason"]}),
            );
        }
        return Err("unrecognized run disposition".into());
    }
    let before = read(&folder.join("environment-before.json"))?;
    let after = read(&folder.join("environment-after.json"))?;
    if before["sha256"] != after["sha256"]
        || before["runner_commit"] != after["runner_commit"]
        || before["dirty"] != ""
        || after["dirty"] != ""
    {
        return Err("run provenance is inconsistent or source was dirty".into());
    }
    let mut inputs = BTreeMap::new();
    for bars in sizes(&m)? {
        let path = folder.join(format!("inputs/{bars}.json"));
        if json!(hash(&path)?) != before["sha256"][format!("target/benchmark-data/{bars}.json")] {
            return Err("archived input hash differs from run provenance".into());
        }
        inputs.insert(bars, read(&path)?);
    }
    let mut samples: BTreeMap<String, BTreeMap<u64, f64>> = BTreeMap::new();
    let mut failed = std::collections::BTreeSet::new();
    for r in raw.as_array().ok_or("samples must be an array")? {
        let id = r["cell"]["id"]
            .as_str()
            .ok_or("sample id missing")?
            .to_owned();
        if r["observation"]["status"] != "ok" || r["observation"]["correctness"]["pass"] != true {
            failed.insert(id);
            continue;
        }
        let output_path = r["observation"]["output_path"]
            .as_str()
            .ok_or("output path missing")?;
        if !archive_key(output_path) || !output_path.starts_with("outputs/") {
            return Err("invalid measured output path".into());
        }
        let path = folder.join(output_path);
        if json!(hash(&path)?) != r["observation"]["output_sha256"] {
            return Err("measured output hash mismatch".into());
        }
        let bars = number(&r["cell"], "bars")? as usize;
        let workload = r["cell"]["workload"].as_str().ok_or("workload missing")?;
        if check_result(
            &m,
            &read(&path)?,
            &inputs.get(&bars).ok_or("input missing")?["expected"][workload],
        )["pass"]
            != true
        {
            return Err("stored successful correctness result is not reproducible".into());
        }
        let ms = r["observation"]["elapsed_ms"]
            .as_f64()
            .ok_or("elapsed missing")?;
        if !ms.is_finite()
            || ms
                < m["precision_floor_ms"]
                    .as_f64()
                    .ok_or("precision floor missing")?
        {
            failed.insert(id.clone());
        }
        let block = number(r, "block")?;
        if samples.entry(id).or_default().insert(block, ms).is_some() {
            return Err("duplicate sample block".into());
        }
    }
    let mut md=String::from("# Exploratory Node.js indicator measurements\n\nThese measurements are exploratory and are not benchmark results. They cover only the predeclared numeric plot projection and do not substantiate universal speed, visual, strategy, or browser-performance claims. All runs use the same Node process boundary and include complete numeric result materialization.\n\nRatio is other engine median / PineWASM median; above 1 favors PineWASM. Intervals are paired block-bootstrap 95% intervals. p95 is descriptive with 30 samples. Failed and diagnostic cells remain in the matrix.\n\n| Workload | Bars | Lane | Engine | Samples | Median ms | p95 ms | Ratio [95% interval] |\n|---|---:|---|---|---:|---:|---:|---|\n");
    let mut summary = Vec::new();
    for c in cells(&m)? {
        let mut row = c.info();
        let id = c.id();
        let gate = gates
            .as_array()
            .ok_or("gates missing")?
            .iter()
            .find(|g| g["id"] == id)
            .ok_or("gate missing")?;
        let xs: Vec<f64> = samples
            .get(&id)
            .map(|s| s.values().copied().collect())
            .unwrap_or_default();
        let valid = xs.len() == number(&m, "samples")? as usize
            && !failed.contains(&id)
            && gate["eligible"] == true;
        row["samples"] = json!(xs.len());
        row["valid_exploratory_samples"] = json!(valid);
        if valid {
            row["median_ms"] = json!(median(&xs));
            row["p95_ms"] = json!(p95(&xs));
            let base = Cell {
                engine: "pinewasm".into(),
                ..c.clone()
            }
            .id();
            let mut ratio = String::from("—");
            if c.engine != "pinewasm" && !failed.contains(&base) {
                if let Some(base_samples) = samples.get(&base) {
                    if base_samples.len() == xs.len() && samples[&id].keys().eq(base_samples.keys())
                    {
                        let bs: Vec<_> = base_samples.values().copied().collect();
                        let ci = ratio_ci(
                            &xs,
                            &bs,
                            number(&m, "bootstrap_seed")?,
                            number(&m, "bootstrap_resamples")? as usize,
                        );
                        let point = median(&xs) / median(&bs);
                        row["competitor_over_pinewasm"] = json!({"median_ratio":point,"ci95":ci});
                        ratio = format!("{point:.2} [{:.2}, {:.2}]", ci[0], ci[1]);
                    }
                }
            }
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.3} | {:.3} | {} |\n",
                c.workload,
                c.bars,
                c.lane,
                c.engine,
                xs.len(),
                median(&xs),
                p95(&xs),
                ratio
            ));
        } else {
            let status = untimed_reason(
                gate,
                strings(&m, "diagnostic_workloads")?.contains(&c.workload),
            );
            row["reason"] = json!(status);
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | — | — | {} |\n",
                c.workload,
                c.bars,
                c.lane,
                c.engine,
                xs.len(),
                status
            ));
        }
        summary.push(row);
    }
    md.push_str("\n## Scope and limitations\n\n");
    for l in strings(&m, "limitations")? {
        md.push_str(&format!("- {l}\n"));
    }
    write(&folder.join("summary.json"), &json!(summary))?;
    fs::write(folder.join("report.md"), md).map_err(|e| e.to_string())?;
    println!("Report: {}", folder.join("report.md").display());
    Ok(())
}

pub(crate) fn run(args: &[String]) -> TaskResult {
    let root = repo_root()?;
    let m = manifest(&root)?;
    let command = args.first().map_or("help", String::as_str);
    if command == "archive" || command == "unpack" {
        let source = args.get(1).ok_or("source directory/archive required")?;
        let dest = args
            .get(2)
            .ok_or("destination archive/directory required")?;
        return if command == "archive" {
            archive(Path::new(source), Path::new(dest))
        } else {
            unpack(Path::new(source), Path::new(dest))
        };
    }
    if command == "prepare" {
        return prepare(&root, &m);
    }
    if command == "setup" {
        return setup(&root, &m);
    }
    let folder = args.get(1).map_or_else(
        || root.join(format!("target/benchmark-runs/{}", epoch())),
        PathBuf::from,
    );
    if command == "stop" {
        if !folder.join("manifest.json").exists() {
            return Err("run folder not found".into());
        }
        return write(
            &folder.join("stop-request.json"),
            &json!({"reason":args.get(2).ok_or("stop reason required")?,"epoch":epoch()}),
        );
    }
    if command == "seal-interrupted" {
        if !folder.join("disposition.json").exists() {
            return Err("interruption disposition required".into());
        }
        // The original process was interrupted before its final snapshot. Do not
        // invent one from a later, potentially changed runner or environment.
        write(
            &folder.join("environment-after.json"),
            &json!({"status":"not-collected","reason":"process interrupted before final snapshot"}),
        )?;
        return report(&folder);
    }
    if command == "report" {
        return report(&folder);
    }
    if !["probe", "run"].contains(&command) {
        return Err(
            "usage: cargo xtask benchmark prepare | probe DIR | run DIR | report DIR".into(),
        );
    }
    if folder.exists() {
        return Err(format!("refusing to overwrite {}", folder.display()));
    }
    if command == "run" && !git(&root, &["status", "--porcelain"])?.trim().is_empty() {
        return Err("commit candidate manifest and runner before timing".into());
    }
    external_adapter(&root)?;
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    write(&folder.join("manifest.json"), &m)?;
    write(&folder.join("samples.json"), &json!([]))?;
    let before = provenance(&root, &m)?;
    write(&folder.join("environment-before.json"), &before)?;
    for bars in sizes(&m)? {
        let input = read(&root.join(format!("target/benchmark-data/{bars}.json")))?;
        write(&folder.join(format!("inputs/{bars}.json")), &input)?;
    }
    let gates = probe(&root, &m, &folder)?;
    write(&folder.join("gates.json"), &json!(gates))?;
    if command == "probe" {
        return Ok(());
    }
    let mut selected: Vec<_> = cells(&m)?
        .into_iter()
        .filter(|c| {
            gates
                .iter()
                .any(|g| g["id"] == c.id() && g["eligible"] == true)
        })
        .collect();
    let mut rng = ChaCha8Rng::seed_from_u64(number(&m, "order_seed")?);
    let mut raw = Vec::new();
    for block in 0..number(&m, "samples")? {
        shuffled(&mut selected, &mut rng);
        for c in &selected {
            if folder.join("stop-request.json").exists() {
                let request = read(&folder.join("stop-request.json"))?;
                write(
                    &folder.join("disposition.json"),
                    &json!({"status":"interrupted","reason":request["reason"],"epoch":epoch(),"completed_samples":raw.len()}),
                )?;
                write(
                    &folder.join("environment-after.json"),
                    &provenance(&root, &m)?,
                )?;
                return report(&folder);
            }
            let warmups = m["warmups"][&c.lane].as_u64().ok_or("warmups missing")?;
            let mut r = invoke(
                &root,
                &m,
                c,
                &folder.join("invocations"),
                &format!("block-{block}-{}", c.id()),
                warmups,
            )?;
            if r["status"] == "ok" {
                let input = read(&root.join(format!("target/benchmark-data/{}.json", c.bars)))?;
                r["correctness"] = check_result(&m, &r["result"], &input["expected"][&c.workload]);
            }
            preserve_output(&folder, &mut r)?;
            raw.push(json!({"block":block,"cell":c.info(),"observation":r}));
            write(&folder.join("samples.json"), &json!(raw))?;
        }
        println!(
            "Completed randomized block {}/{} ({} cells)",
            block + 1,
            number(&m, "samples")?,
            selected.len()
        );
    }
    let after = provenance(&root, &m)?;
    write(&folder.join("environment-after.json"), &after)?;
    if before["sha256"] != after["sha256"] {
        return Err("input, adapter, or artifact changed during run".into());
    }
    report(&folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_hardware_fields_are_parsed_rather_than_left_blank() {
        // These are the Linux probes that parse /proc tables, so they are
        // exercised on every platform from fixed text. Empty CPU or memory
        // fields must not be recorded as readings.
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R) Gold 6248R CPU @ 3.00GHz\nsiblings\t: 96\n";
        assert_eq!(
            proc_field(cpuinfo, "model name"),
            "Intel(R) Xeon(R) Gold 6248R CPU @ 3.00GHz"
        );
        assert_eq!(proc_field(cpuinfo, "flags"), "");
        // Kilobytes in, bytes out, so both platforms record one unit.
        assert_eq!(
            meminfo_bytes("MemTotal:       131035160 kB\nMemFree:        99999 kB\n"),
            "134180003840"
        );
        assert_eq!(meminfo_bytes("MemFree: 12 kB\n"), "");
        assert_eq!(meminfo_bytes(""), "");
    }

    #[test]
    fn hardware_completeness_follows_the_fields_a_runner_actually_stated() {
        let stated = json!({
            "os": "Linux runner 6.8.0",
            "cpu": "Intel(R) Xeon(R) Gold 6248R CPU @ 3.00GHz",
            "memory_bytes": "134180003840",
            "logical_cpus": "96",
            "power": "declared: performance governor, turbo disabled",
            "thermal": "",
        });
        // Thermal is empty here and completeness does not care: plenty of hosts
        // expose no thermal zones.
        assert!(hardware_complete(&stated));
        // A Linux virtual machine with no ACPI platform profile and no cpufreq
        // governor is exactly the case this flag exists to catch. Seen on
        // a virtualized Linux x86_64 host.
        let mut unstated_power = stated.clone();
        unstated_power["power"] = json!("");
        assert!(!hardware_complete(&unstated_power));
        // A refused probe records the error's own text, which is not a reading.
        let mut refused = stated.clone();
        refused["cpu"] = json!("No such file or directory (os error 2)");
        assert!(!hardware_complete(&refused));
        let mut whitespace = stated;
        whitespace["logical_cpus"] = json!("   ");
        assert!(!hardware_complete(&whitespace));
        assert!(!hardware_complete(&json!({})));
    }

    #[test]
    fn a_hardware_snapshot_names_every_field_a_bundle_records() {
        let hardware = hardware_profile();
        for key in [
            "os",
            "cpu",
            "memory_bytes",
            "logical_cpus",
            "power",
            "thermal",
            "load",
        ] {
            assert!(hardware[key].is_string(), "{key} must be recorded");
        }
        assert!(hardware["process_load"].is_array());
        // The flag describes this host rather than asserting something about
        // it: a container with no cpufreq governor legitimately reports false,
        // which is what stops it from publishing timing. macOS always answers
        // `sysctl` and `pmset`, so a false flag there is a regressed probe.
        assert_eq!(
            hardware["provenance_complete"],
            json!(hardware_complete(&hardware))
        );
        if cfg!(target_os = "macos") {
            assert_eq!(hardware["provenance_complete"], true, "{hardware}");
        }
        // The snapshot merges into a provenance object without displacing what
        // the caller already recorded.
        let mut snapshot = json!({"epoch": 7, "profile": "release"});
        merge_hardware(&mut snapshot, hardware);
        assert_eq!(snapshot["epoch"], 7);
        assert_eq!(snapshot["profile"], "release");
        assert!(snapshot["cpu"].is_string());
    }

    #[test]
    fn external_adapter_rejects_checkout_files_and_accepts_external_files() {
        let temp = crate::command::temp_dir("external-benchmark-adapter").unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let inside = root.join("adapter.mjs");
        let outside = temp.path().join("adapter.mjs");
        fs::write(&inside, "").unwrap();
        fs::write(&outside, "").unwrap();
        assert!(external_adapter_path(&root, Path::new("adapter.mjs")).is_err());
        assert!(external_adapter_path(&root, &inside).is_err());
        assert!(external_adapter_path(&root, temp.path()).is_err());
        assert!(external_adapter_path(&root, &temp.path().join("missing")).is_err());
        assert_eq!(
            external_adapter_path(&root, &outside).unwrap(),
            outside.canonicalize().unwrap()
        );
        #[cfg(unix)]
        {
            let link = temp.path().join("external-link.mjs");
            std::os::unix::fs::symlink(&inside, &link).unwrap();
            assert!(external_adapter_path(&root, &link).is_err());
        }
    }

    #[test]
    fn unavailable_lane_is_distinct_from_failure_or_diagnostic() {
        let unavailable = json!({"status":"unavailable-lane"});
        assert_eq!(untimed_reason(&unavailable, false), "unavailable API lane");
        assert_eq!(untimed_reason(&unavailable, true), "unavailable API lane");
        assert_eq!(
            untimed_reason(&json!({"eligible":true}), false),
            "incomplete/failed samples"
        );
        assert_eq!(
            untimed_reason(&json!({"eligible":false}), true),
            "diagnostic only"
        );
        assert_eq!(
            untimed_reason(&json!({"eligible":false}), false),
            "unsupported or numeric mismatch"
        );
    }
    #[test]
    fn interrupted_run_never_reports_ratios() {
        let temp = crate::command::temp_dir("benchmark-interruption").unwrap();
        let folder = temp.path();
        for (name, value) in [
            ("manifest.json", json!({})),
            ("samples.json", json!([{"observation":{"elapsed_ms":1}}])),
            ("gates.json", json!([])),
            (
                "disposition.json",
                json!({"status":"interrupted","reason":"Concurrent build"}),
            ),
        ] {
            write(&folder.join(name), &value).unwrap();
        }
        report(folder).unwrap();
        let summary = read(&folder.join("summary.json")).unwrap();
        assert_eq!(summary["status"], "interrupted");
        assert_eq!(summary["completed_samples"], 1);
        assert!(summary.get("competitor_over_pinewasm").is_none());
    }
    #[test]
    fn archive_rejects_paths_and_altered_output() {
        assert!(!archive_key("outputs/../../README.md"));
        assert!(!archive_key("inputs/10000.json/extra"));
        assert!(archive_key("inputs/10000.json"));
        // Optional records travel with a bundle; unnamed top-level files do not.
        assert!(archive_key("disposition.json"));
        assert!(archive_key("artifacts.json"));
        assert!(archive_key("memory.json"));
        assert!(archive_key("memory-summary.json"));
        assert!(!archive_key("notes.json"));
        let output = json!({"bars":2,"plots":[{"title":"x","values":[null,100.21019673990946]}]});
        let name = format!(
            "outputs/{}.json",
            sha256(&serde_json::to_vec(&output).unwrap())
        );
        let mut bundle = json!({"schema_version":1,"files":{name.clone():output}});
        let encode = |v: &Value| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&serde_json::to_vec(v).unwrap()).unwrap();
            e.finish().unwrap()
        };
        assert!(archive_contents(&encode(&bundle)).is_ok());
        bundle["files"][&name]["bars"] = json!(3);
        assert!(archive_contents(&encode(&bundle)).is_err());
        bundle["files"] = json!({"../outside.json":{}});
        assert!(archive_contents(&encode(&bundle)).is_err());
    }

    #[test]
    fn archive_round_trip_retains_peak_memory_evidence() {
        let temp = crate::command::temp_dir("benchmark-memory-archive").unwrap();
        let run = temp.path().join("run");
        for (name, value) in [
            ("manifest.json", json!({"suite":"subject-tiers-v1"})),
            ("environment-before.json", json!({"runner_commit":"abc"})),
            ("environment-after.json", json!({"runner_commit":"abc"})),
            ("gates.json", json!([])),
            ("samples.json", json!([])),
            ("summary.json", json!([])),
        ] {
            write(&run.join(name), &value).unwrap();
        }
        fs::create_dir_all(run.join("inputs")).unwrap();
        fs::create_dir_all(run.join("outputs")).unwrap();
        let raw = "Maximum resident set size (kbytes): 12345\n";
        let memory = json!([{
            "block":0,
            "cell":{"id":"history-2000-peak-memory"},
            "observation":{"memory":{
                "platform":"linux",
                "sampler":"/usr/bin/time -v",
                "field":"Maximum resident set size (kbytes)",
                "source_units_to_bytes":1024,
                "cadence":"Kernel-maintained process high-water mark; no polling interval",
                "raw_output":raw,
                "raw_output_sha256":sha256(raw.as_bytes()),
                "peak_rss_bytes":12_641_280,
            }},
        }]);
        let memory_summary = json!([{
            "id":"history-2000-peak-memory",
            "samples":30,
            "median_bytes":12_641_280,
            "p95_bytes":12_641_280,
            "maximum_bytes":12_641_280,
        }]);
        write(&run.join("memory.json"), &memory).unwrap();
        write(&run.join("memory-summary.json"), &memory_summary).unwrap();

        let bundle = temp.path().join("run.json.gz");
        archive(&run, &bundle).unwrap();
        let contents = archive_contents(&fs::read(&bundle).unwrap()).unwrap();
        assert_eq!(contents["files"]["memory.json"], memory);
        assert_eq!(contents["files"]["memory-summary.json"], memory_summary);

        let unpacked = temp.path().join("unpacked");
        unpack_archive(&bundle, &unpacked).unwrap();
        assert_eq!(read(&unpacked.join("memory.json")).unwrap(), memory);
        assert_eq!(
            read(&unpacked.join("memory-summary.json")).unwrap(),
            memory_summary
        );
    }
    #[test]
    fn statistics_use_even_median_and_nearest_rank() {
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]), 4.0);
        assert_eq!(p95(&(1..=30).map(f64::from).collect::<Vec<_>>()), 29.0);
    }
    #[test]
    fn correctness_rejects_missing_values_and_metadata() {
        let mut errors = Vec::new();
        let mut max = 0.0;
        diff(
            &json!({"plots":[{"title":"wrong","values":[null,2]}]}),
            &json!({"plots":[{"title":"right","values":[1,2]}]}),
            "",
            1e-9,
            1e-10,
            &mut errors,
            &mut max,
        );
        assert_eq!(errors.len(), 2);
    }
    #[test]
    fn bootstrap_preserves_paired_scale() {
        let a = vec![1.0, 3.0, 7.0, 11.0];
        let b: Vec<_> = a.iter().map(|x| x * 2.0).collect();
        assert_eq!(ratio_ci(&b, &a, 7, 1000), [2.0, 2.0]);
    }
    #[test]
    fn reference_sma_requires_complete_window() {
        assert_eq!(
            sma(&[Some(2.0), Some(4.0), Some(6.0)], 2),
            vec![None, Some(3.0), Some(5.0)]
        );
        assert_eq!(sma(&[None, Some(4.0)], 2), vec![None, None]);
    }
}
