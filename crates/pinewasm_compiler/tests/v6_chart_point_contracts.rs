//! chart.point call binding checks.
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_span::Severity;

fn errors(body: &str) -> Vec<String> {
    compile(
        &format!("//@version=6\nindicator(\"chart.point contract\")\n{body}\nplot(close)\n"),
        CompileOptions::default(),
    )
    .diagnostics
    .into_iter()
    .filter(|diagnostic| diagnostic.severity == Severity::Error)
    .map(|diagnostic| diagnostic.message)
    .collect()
}

#[test]
fn chart_point_rejects_undocumented_names_and_incompatible_types() {
    for body in [
        "p = chart.point.from_index(bar=bar_index, price=close)",
        "p = chart.point.from_index(index=bar_index, index=bar_index, price=close)",
        "p = chart.point.copy(point=chart.point.now(close))",
        "p = chart.point.from_time(timestamp=time, price=close)",
        "p = chart.point.new(timestamp=time, index=bar_index, price=close)",
        "p = chart.point.from_index(true, close)",
        "p = chart.point.from_time(time, true)",
        "p = chart.point.copy(close)",
        "p = chart.point.new(time, close, close)",
    ] {
        assert!(!errors(body).is_empty(), "unexpectedly accepted: {body}");
    }
}
