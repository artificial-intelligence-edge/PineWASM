//! A warning from an inner check never takes the place of a required error.
//!
//! Several rules evaluate an argument and report their own error only when
//! the evaluation reported none, so that one mistake is not reported twice.
//! A warning is not that mistake: an argument whose helper function also has
//! a discarded expression must fail exactly as it fails without one.

use crate::analyze;
use pinewasm_parse::parse;
use pinewasm_span::{codes, has_errors, Diagnostic, Severity};

/// Each case: the value the helper `v()` returns, the rest of the script,
/// and the error the script must report.
const CASES: &[(&str, &str, &str)] = &[
    (
        "string s = na\n    s",
        "library(v())\nexport h() => 1\n",
        codes::E0205,
    ),
    (
        "string s = na\n    s",
        "indicator(v())\nplot(close)\n",
        codes::E0205,
    ),
    (
        "string s = na\n    s",
        "indicator(\"x\", scale = v())\nplot(close)\n",
        codes::E0205,
    ),
    (
        "string s = na\n    s",
        "strategy(\"x\", currency = v())\nplot(close)\n",
        codes::E0205,
    ),
    (
        "int s = na\n    s",
        "indicator(\"x\", max_bars_back = v())\nplot(close)\n",
        codes::E0219,
    ),
    (
        "float s = na\n    s",
        "strategy(\"x\", initial_capital = v())\nplot(close)\n",
        codes::E0219,
    ),
    (
        "bool(na)",
        "strategy(\"x\", calc_on_every_tick = v())\nplot(close)\n",
        codes::E0219,
    ),
    (
        "int s = na\n    s",
        "indicator(\"x\")\nplot(input.int(v()))\n",
        codes::E0230,
    ),
    (
        "string s = na\n    s",
        "indicator(\"x\")\na = input.string(v())\nplot(close)\n",
        codes::E0230,
    ),
    (
        "int s = na\n    s",
        "indicator(\"x\")\nplot(input.int(1, minval = v()))\n",
        codes::E0230,
    ),
    (
        "int s = na\n    s",
        "indicator(\"x\")\nplot(input.int(1, options = [1, v()]))\n",
        codes::E0230,
    ),
    (
        "string s = na\n    s",
        "strategy(v())\nplot(close)\n",
        codes::E0205,
    ),
    (
        "string s = na\n    s",
        "strategy(\"x\", default_qty_type = v())\nplot(close)\n",
        codes::E0205,
    ),
];

fn codes_of(source: &str, severity: Severity) -> Vec<String> {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "test source must parse cleanly: {source:?} {:?}",
        parsed.diagnostics
    );
    let diagnostics: Vec<Diagnostic> = analyze(&parsed.program).diagnostics;
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == severity)
        .filter_map(|diagnostic| diagnostic.code.clone())
        .collect()
}

#[test]
fn a_warning_inside_an_argument_does_not_hide_its_error() {
    for &(value, rest, error) in CASES {
        let plain = format!("//@version=6\nv() =>\n    {value}\n{rest}");
        let warned = format!("//@version=6\nv() =>\n    close\n    {value}\n{rest}");

        assert_eq!(
            codes_of(&plain, Severity::Error),
            vec![error],
            "without the warning: {plain:?}"
        );
        let mut expected_warnings = vec![codes::W0002.to_string()];
        expected_warnings.extend(codes_of(&plain, Severity::Warning));
        assert_eq!(
            codes_of(&warned, Severity::Warning),
            expected_warnings,
            "the discarded `close` warns: {warned:?}"
        );
        assert_eq!(
            codes_of(&warned, Severity::Error),
            codes_of(&plain, Severity::Error),
            "with the warning: {warned:?}"
        );
    }
}

/// An error inside the argument still stands in for the rule's own error,
/// with or without a warning beside it: the mistake is reported once.
#[test]
fn an_error_inside_an_argument_is_still_reported_once() {
    let source = "//@version=6\nv() =>\n    close\n    nope\nindicator(v())\nplot(close)\n";
    assert_eq!(codes_of(source, Severity::Error), vec![codes::E0201]);
    assert_eq!(codes_of(source, Severity::Warning), vec![codes::W0002]);
}
