//! Full-length script tier: purpose-written v5 scripts in `tests/scripts/`
//! that exercise many rules at once, the way a real script does.
//!
//! Each script `name` has three checked-in files:
//!
//! - `name.v5.pine`: the input, written for PineWASM (see the README there).
//! - `name.v6.pine`: the exact expected output of the default migration.
//! - `name.findings`: one line per finding, `L<line> <rule> <class>
//!   <severity> <applied|review>`, in the order the migrator reports them,
//!   so a change to any review site shows up in the diff.
//!
//! For every script the test asserts: the status is `Migrated`; there is no
//! `internal` finding; the output and the findings match the checked-in
//! files; the reported edits replay to the output; migrating the output
//! again is `AlreadyV6`; the output parses without errors; and the compile
//! expectation below holds. A script marked [`Compile::Yes`] must compile
//! cleanly with PineWASM. A script marked [`Compile::No`] must still fail to
//! compile, for the stated reason, so the exclusion is revisited as soon as
//! PineWASM supports the construct.
//!
//! Regenerate the expected files with `PINEWASM_BLESS=1 cargo test -p
//! pinewasm_migrate --test scripts`, then review every changed `.v6.pine`
//! against docs/migration.md by hand before committing it.

use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_migrate::{migrate, Finding, MigrationOptions, RuleClass, Severity, Status};
use pretty_assertions::assert_eq;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Whether PineWASM is expected to compile a script's v6 output.
#[derive(Clone, Copy)]
enum Compile {
    Yes,
    /// PineWASM does not support something the script needs yet. The
    /// string names the construct and the compiler diagnostic code it
    /// reports, which the test checks.
    No(&'static str, &'static str),
}

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts")
}

fn blessing() -> bool {
    std::env::var_os("PINEWASM_BLESS").is_some_and(|v| v != "0")
}

fn class(class: RuleClass) -> &'static str {
    match class {
        RuleClass::Exact => "exact",
        RuleClass::ExactWithHoisting => "exactWithHoisting",
        RuleClass::Approximate => "approximate",
        RuleClass::Diagnostic => "diagnostic",
    }
}

fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

fn snapshot(source: &str, findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let line = source[..f.start as usize].matches('\n').count() + 1;
        writeln!(
            out,
            "L{line} {} {} {} {}",
            f.rule,
            class(f.class),
            severity(f.severity),
            if f.applied { "applied" } else { "review" }
        )
        .unwrap();
    }
    out
}

fn check_expected(path: &Path, actual: &str) {
    if blessing() {
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with PINEWASM_BLESS=1)", path.display()));
    assert_eq!(expected, actual, "{}", path.display());
}

fn check(name: &str, expectation: Compile) {
    let dir = directory();
    let input_path = dir.join(format!("{name}.v5.pine"));
    let input = std::fs::read_to_string(&input_path)
        .unwrap_or_else(|e| panic!("{}: {e}", input_path.display()));
    let options = MigrationOptions::default();
    let result = migrate(&input, &options);
    assert_eq!(result.status, Status::Migrated, "{:?}", result.parse_errors);
    assert!(
        !result.findings.iter().any(|f| f.rule == "internal"),
        "{:?}",
        result.findings
    );

    check_expected(&dir.join(format!("{name}.v6.pine")), &result.output);
    check_expected(
        &dir.join(format!("{name}.findings")),
        &snapshot(&input, &result.findings),
    );

    // The reported edits reproduce the output.
    let mut replay = input.clone();
    for edit in result.edits.iter().rev() {
        replay.replace_range(edit.start as usize..edit.end as usize, &edit.new_text);
    }
    assert_eq!(replay, result.output);

    // The output is v6 and needs nothing further.
    let again = migrate(&result.output, &options);
    assert_eq!(again.status, Status::AlreadyV6);
    assert!(again.edits.is_empty());

    let parse_errors: Vec<_> = pinewasm_parse::parse(&result.output)
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == pinewasm_span::Severity::Error)
        .collect();
    assert!(parse_errors.is_empty(), "{parse_errors:?}");

    let errors: Vec<_> = compile(&result.output, CompileOptions::default())
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == pinewasm_span::Severity::Error)
        .collect();
    match expectation {
        Compile::Yes => assert!(errors.is_empty(), "{errors:?}"),
        Compile::No(reason, code) => assert!(
            errors.iter().any(|d| d.code.as_deref() == Some(code)),
            "{name} is excluded from compilation because {reason} ({code}), but PineWASM \
             no longer reports {code}: mark it Compile::Yes if it now compiles. {errors:?}"
        ),
    }
}

macro_rules! scripts {
    ($($name:ident: $compile:expr,)*) => {
        $(
            #[test]
            fn $name() {
                check(stringify!($name), $compile);
            }
        )*

        const NAMES: &[&str] = &[$(stringify!($name)),*];
    };
}

scripts! {
    alert_scanner: Compile::Yes,
    atr_trailing_strategy: Compile::Yes,
    breakout_pyramid: Compile::Yes,
    candle_patterns: Compile::Yes,
    correlation_heatmap: Compile::Yes,
    crossover_strategy: Compile::Yes,
    grid_reversion: Compile::Yes,
    market_structure: Compile::Yes,
    mtf_ribbon: Compile::Yes,
    // The v5 `while` condition calls `ta.sma`; the migrated loop evaluates
    // it at the top of the body, and PineWASM rejects stateful calls in
    // loop bodies.
    order_flow_meter: Compile::No("a stateful call runs inside a loop body", "E0222"),
    pivot_levels: Compile::Yes,
    regime_styles: Compile::Yes,
    regression_channel: Compile::Yes,
    rsi_divergence: Compile::Yes,
    seasonality_table: Compile::Yes,
    // The weekly request's expression becomes a user function (its `and`
    // operand is hoisted into the requested context), which PineWASM's
    // secondary request context cannot evaluate yet.
    sector_rotation: Compile::No("a request expression calls a user function", "E0244"),
    squeeze_bands: Compile::Yes,
    toolkit_library: Compile::Yes,
    trend_dashboard: Compile::Yes,
    volume_profile: Compile::Yes,
}

/// Every input in the directory has a test, and every test has an input
/// and both expected files.
#[test]
fn every_script_is_listed() {
    let mut found: Vec<String> = std::fs::read_dir(directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    let mut expected: Vec<String> = NAMES
        .iter()
        .flat_map(|n| {
            [
                format!("{n}.findings"),
                format!("{n}.v5.pine"),
                format!("{n}.v6.pine"),
            ]
        })
        .chain(["README.md".to_string()])
        .collect();
    expected.sort();
    if blessing() {
        // Blessing writes the expected files; only the inputs must exist.
        found.retain(|f| f.ends_with(".v5.pine"));
        expected.retain(|f| f.ends_with(".v5.pine"));
    }
    assert_eq!(found, expected);
}
