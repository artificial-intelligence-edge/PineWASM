//! Dev helper: compile one or more Pine files and print a compact JSON verdict
//! per file. Checks that each file compiles cleanly and, with --run, executes.
//! Not part of the shipped library surface.
//!
//!   pinewasm-check FILE [FILE ...]           # compile-only
//!   pinewasm-check --run FILE [FILE ...]     # compile + run on a tiny synthetic feed
//!   pinewasm-check --builtin-catalog         # print every builtin table as JSON
//!   pinewasm-check -h | --help               # print this message
//!
//! Output: one JSON object per line: {"file","ok","errors":[..],"warnings":[..]}

use pinewasm_compiler::{
    builtin_catalog, compile, compile_and_run, CompileOptions, MarketData, RunOptions,
};
use std::fs;
use std::process::ExitCode;

/// Kept verbatim in sync with the module docs above.
const USAGE: &str = "\
pinewasm-check FILE [FILE ...]           # compile-only
pinewasm-check --run FILE [FILE ...]     # compile + run on a tiny synthetic feed
pinewasm-check --builtin-catalog         # print every builtin table as JSON
pinewasm-check -h | --help               # print this message

Output: one JSON object per line: {\"file\",\"ok\",\"errors\":[..],\"warnings\":[..]}";

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn diag_list(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", json_escape(s)))
        .collect();
    format!("[{}]", parts.join(","))
}

fn synthetic_data(n: usize) -> MarketData {
    let mut open = Vec::with_capacity(n);
    let mut high = Vec::with_capacity(n);
    let mut low = Vec::with_capacity(n);
    let mut close = Vec::with_capacity(n);
    let mut volume = Vec::with_capacity(n);
    let mut time = Vec::with_capacity(n);
    for i in 0..n {
        let base = 100.0 + (i as f64) * 0.5;
        open.push(base);
        high.push(base + 1.0);
        low.push(base - 1.0);
        close.push(base + 0.25);
        volume.push(1000.0 + i as f64);
        time.push(1_000_000_000_000 + (i as i64) * 86_400_000);
    }
    MarketData {
        open,
        high,
        low,
        close,
        volume,
        time,
        ..MarketData::default()
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|a| a == "--builtin-catalog") {
        if args.len() > 1 {
            eprintln!("pinewasm-check: --builtin-catalog takes no files\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
        println!("{}", builtin_catalog());
        return ExitCode::SUCCESS;
    }
    let run = args.first().is_some_and(|a| a == "--run");
    if run {
        args.remove(0);
    }
    // Without this, a caller who forgot the file list gets a silent success.
    if args.is_empty() {
        eprintln!("pinewasm-check: no input files\n\n{USAGE}");
        return ExitCode::FAILURE;
    }
    let data = synthetic_data(60);
    let run_opts = RunOptions {
        max_collection_elements: 1_000_000,
        ..RunOptions::default()
    };
    let mut any_failed = false;

    for file in &args {
        let src = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                println!(
                    "{{\"file\":\"{}\",\"ok\":false,\"errors\":[\"read error: {}\"],\"warnings\":[]}}",
                    json_escape(file),
                    json_escape(&e.to_string())
                );
                any_failed = true;
                continue;
            }
        };
        let compiled = compile(&src, CompileOptions::all());
        let errors: Vec<String> = compiled
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| format!("{}: {}", d.code.clone().unwrap_or_default(), d.message))
            .collect();
        let warnings: Vec<String> = compiled
            .diagnostics
            .iter()
            .filter(|d| !d.is_error())
            .map(|d| format!("{}: {}", d.code.clone().unwrap_or_default(), d.message))
            .collect();
        let mut ok = compiled.success && errors.is_empty();
        let mut extra = String::new();
        if ok && run {
            let r = compile_and_run(&src, &data, CompileOptions::default(), &run_opts);
            if !r.runtime_errors.is_empty() {
                ok = false;
                let rt: Vec<String> = r.runtime_errors.iter().map(|e| format!("{e:?}")).collect();
                extra = format!(",\"runtime\":{}", diag_list(&rt));
            } else {
                extra = format!(",\"bars\":{}", r.bars_executed);
            }
        }
        println!(
            "{{\"file\":\"{}\",\"ok\":{},\"errors\":{},\"warnings\":{}{}}}",
            json_escape(file),
            ok,
            diag_list(&errors),
            diag_list(&warnings),
            extra
        );
        any_failed |= !ok;
    }
    if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
