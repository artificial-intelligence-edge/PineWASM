//! Rendering regression tests for the VM.
use pinewasm_compiler::{compile_and_run, CompileOptions, MarketData, RunOptions};
use serde::Deserialize;
#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    title: String,
    source: String,
    expected: String,
    #[serde(rename = "expectedCompileSuccess")]
    success: bool,
}
fn run(source: &str) -> pinewasm_compiler::RunResult {
    let data = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0; 3],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    result
}
fn check(title: &str) {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/plotchar_character_cases.json")).unwrap();
    let case = fixture.cases.iter().find(|v| v.title == title).unwrap();

    assert!(case.success);
    let result = run(&case.source);
    assert_eq!(result.plots.len(), 1);
    assert_eq!(
        result.plots[0].style.character.as_deref(),
        Some(case.expected.as_str())
    );
    assert_eq!(result.plots[0].style.text.as_deref(), Some("T"));
    assert!(!result.plots[0]
        .style
        .bound_scalars
        .iter()
        .any(|v| v.field == "char"));
}
#[test]
fn plotchar_direct_na() {
    check("direct-na");
}
#[test]
fn plotchar_literal_na() {
    check("literal-na");
}
#[test]
fn plotchar_literal_nan() {
    check("literal-NaN");
}
#[test]
fn plotchar_literal_long() {
    check("literal-long");
}
#[test]
fn plotchar_unicode_pair() {
    check("unicode-pair");
}
#[test]
fn plotchar_combining() {
    check("combining");
}
#[test]
fn plotchar_cast_na() {
    check("cast-na");
}
#[test]
fn omitted_character_stays_distinct_from_explicit_empty() {
    let omitted = run("//@version=6\nindicator(\"Omitted char\")\nplotchar(true,text=\"T\")");
    let empty = run("//@version=6\nindicator(\"Empty char\")\nplotchar(true,char=\"\",text=\"T\")");
    assert_eq!(omitted.plots[0].style.character, None);
    assert_eq!(empty.plots[0].style.character.as_deref(), Some(""));
}
#[test]
fn input_character_retains_its_bound_value_without_literal_folding() {
    let result=run("//@version=6\nindicator(\"Input char\")\nglyph=input.string(\"abc\")\nplotchar(true,char=glyph,text=\"TEXT\")");
    assert_eq!(result.plots[0].style.character, None);
    assert!(result.plots[0]
        .style
        .bound_scalars
        .iter()
        .any(|v| v.field == "char"));
    assert_eq!(result.plots[0].style.text.as_deref(), Some("TEXT"));
}

#[test]
fn plotchar_implicit_constant_alias() {
    check("implicit-alias");
}
#[test]
fn plotchar_explicit_constant_alias() {
    check("explicit-const-alias");
}
#[test]
fn plotchar_input_alias_keeps_full_combining_string() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/plotchar_character_cases.json")).unwrap();
    let case = fixture
        .cases
        .iter()
        .find(|v| v.title == "input-alias")
        .unwrap();

    assert!(case.success);
    let result = run(&case.source);
    assert_eq!(result.plots[0].style.character, None);
    let binding = result.plots[0]
        .style
        .bound_scalars
        .iter()
        .find(|v| v.field == "char")
        .unwrap();
    assert_eq!(
        serde_json::to_value(&binding.expression).unwrap(),
        serde_json::json!({"kind":"input", "input":0})
    );
    assert_eq!(
        serde_json::to_value(
            &pinewasm_compiler::compile(&case.source, CompileOptions::default())
                .program
                .unwrap()
                .inputs[0]
                .defval
        )
        .unwrap(),
        serde_json::json!({"String":case.expected})
    );
}

#[test]
fn literal_plotchar_preserves_explicit_presentation_selectors() {
    let source = "//@version=6\nindicator(\"Arrow presentation\")\n\
         const string emoji_arrow = \"⬇\u{fe0f}\"\n\
         plotchar(true, char=\"⬇\")\n\
         plotchar(true, char=\"⬇\u{fe0e}\")\n\
         plotchar(true, char=\"⬇\u{fe0f}\")\n\
         plotchar(true, char=emoji_arrow)\n";
    let result = run(source);
    let glyphs: Vec<_> = result
        .plots
        .iter()
        .map(|plot| plot.style.character.as_deref())
        .collect();
    assert_eq!(
        glyphs,
        [
            Some("⬇"),
            Some("⬇\u{fe0e}"),
            Some("⬇\u{fe0f}"),
            Some("⬇\u{fe0f}")
        ]
    );
}
