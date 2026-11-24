//! Contributor tool: profile the compile/run pipeline for one Pine
//! script and print a per-stage wall-clock breakdown. Companion to `pinewasm-check`
//! (which verifies), this answers "where does the time go?" for a heavyweight
//! script. It is maintained and integration-tested as a workspace binary, but
//! it is not part of the compiler library API or a stable benchmark format.
//!
//!   `pinewasm-profile [OPTIONS] FILE`
//!
//!   --lib PATH=FILE     importable library (repeatable), e.g.
//!                       --lib example/MyLib/1=libraries/mylib.pine
//!   --csv FILE          OHLCV feed (`time,open,high,low,close[,volume]`);
//!                       standard CSV quoting supported; synthetic 6000-bar
//!                       feed when omitted
//!   --bars N            truncate the feed to its first N bars
//!   --repeat N          compile/run N times, report medians (default 3)
//!   --dumps             request every debug dump (AST/HIR/MIR/CSIL)
//!   --no-run            compile only
//!   -h, --help          print this message
//!
//! Output: a human-readable table plus one `pinewasm-profile-json:{...}` line for
//! scripting.

use pinewasm_compiler::{
    compile_and_run_with_libraries, compile_with_libraries, CompileOptions, CompileResult,
    MarketData, PhaseTimings, RunOptions,
};
use serde::Serialize;
use std::fs;
use std::process::ExitCode;

/// Kept verbatim in sync with the module docs above: this is what `--help`
/// prints and what a parse error points the caller back to.
const USAGE: &str = "\
pinewasm-profile [OPTIONS] FILE

  --lib PATH=FILE     importable library (repeatable), e.g.
                      --lib example/MyLib/1=libraries/mylib.pine
  --csv FILE          OHLCV feed (time,open,high,low,close[,volume]);
                      standard CSV quoting supported; synthetic 6000-bar
                      feed when omitted
  --bars N            truncate the feed to its first N bars
  --repeat N          compile/run N times, report medians (default 3)
  --dumps             request every debug dump (AST/HIR/MIR/CSIL)
  --no-run            compile only
  -h, --help          print this message";

struct Args {
    file: String,
    libs: Vec<(String, String)>,
    csv: Option<String>,
    bars: Option<usize>,
    repeat: usize,
    dumps: bool,
    run: bool,
}

/// `--help` is not a degenerate profiling run, so it does not travel as an
/// `Args` with an empty file.
enum Invocation {
    Profile(Args),
    Help,
}

fn parse_args() -> Result<Invocation, String> {
    let mut args = Args {
        file: String::new(),
        libs: Vec::new(),
        csv: None,
        bars: None,
        repeat: 3,
        dumps: false,
        run: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "--lib" => {
                let spec = it.next().ok_or("--lib needs PATH=FILE")?;
                let (path, file) = spec
                    .split_once('=')
                    .ok_or_else(|| format!("--lib `{spec}` is not PATH=FILE"))?;
                let source =
                    fs::read_to_string(file).map_err(|e| format!("library file `{file}`: {e}"))?;
                args.libs.push((path.to_string(), source));
            }
            "--csv" => args.csv = Some(it.next().ok_or("--csv needs FILE")?),
            "--bars" => {
                let n = it.next().ok_or("--bars needs N")?;
                args.bars = Some(n.parse().map_err(|e| format!("--bars `{n}`: {e}"))?);
            }
            "--repeat" => {
                let n = it.next().ok_or("--repeat needs N")?;
                args.repeat = n.parse().map_err(|e| format!("--repeat `{n}`: {e}"))?;
                if args.repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "--dumps" => args.dumps = true,
            "--no-run" => args.run = false,
            _ if arg.starts_with("--") => return Err(format!("unknown option `{arg}`")),
            _ if args.file.is_empty() => args.file = arg,
            _ => return Err(format!("unexpected extra argument `{arg}`")),
        }
    }
    if args.file.is_empty() {
        return Err(format!("no input file\n\n{USAGE}"));
    }
    Ok(Invocation::Profile(args))
}

/// Load a `time,open,high,low,close[,volume]` CSV (header required; seconds
/// or milliseconds timestamps). The `csv` crate handles quoting, escaped
/// delimiters, CRLF input, and record-length validation; this helper only
/// supplies the market-data schema and numerical conversions.
fn load_csv(path: &str) -> Result<MarketData, String> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|error| format!("csv `{path}`: {error}"))?;
    let headers = reader
        .headers()
        .map_err(|error| format!("csv `{path}`: {error}"))?
        .clone();
    let col = |name: &str| {
        headers
            .iter()
            .position(|header| header == name)
            .ok_or_else(|| format!("csv `{path}`: missing `{name}` column"))
    };
    let (time_col, open_col, high_col, low_col, close_col) = (
        col("time")?,
        col("open")?,
        col("high")?,
        col("low")?,
        col("close")?,
    );
    let volume_col = headers.iter().position(|header| header == "volume");

    let mut data = MarketData::default();
    for (index, record) in reader.records().enumerate() {
        let record =
            record.map_err(|error| format!("csv `{path}` record {}: {error}", index + 1))?;
        let get = |column: usize, name: &str| -> Result<f64, String> {
            record
                .get(column)
                .ok_or_else(|| format!("csv `{path}` record {}: missing {name}", index + 1))?
                .parse::<f64>()
                .map_err(|error| format!("csv `{path}` record {}: bad {name}: {error}", index + 1))
        };
        let raw_time = get(time_col, "time")? as i64;
        data.time.push(if raw_time.abs() < 10_000_000_000 {
            raw_time * 1000
        } else {
            raw_time
        });
        data.open.push(get(open_col, "open")?);
        data.high.push(get(high_col, "high")?);
        data.low.push(get(low_col, "low")?);
        data.close.push(get(close_col, "close")?);
        data.volume
            .push(volume_col.map_or(Ok(0.0), |c| get(c, "volume"))?);
    }
    Ok(data)
}

fn synthetic_data(n: usize) -> MarketData {
    let mut data = MarketData::default();
    for i in 0..n {
        // Deterministic wobble so indicators have signal to chew on.
        let base = 100.0 + (i as f64) * 0.05 + ((i as f64) * 0.7).sin() * 2.0;
        data.open.push(base);
        data.high.push(base + 1.0);
        data.low.push(base - 1.0);
        data.close.push(base + ((i as f64) * 1.3).cos() * 0.5);
        data.volume.push(1000.0 + (i % 97) as f64 * 10.0);
        data.time.push(1_000_000_000_000 + (i as i64) * 86_400_000);
    }
    data
}

fn truncate(data: &mut MarketData, n: usize) {
    data.open.truncate(n);
    data.high.truncate(n);
    data.low.truncate(n);
    data.close.truncate(n);
    data.volume.truncate(n);
    data.time.truncate(n);
}

/// Median by sort; input order is scratch.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn print_row(label: &str, ms: f64, total: f64) {
    let pct = if total > 0.0 { ms / total * 100.0 } else { 0.0 };
    let bar_len = (pct / 2.0).round() as usize;
    println!(
        "  {label:<12} {ms:>10.2} ms  {pct:>5.1}%  {}",
        "#".repeat(bar_len.min(50))
    );
}

fn program_stats(compiled: &CompileResult) -> (usize, usize) {
    compiled.program.as_ref().map_or((0, 0), |p| {
        (
            p.functions.len(),
            p.functions.iter().map(|f| f.code.len()).sum(),
        )
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileRecord<'a> {
    file: &'a str,
    parse_ms: f64,
    libraries_ms: f64,
    semantics_ms: f64,
    mir_ms: f64,
    #[serde(rename = "ilMs")]
    csil_ms: f64,
    validate_ms: f64,
    dumps_ms: f64,
    compile_ms: f64,
    run_ms: f64,
    functions: usize,
    instructions: usize,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Invocation::Profile(a)) => a,
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("pinewasm-profile: {e}");
            return ExitCode::FAILURE;
        }
    };
    let source = match fs::read_to_string(&args.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pinewasm-profile: `{}`: {e}", args.file);
            return ExitCode::FAILURE;
        }
    };
    let mut data = match &args.csv {
        Some(path) => match load_csv(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("pinewasm-profile: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => synthetic_data(6000),
    };
    if let Some(n) = args.bars {
        truncate(&mut data, n);
    }

    let compile_options = if args.dumps {
        CompileOptions::all()
    } else {
        CompileOptions::default()
    };
    let run_options = RunOptions {
        max_collection_elements: 100_000_000,
        ..RunOptions::default()
    };

    println!(
        "pinewasm-profile: {} ({} bytes, {} libraries, {} bars, repeat={})",
        args.file,
        source.len(),
        args.libs.len(),
        data.close.len(),
        args.repeat
    );

    let mut compile_timings: Vec<PhaseTimings> = Vec::new();
    let mut run_times: Vec<f64> = Vec::new();
    let mut last: Option<CompileResult> = None;
    let mut compile_failed = false;
    let mut runtime_failed = false;
    for _ in 0..args.repeat {
        if args.run {
            let result = compile_and_run_with_libraries(
                &source,
                &args.libs,
                &data,
                compile_options,
                &run_options,
            );
            runtime_failed |= !result.runtime_errors.is_empty();
            for err in &result.runtime_errors {
                eprintln!("pinewasm-profile: runtime error: {}", err.message);
            }
            if let Some(ms) = result.run_ms {
                run_times.push(ms);
            }
            let compiled = result.compile.expect("compile section always present");
            compile_failed |= !compiled.success;
            if let Some(t) = compiled.timings {
                compile_timings.push(t);
            }
            last = Some(compiled);
        } else {
            let compiled = compile_with_libraries(&source, &args.libs, compile_options);
            compile_failed |= !compiled.success;
            if let Some(t) = compiled.timings {
                compile_timings.push(t);
            }
            last = Some(compiled);
        }
    }

    let last = last.expect("at least one iteration ran");
    for d in last.diagnostics.iter().filter(|d| d.is_error()) {
        eprintln!(
            "pinewasm-profile: error {}: {}",
            d.code.clone().unwrap_or_default(),
            d.message
        );
    }
    if !last.success {
        eprintln!("pinewasm-profile: compile failed; timings cover the stages that ran");
    }

    // Median per stage across repeats (medians of stages need not sum to the
    // median total; close enough for a breakdown).
    let stage = |f: fn(&PhaseTimings) -> f64| -> f64 {
        let mut v: Vec<f64> = compile_timings.iter().map(f).collect();
        if v.is_empty() {
            0.0
        } else {
            median(&mut v)
        }
    };
    let parse = stage(|t| t.parse_ms);
    let libraries = stage(|t| t.libraries_ms);
    let semantics = stage(|t| t.semantics_ms);
    let mir = stage(|t| t.mir_ms);
    let csil = stage(|t| t.csil_ms);
    let validate = stage(|t| t.validate_ms);
    let dumps = stage(|t| t.dumps_ms);
    let compile_total = stage(|t| t.total_ms);
    let run_ms = if run_times.is_empty() {
        0.0
    } else {
        median(&mut run_times)
    };
    let grand_total = compile_total + run_ms;

    let (fn_count, inst_count) = program_stats(&last);
    println!("  program: {fn_count} functions, {inst_count} CSIL instructions");
    println!("  --- median of {} runs ---", args.repeat.max(1));
    print_row("parse", parse, grand_total);
    print_row("libraries", libraries, grand_total);
    print_row("semantics", semantics, grand_total);
    print_row("mir", mir, grand_total);
    print_row("csil", csil, grand_total);
    print_row("validate", validate, grand_total);
    if args.dumps {
        print_row("dumps", dumps, grand_total);
    }
    print_row("compile", compile_total, grand_total);
    if args.run {
        print_row("vm run", run_ms, grand_total);
    }
    println!("  {:<12} {grand_total:>10.2} ms", "TOTAL");

    // Machine-readable line for benchmark-diff tooling.
    let record = ProfileRecord {
        file: &args.file,
        parse_ms: parse,
        libraries_ms: libraries,
        semantics_ms: semantics,
        mir_ms: mir,
        csil_ms: csil,
        validate_ms: validate,
        dumps_ms: dumps,
        compile_ms: compile_total,
        run_ms,
        functions: fn_count,
        instructions: inst_count,
    };
    println!(
        "pinewasm-profile-json:{}",
        serde_json::to_string(&record).expect("profile record is valid JSON")
    );

    if compile_failed || runtime_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
