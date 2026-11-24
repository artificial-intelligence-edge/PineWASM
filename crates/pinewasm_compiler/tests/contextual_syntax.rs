//! Surface syntax accepted by this compiler.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

#[test]
fn type_identifier_and_leading_dot_literals_keep_their_values() {
    let source = r#"//@version=6
indicator("Contextual syntax")
type = .5
f(string type) =>
    switch type
        "half" => .5
        => .25e1
type Point
    float value
point = Point.new(-.25)
plot(type + f("half") + point.value)
plot(f(type="other"))
"#;
    let market = MarketData {
        open: vec![1.0; 2],
        high: vec![1.0; 2],
        low: vec![1.0; 2],
        close: vec![1.0; 2],
        volume: vec![1.0; 2],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[0.75,0.75]"
    );
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[2.5,2.5]"
    );
}

#[test]
fn drawing_count_bounds_distinguish_polylines() {
    for declaration in ["indicator", "strategy"] {
        let source = format!(
            "//@version=6\n{declaration}(\"Zero caps\", max_lines_count=0, max_labels_count=0, max_boxes_count=0, max_polylines_count=1)\nplot(close)\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{:?}", result.diagnostics);
        let caps = result.program.unwrap().metadata.drawing_caps;
        assert_eq!(caps.max_lines_count, Some(0));
        assert_eq!(caps.max_labels_count, Some(0));
        assert_eq!(caps.max_boxes_count, Some(0));
        for invalid in [
            source.replace("max_lines_count=0", "max_lines_count=-1"),
            source.replace("max_polylines_count=1", "max_polylines_count=0"),
            source.replace("max_polylines_count=1", "max_polylines_count=101"),
        ] {
            let result = compile(&invalid, CompileOptions::default());
            assert!(!result.success);
            assert!(result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0219")));
        }
    }
}
