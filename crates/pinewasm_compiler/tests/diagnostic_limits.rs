//! One compile reports a bounded list of diagnostics.
//!
//! A source can be wrong, or merely noisy, in far more places than anyone
//! reads: a megabyte of stray characters is half a million errors. A compile
//! reports the first 1,000 errors and the first 1,000 warnings and ends the
//! list with an `E0002` or a `W0007` when it left some out. What it leaves
//! out never changes whether the compile succeeds.
//!
//! The limits are defined here as constants: these are the numbers a
//! host sees.

use pinewasm_compiler::{
    compile, compile_and_run_with_libraries, compile_with_libraries, CompileOptions, CompileResult,
    Diagnostic, MarketData, RunOptions, Severity, SourceId,
};

const MAX_ERRORS: usize = 1000;
const MAX_WARNINGS: usize = 1000;

const HEADER: &str = "//@version=6\nindicator(\"limits\")\n";

/// One error for each repetition: an unknown identifier.
const ERROR: &str = "nope\n";

/// One warning for each repetition: an expression whose result is discarded.
const WARNING: &str = "close\n";

fn compiled(source: &str) -> CompileResult {
    compile(source, CompileOptions::default())
}

fn count(diagnostics: &[Diagnostic], severity: Severity) -> usize {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == severity)
        .count()
}

fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_deref().unwrap_or("none"))
        .collect()
}

fn count_code(diagnostics: &[Diagnostic], code: &str) -> usize {
    codes(diagnostics)
        .into_iter()
        .filter(|found| *found == code)
        .count()
}

/// The list ends by saying errors were left out, in the words a host shows.
fn assert_ends_with_too_many_errors(diagnostics: &[Diagnostic]) {
    let last = diagnostics.last().expect("a diagnostic");
    assert_eq!(last.severity, Severity::Error);
    assert_eq!(last.code.as_deref(), Some("E0002"));
    assert_eq!(
        last.message,
        "too many errors: only the first 1000 are reported"
    );
    assert!(last.labels.is_empty());
    assert_eq!(count_code(diagnostics, "E0002"), 1);
    assert_eq!(count(diagnostics, Severity::Error), MAX_ERRORS + 1);
}

fn assert_ends_with_too_many_warnings(diagnostics: &[Diagnostic]) {
    let last = diagnostics.last().expect("a diagnostic");
    assert_eq!(last.severity, Severity::Warning);
    assert_eq!(last.code.as_deref(), Some("W0007"));
    assert_eq!(
        last.message,
        "too many warnings: only the first 1000 are reported"
    );
    assert!(last.labels.is_empty());
    assert_eq!(count_code(diagnostics, "W0007"), 1);
    assert_eq!(count(diagnostics, Severity::Warning), MAX_WARNINGS + 1);
}

#[test]
fn a_megabyte_of_mistakes_is_one_short_list_at_every_stage() {
    // Stray characters (the lexer), malformed lines (the parser) and unknown
    // names (the analyzer), each filling the 1 MiB a source may be.
    let megabyte = 1 << 20;
    for (unit, first) in [("$ ", "E0101"), ("x 1\n", "E0110"), ("a\n", "E0201")] {
        let result = compiled(&unit.repeat((megabyte - 2) / unit.len()));
        assert!(!result.success, "{unit:?}");
        assert!(result.program.is_none(), "{unit:?}");
        assert_eq!(result.diagnostics[0].code.as_deref(), Some(first));
        assert_ends_with_too_many_errors(&result.diagnostics);
        // The errors, the mark, and at most the warning that the script
        // declares no indicator.
        assert!(
            result.diagnostics.len() <= MAX_ERRORS + 2,
            "{unit:?}: {}",
            result.diagnostics.len()
        );
    }
}

#[test]
fn a_list_exactly_at_the_limit_is_whole_and_says_nothing_more() {
    let at_limit = compiled(&format!("{HEADER}{}", ERROR.repeat(MAX_ERRORS)));
    assert!(!at_limit.success);
    assert_eq!(at_limit.diagnostics.len(), MAX_ERRORS);
    assert_eq!(count_code(&at_limit.diagnostics, "E0201"), MAX_ERRORS);

    let over = compiled(&format!("{HEADER}{}", ERROR.repeat(MAX_ERRORS + 1)));
    assert_eq!(over.diagnostics[..MAX_ERRORS], at_limit.diagnostics);
    assert_eq!(over.diagnostics.len(), MAX_ERRORS + 1);
    assert_ends_with_too_many_errors(&over.diagnostics);

    let at_limit = compiled(&format!(
        "{HEADER}{}plot(close)\n",
        WARNING.repeat(MAX_WARNINGS)
    ));
    assert!(at_limit.success, "{:?}", at_limit.diagnostics.first());
    assert_eq!(at_limit.diagnostics.len(), MAX_WARNINGS);
    assert_eq!(count_code(&at_limit.diagnostics, "W0002"), MAX_WARNINGS);

    let over = compiled(&format!(
        "{HEADER}{}plot(close)\n",
        WARNING.repeat(MAX_WARNINGS + 1)
    ));
    assert_eq!(over.diagnostics[..MAX_WARNINGS], at_limit.diagnostics);
    assert_eq!(over.diagnostics.len(), MAX_WARNINGS + 1);
    assert_ends_with_too_many_warnings(&over.diagnostics);
}

#[test]
fn a_noisy_script_still_compiles_and_runs() {
    // Far more warnings than are reported, and nothing wrong.
    let source = format!("{HEADER}{}plot(close + 1)\n", WARNING.repeat(40_000));
    let data = MarketData {
        open: vec![1.0, 2.0, 3.0],
        high: vec![1.0, 2.0, 3.0],
        low: vec![1.0, 2.0, 3.0],
        close: vec![1.0, 2.0, 3.0],
        volume: vec![1.0, 1.0, 1.0],
        ..MarketData::default()
    };
    let run = compile_and_run_with_libraries(
        &source,
        &[],
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let compile = run.compile.as_ref().expect("the compile result");
    assert!(compile.success);
    assert!(compile.program.is_some());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, 3);
    assert_eq!(run.plots.len(), 1);

    // The run carries the same short list as the compile.
    assert_eq!(run.diagnostics, compile.diagnostics);
    assert_eq!(run.diagnostics.len(), MAX_WARNINGS + 1);
    assert_ends_with_too_many_warnings(&run.diagnostics);
    assert_eq!(count(&run.diagnostics, Severity::Error), 0);
}

#[test]
fn warnings_never_hide_an_error_and_errors_never_hide_a_warning() {
    // One mistake after a flood of warnings: the compile fails and says why.
    let result = compiled(&format!(
        "{HEADER}{}{ERROR}plot(close)\n",
        WARNING.repeat(5 * MAX_WARNINGS)
    ));
    assert!(!result.success);
    assert!(result.program.is_none());
    assert_eq!(count_code(&result.diagnostics, "E0201"), 1);
    assert_eq!(count(&result.diagnostics, Severity::Error), 1);
    assert_ends_with_too_many_warnings(&result.diagnostics);

    // A few warnings after a flood of errors are all reported.
    let result = compiled(&format!(
        "{HEADER}{}{}",
        ERROR.repeat(5 * MAX_ERRORS),
        WARNING.repeat(3)
    ));
    assert!(!result.success);
    assert_eq!(count_code(&result.diagnostics, "W0002"), 3);
    assert_ends_with_too_many_errors(&result.diagnostics);

    // Both run out: one mark each, errors first, after everything else.
    let result = compiled(&format!(
        "{HEADER}{}{}",
        ERROR.repeat(MAX_ERRORS + 7),
        WARNING.repeat(MAX_WARNINGS + 7)
    ));
    assert!(!result.success);
    assert_eq!(result.diagnostics.len(), MAX_ERRORS + MAX_WARNINGS + 2);
    assert_eq!(
        codes(&result.diagnostics[MAX_ERRORS + MAX_WARNINGS..]),
        ["E0002", "W0007"]
    );
    assert_eq!(count_code(&result.diagnostics, "E0201"), MAX_ERRORS);
    assert_eq!(count_code(&result.diagnostics, "W0002"), MAX_WARNINGS);
}

#[test]
fn the_stages_of_one_compile_share_one_limit() {
    // 599 warnings from the parser (a repeated version directive) and 600
    // from the analyzer: neither stage runs out alone, the compile does.
    let source = format!(
        "{}indicator(\"limits\")\n{}plot(close)\n",
        "//@version=6\n".repeat(600),
        WARNING.repeat(600)
    );
    let result = compiled(&source);
    assert!(result.success, "{:?}", result.diagnostics.first());
    assert_eq!(count_code(&result.diagnostics, "W0005"), 599);
    assert_eq!(count_code(&result.diagnostics, "W0002"), MAX_WARNINGS - 599);
    assert_eq!(result.diagnostics.len(), MAX_WARNINGS + 1);
    assert_ends_with_too_many_warnings(&result.diagnostics);
    // The parser's come first, in the order the stages ran.
    assert!(result.diagnostics[..599]
        .iter()
        .all(|diagnostic| diagnostic.code.as_deref() == Some("W0005")));

    // Each stage runs out on its own: still one mark, not one for each.
    let source = format!(
        "{}indicator(\"limits\")\n{}plot(close)\n",
        "//@version=6\n".repeat(MAX_WARNINGS + 50),
        WARNING.repeat(MAX_WARNINGS + 50)
    );
    let result = compiled(&source);
    assert!(result.success);
    assert_eq!(count_code(&result.diagnostics, "W0005"), MAX_WARNINGS);
    assert_eq!(count_code(&result.diagnostics, "W0002"), 0);
    assert_eq!(result.diagnostics.len(), MAX_WARNINGS + 1);
    assert_ends_with_too_many_warnings(&result.diagnostics);
}

const IMPORTER: &str = "//@version=6\nindicator(\"Importer\")\n";

#[test]
fn a_library_full_of_mistakes_is_reported_within_the_same_limit() {
    let library = format!(
        "//@version=6\nlibrary(\"Broken\")\n{}",
        "x 1\n".repeat(3 * MAX_ERRORS)
    );
    let main = format!("{IMPORTER}import me/Broken/1 as broken\nplot(close)\n");
    let libraries = vec![("me/Broken/1".to_string(), library)];
    let result = compile_with_libraries(&main, &libraries, CompileOptions::default());
    assert!(!result.success);
    // The note at the import comes first, so a host that only shows the
    // main buffer still sees why the compile failed.
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0243"));
    assert_eq!(
        result.diagnostics[0].message,
        "library `me/Broken/1` failed to parse"
    );
    assert!(result.diagnostics[1..MAX_ERRORS]
        .iter()
        .all(|diagnostic| diagnostic.code.as_deref() == Some("E0110")
            && diagnostic.primary_span().map(|span| span.source) == Some(SourceId(1))));
    assert_eq!(result.diagnostics.len(), MAX_ERRORS + 1);
    assert_ends_with_too_many_errors(&result.diagnostics);
}

#[test]
fn several_broken_libraries_share_the_limit_of_the_compile_that_imports_them() {
    let mut main = IMPORTER.to_string();
    let mut libraries = Vec::new();
    for n in 0..4 {
        main.push_str(&format!("import me/Broken{n}/1 as broken{n}\n"));
        libraries.push((
            format!("me/Broken{n}/1"),
            format!(
                "//@version=6\nlibrary(\"Broken{n}\")\n{}",
                "x 1\n".repeat(600)
            ),
        ));
    }
    main.push_str("plot(close)\n");
    let result = compile_with_libraries(&main, &libraries, CompileOptions::default());
    assert!(!result.success);
    assert_eq!(result.diagnostics.len(), MAX_ERRORS + 1);
    assert_ends_with_too_many_errors(&result.diagnostics);
    // The first library whole (its note and 600 errors), then as much of
    // the second as there is room for.
    assert_eq!(
        result.diagnostics[601].message,
        "library `me/Broken1/1` failed to parse"
    );
    assert_eq!(count_code(&result.diagnostics, "E0243"), 2);
    assert_eq!(count_code(&result.diagnostics, "E0110"), MAX_ERRORS - 2);
}

#[test]
fn a_library_that_redefines_a_method_a_thousand_times_is_one_short_list() {
    let library = format!(
        "//@version=6\nlibrary(\"Repeats\")\n{}",
        "export method m(array<float> a) => a.sum()\n".repeat(MAX_ERRORS + 100)
    );
    let main = format!("{IMPORTER}import me/Repeats/1 as repeats\nplot(close)\n");
    let libraries = vec![("me/Repeats/1".to_string(), library)];
    let result = compile_with_libraries(&main, &libraries, CompileOptions::default());
    assert!(!result.success);
    assert_eq!(count_code(&result.diagnostics, "E0224"), MAX_ERRORS);
    assert_eq!(result.diagnostics.len(), MAX_ERRORS + 1);
    assert_ends_with_too_many_errors(&result.diagnostics);
}

#[test]
fn the_marks_reach_a_host_as_ordinary_diagnostics() {
    let result = compiled(&format!(
        "{HEADER}{}{}",
        ERROR.repeat(MAX_ERRORS + 1),
        WARNING.repeat(MAX_WARNINGS + 1)
    ));
    let wire = serde_json::to_value(&result).expect("a compile result serializes");
    let diagnostics = wire["diagnostics"].as_array().expect("a diagnostics array");
    assert_eq!(diagnostics.len(), MAX_ERRORS + MAX_WARNINGS + 2);
    let errors = &diagnostics[diagnostics.len() - 2];
    assert_eq!(
        *errors,
        serde_json::json!({
            "severity": "error",
            "code": "E0002",
            "message": "too many errors: only the first 1000 are reported",
            "labels": [],
            "notes": ["fix the errors reported and compile again to see any that remain"],
        })
    );
    let warnings = &diagnostics[diagnostics.len() - 1];
    assert_eq!(
        *warnings,
        serde_json::json!({
            "severity": "warning",
            "code": "W0007",
            "message": "too many warnings: only the first 1000 are reported",
            "labels": [],
            "notes": ["resolve the warnings reported and compile again to see any that remain"],
        })
    );
    assert_eq!(wire["success"], false);
}
