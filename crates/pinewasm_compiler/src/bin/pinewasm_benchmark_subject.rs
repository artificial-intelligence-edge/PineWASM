//! The measured PineWASM subject for the benchmark suite.
//!
//! `docs/benchmarks.md` reports each timing boundary independently, so the
//! benchmark subject needs one process that can enter exactly one boundary per
//! invocation and nothing else. That is what this binary is: it reads a
//! request, performs one measurement mode's work, and writes the elapsed time together
//! with the fully materialized numeric projection the correctness gate checks.
//!
//!   `pinewasm-benchmark-subject REQUEST.json RESPONSE.json`
//!
//! It exists as a workspace binary rather than as code inside `pinewasm_xtask`
//! for two reasons. First, the cold-process mode needs a real process
//! boundary, and a separate optimized executable is the only honest way to own
//! one. Second, the repository's policy jobs build `pinewasm_xtask` on fresh
//! runners; linking the whole compiler into it would make every policy job pay
//! for the pipeline it never calls.
//!
//! The request/response contract deliberately mirrors the harness protocol in
//! `crates/pinewasm_xtask/src/benchmark_run.rs` so both measurement modes share
//! one expected-value check, one tolerance rule, and one archive schema. The
//! difference is that this subject lives in the checkout, so the benchmark
//! subject's correctness gate reproduces from a clean checkout with no network
//! access and no external services.
//!
//! This binary measures and reports the measurement only.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, IncrementalSession, MarketData, PlotTickSample,
    PreparedProgram, RunOptions, RunResult, Tick, Value,
};
use serde::Deserialize;
use serde_json::{json, Map, Value as Json};
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "pinewasm-benchmark-subject REQUEST.json RESPONSE.json";

/// One measured invocation, written by the harness.
///
/// `iterations` is the inner repetition count the pre-registered manifest
/// selected for this mode. A mode whose single operation lands under the
/// timer's usable resolution is repeated inside one measured block rather than
/// reported at a precision the clock cannot support; the harness divides and
/// records both numbers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: String,
    input: String,
    lane: String,
    warmups: u64,
    iterations: u64,
    /// Where to write the reusable compiled artifact, for the artifact-size
    /// measurement only. The harness owns compression and never archives the
    /// bytes; this binary only produces them.
    #[serde(default)]
    artifact_out: Option<String>,
}

/// The declared projection: every named numeric plot, in declaration order,
/// with `na` spelled as JSON `null`.
///
/// This is the same projection the benchmark comparison checks, which is what
/// lets both modes check against the same expected values in the input file.
/// Styles, colors, fills, drawings, alerts, and strategy events are outside it,
/// and no mode here claims otherwise.
fn projection(result: &RunResult, bars: usize) -> Json {
    let plots: Vec<Json> = result
        .plots
        .iter()
        .map(|plot| {
            let values: Vec<Json> = plot.values.iter().map(scalar).collect();
            json!({"title": plot.title.clone().unwrap_or_default(), "values": values})
        })
        .collect();
    json!({ "bars": bars, "plots": plots })
}

/// One bar's slice of the declared projection, used by the incremental mode.
///
/// The incremental boundary is defined as one bar through materialization of
/// *that bar's* projection, so nothing inside the timer may touch a plot series:
/// a full-history snapshot would report history length rather than the bar.
/// `sample` is what the session hands back for the tick — one value per plot
/// slot, in slot order — and `titles` was read from the compiled program before
/// the measured block began.
fn forming_bar_projection(titles: &[String], sample: &PlotTickSample) -> Json {
    let plots: Vec<Json> = titles
        .iter()
        .enumerate()
        .map(|(slot, title)| {
            json!({
                "title": title,
                "value": sample.values.get(slot).map_or(Json::Null, scalar),
            })
        })
        .collect();
    json!({ "plots": plots })
}

/// Normalize one runtime scalar. `na` and any non-finite float become `null`;
/// integers keep their exact spelling, which the tolerant comparison treats as
/// equivalent to the expected float value.
fn scalar(value: &Value) -> Json {
    match value {
        Value::Na => Json::Null,
        Value::Int(n) => json!(n),
        Value::Float(x) if x.is_finite() => json!(x),
        Value::Bool(b) => json!(b),
        Value::String(s) => json!(s),
        _ => Json::Null,
    }
}

/// Read the shared input file and hand back the market-data object and bar
/// count without converting either. Conversion into the engine's own market
/// data is part of every timed boundary, so it must not happen here.
fn load_input(path: &str) -> Result<(Json, usize), String> {
    let text = fs::read_to_string(path).map_err(|error| format!("input `{path}`: {error}"))?;
    let value: Json =
        serde_json::from_str(&text).map_err(|error| format!("input `{path}`: {error}"))?;
    let data = value
        .get("data")
        .cloned()
        .ok_or_else(|| format!("input `{path}`: missing `data`"))?;
    let bars = data
        .get("close")
        .and_then(Json::as_array)
        .map(Vec::len)
        .ok_or_else(|| format!("input `{path}`: missing `data.close`"))?;
    Ok((data, bars))
}

/// Convert host JSON into the engine's market data. Inside the timer for every
/// mode that names data conversion, so each timed boundary includes it.
fn convert(data: &Json) -> Result<MarketData, String> {
    MarketData::deserialize(data).map_err(|error| format!("market data: {error}"))
}

/// Compile and fail loudly on any error diagnostic. Untimed modes use this to
/// prepare state; the compile mode times the same call directly.
fn compile_program(source: &str) -> Result<pinewasm_compiler::Program, String> {
    let compiled = compile(source, CompileOptions::default());
    compiled.program.ok_or_else(|| {
        let first = compiled
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .next()
            .unwrap_or_else(|| "compilation produced no program".to_string());
        format!("compile failed: {first}")
    })
}

/// Fail a run that did not execute cleanly, so a partial series can never be
/// compared against complete expected values and silently "pass" on length alone.
fn require_clean(result: &RunResult) -> Result<(), String> {
    if let Some(error) = result.runtime_errors.first() {
        return Err(format!("runtime error: {}", error.message));
    }
    Ok(())
}

/// The outcome of one mode: the measured block, the inner repetition count it
/// covers, and the projection the gate compares.
struct Measured {
    elapsed_ms: f64,
    iterations: u64,
    result: Json,
}

/// Serialize one projection and hand back both the value and its bytes.
///
/// Materialization is part of every timed boundary this suite defines, so the
/// bytes are produced inside the timer even though only the value is reported.
fn materialize(projected: Json) -> Result<(Json, Vec<u8>), String> {
    let bytes = serde_json::to_vec(&projected).map_err(|error| error.to_string())?;
    Ok((projected, bytes))
}

/// Time `body` once after `warmups` discarded repetitions.
///
/// `body` returns its boundary's projection together with the materialized
/// bytes of that projection. Black-boxing the bytes is what makes the
/// optimizer keep the work: a mode that computed a series and dropped it
/// unread would be free to compute nothing. The projection travels back as a
/// value rather than being parsed out of those bytes, because this crate's
/// `serde_json` does not enable exact float parsing and a self-round-trip
/// would quietly move the last bit of every measured value before the gate
/// ever saw it.
fn timed(
    warmups: u64,
    iterations: u64,
    mut body: impl FnMut() -> Result<(Json, Vec<u8>), String>,
) -> Result<(f64, Json), String> {
    for _ in 0..warmups {
        black_box(body()?.1);
    }
    let start = Instant::now();
    let mut last = Json::Null;
    for _ in 0..iterations {
        let (projected, bytes) = body()?;
        black_box(bytes);
        last = projected;
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    Ok((elapsed, last))
}

/// Cold source-to-result: source and converted inputs in, complete
/// materialized projection out, with nothing reused between repetitions.
fn lane_cold(
    request: &Request,
    source: &str,
    data: &Json,
    bars: usize,
) -> Result<Measured, String> {
    let (elapsed_ms, result) = timed(request.warmups, request.iterations, || {
        let market = convert(data)?;
        let program = compile_program(source)?;
        let run = run_program(&program, &market, &RunOptions::default());
        require_clean(&run)?;
        materialize(projection(&run, bars))
    })?;
    Ok(Measured {
        elapsed_ms,
        iterations: request.iterations,
        result,
    })
}

/// Compilation only: parse, analyze, lower, validate, and emit the reusable
/// program. No workload executes inside the timer.
///
/// The gate still needs a projection, so one untimed execution runs after the
/// measured block. That run is correctness evidence, never a timing input.
fn lane_compile(
    request: &Request,
    source: &str,
    data: &Json,
    bars: usize,
) -> Result<Measured, String> {
    let (elapsed_ms, _) = timed(request.warmups, request.iterations, || {
        let program = compile_program(source)?;
        // The artifact is the product of this boundary. Touching its size keeps
        // the emitted program observable without paying for serialization,
        // which belongs to the artifact-size measurement rather than here.
        let instructions: usize = program.functions.iter().map(|f| f.code.len()).sum();
        Ok((Json::Null, instructions.to_le_bytes().to_vec()))
    })?;
    let program = compile_program(source)?;
    let result = run_program(&program, &convert(data)?, &RunOptions::default());
    require_clean(&result)?;
    Ok(Measured {
        elapsed_ms,
        iterations: request.iterations,
        result: projection(&result, bars),
    })
}

/// Cached full-data execution: the reusable compiled artifact is prepared once
/// outside the timer, and each repetition executes it over the full data set
/// with fresh market data, fresh execution state, and fresh budgets.
fn lane_cached(
    request: &Request,
    source: &str,
    data: &Json,
    bars: usize,
) -> Result<Measured, String> {
    let prepared = PreparedProgram::new(compile_program(source)?)?;
    let (elapsed_ms, result) = timed(request.warmups, request.iterations, || {
        let market = convert(data)?;
        let run = prepared.run_profiled(&market, &RunOptions::default(), None);
        require_clean(&run)?;
        materialize(projection(&run, bars))
    })?;
    Ok(Measured {
        elapsed_ms,
        iterations: request.iterations,
        result,
    })
}

/// Incremental bar: seed the declared history prefix outside the timer, then
/// time re-execution of one forming bar through that bar's projection.
///
/// The repetitions replay the same open tick rather than committing a
/// different bar each time. That is the session's own realtime path — each
/// advance rolls back to the pre-bar state and re-executes bar `bars - 1` — so
/// every repetition measures the same declared bar index instead of drifting
/// through the feed. The bar is committed once afterwards, outside the timer,
/// and the resulting full history is what the gate compares: the incremental
/// path has to land on the same series the batch modes produce.
///
/// The measured block goes through [`IncrementalSession::advance_samples`]
/// rather than `advance`, because `advance` answers with a snapshot of every
/// plot series from bar zero. Timing that snapshot makes this row scale with the
/// seeded prefix instead of measuring the bar: on a script whose per-bar work
/// is constant the optimized subject read 0.0025 ms at 1000 bars against
/// 0.045 ms at 20000, which is a measurement of result materialization wearing
/// the name of one bar of incremental execution.
fn lane_incremental(
    request: &Request,
    source: &str,
    data: &Json,
    bars: usize,
) -> Result<Measured, String> {
    if bars < 2 {
        return Err("incremental-bar mode needs at least two bars".into());
    }
    let program = compile_program(source)?;
    // Plot titles belong to the compiled program, not to any one bar, so they
    // are read once here and never re-derived inside the measured block.
    let titles: Vec<String> = program
        .plots
        .iter()
        .map(|plot| plot.title.clone().unwrap_or_default())
        .collect();
    let full = convert(data)?;
    let mut history = full.clone();
    truncate(&mut history, bars - 1);
    let options = RunOptions::default();
    let mut session = IncrementalSession::new(&program, &history, &options)
        .map_err(|error| format!("session: {error}"))?;
    let last = bars - 1;
    let tick = |is_bar_close| Tick {
        bar_index: last,
        open: full.open[last],
        high: full.high[last],
        low: full.low[last],
        close: full.close[last],
        volume: full.volume[last],
        time: full.time[last],
        trading_day: None,
        is_bar_close,
    };
    let (elapsed_ms, _) = timed(request.warmups, request.iterations, || {
        let samples = session
            .advance_samples(&[tick(false)])
            .map_err(|error| format!("runtime error: {}", error.message))?;
        let forming = samples
            .last()
            .ok_or("incremental advance returned no sample for its tick")?;
        materialize(forming_bar_projection(&titles, forming))
    })?;
    let committed = session.advance(&[tick(true)]);
    require_clean(&committed)?;
    Ok(Measured {
        elapsed_ms,
        iterations: request.iterations,
        result: projection(&committed, bars),
    })
}

/// Truncate every aligned series of a feed to `n` bars.
fn truncate(data: &mut MarketData, n: usize) {
    data.open.truncate(n);
    data.high.truncate(n);
    data.low.truncate(n);
    data.close.truncate(n);
    data.volume.truncate(n);
    data.time.truncate(n);
    data.trading_day.truncate(n);
}

/// Artifact size: write the canonical serialization of the reusable compiled
/// artifact so the harness can record exact uncompressed and compressed bytes.
///
/// This is not a timing mode. It is deterministic for a given source and
/// compiler revision, which is precisely why it makes a useful regression
/// check on ordinary shared runners where timing never would.
fn lane_artifact(
    request: &Request,
    source: &str,
    data: &Json,
    bars: usize,
) -> Result<Measured, String> {
    let destination = request
        .artifact_out
        .as_deref()
        .ok_or("artifact-size mode requires artifact_out")?;
    let program = compile_program(source)?;
    let bytes = serde_json::to_vec(&program).map_err(|error| error.to_string())?;
    fs::write(destination, &bytes).map_err(|error| format!("artifact `{destination}`: {error}"))?;
    let result = run_program(&program, &convert(data)?, &RunOptions::default());
    require_clean(&result)?;
    Ok(Measured {
        // No wall-clock claim is made for this measurement; the harness reads
        // the byte counts and never treats this as a timing sample.
        elapsed_ms: f64::NAN,
        iterations: 0,
        result: projection(&result, bars),
    })
}

fn measure(request: &Request) -> Result<Measured, String> {
    let source = fs::read_to_string(&request.source)
        .map_err(|error| format!("source `{}`: {error}", request.source))?;
    let (data, bars) = load_input(&request.input)?;
    match request.lane.as_str() {
        // The cold-process mode runs the cold body once; its published elapsed
        // time is the harness's process-boundary measurement, not this one.
        "cold-process" | "cold-runtime" => lane_cold(request, &source, &data, bars),
        "compile" => lane_compile(request, &source, &data, bars),
        "cached" => lane_cached(request, &source, &data, bars),
        "incremental-bar" => lane_incremental(request, &source, &data, bars),
        "artifact-size" => lane_artifact(request, &source, &data, bars),
        other => Err(format!("unimplemented lane `{other}`")),
    }
}

fn response(measured: Result<Measured, String>) -> Json {
    match measured {
        Ok(m) => {
            let mut object = Map::new();
            object.insert("status".into(), json!("ok"));
            if m.iterations > 0 {
                object.insert("elapsed_ms".into(), json!(m.elapsed_ms));
                object.insert("iterations".into(), json!(m.iterations));
                object.insert(
                    "per_iteration_ms".into(),
                    json!(m.elapsed_ms / m.iterations as f64),
                );
            }
            object.insert("result".into(), m.result);
            Json::Object(object)
        }
        Err(message) => json!({"status": "error", "message": message}),
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [request_path, response_path] = arguments.as_slice() else {
        eprintln!("pinewasm-benchmark-subject: {USAGE}");
        return ExitCode::FAILURE;
    };
    let request = match fs::read(request_path)
        .map_err(|error| format!("request `{request_path}`: {error}"))
        .and_then(|bytes| {
            serde_json::from_slice::<Request>(&bytes)
                .map_err(|error| format!("request `{request_path}`: {error}"))
        }) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("pinewasm-benchmark-subject: {message}");
            return ExitCode::FAILURE;
        }
    };
    let body = response(measure(&request));
    // A response that cannot be written is a crashed invocation, not a silent
    // pass: the harness records the missing file rather than inventing a
    // sample.
    match serde_json::to_vec(&body)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            fs::write(Path::new(response_path), bytes)
                .map_err(|error| format!("response `{response_path}`: {error}"))
        }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pinewasm-benchmark-subject: {message}");
            ExitCode::FAILURE
        }
    }
}
