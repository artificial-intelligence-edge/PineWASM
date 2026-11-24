//! Admission rule: an unsupported constant fold must not become
//! an asserted colorer binding merely because an enclosing block is dynamic.
use pinewasm_compiler::{compile, run_program, CompileOptions, MarketData, RunOptions, Value};

fn check(body: &str) {
    let source = format!(
        "//@version=6\nindicator(\"unknown composition admission\")\ncolor UP=#336699\n{body}\nplotarrow(close,colorup=UP,colordown=#993366)"
    );
    let result = compile(&source, CompileOptions::default());
    assert!(result.success, "{:?}", result.diagnostics);
    let program = result.program.unwrap();
    assert!(program.plots[0].style.arrow_static_colors.is_none());
    assert!(program.plots[0].style.arrow_runtime_colorers.is_none());
    assert!(program.plots[0]
        .style
        .unsupported
        .iter()
        .any(|option| option.reason.contains("cannot be proven")));
    let market = MarketData {
        open: vec![10.0; 3],
        high: vec![12.0; 3],
        low: vec![9.0; 3],
        close: vec![11.0, 10.0, 12.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let run = run_program(&program, &market, &RunOptions::default());
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(
        run.plots[0].values,
        market
            .close
            .into_iter()
            .map(Value::Float)
            .collect::<Vec<_>>()
    );
    let style = serde_json::to_value(&run.plots[0].style).unwrap();
    assert!(style.get("arrowStaticColors").is_none());
    assert!(style.get("arrowRuntimeColorers").is_none());
}

#[test]
fn unsupported_nested_fold_survives_dynamic_if() {
    check("if bar_index>0\n    if math.tan(0)>0\n        UP:=#336699");
}

#[test]
fn unsupported_nested_fold_survives_loop_body() {
    check("for i=0 to 1\n    if math.tan(0)>0\n        UP:=#336699");
}

#[test]
fn unsupported_fold_survives_dynamic_else_merge() {
    check("if bar_index>0\n    UP:=#336699\nelse\n    if math.tan(0)>0\n        UP:=#336699");
}
