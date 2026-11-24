//! Local compiler/runtime safeguards for persistent-write cast classification.
//! They cover only the cases listed below.

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, InputActiveExpr, InputValue, MarketData, RunOptions,
    Value,
};

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Persistent cast effects\")\n{body}\n")
}

fn assert_offset_and_values(body: &str) {
    let market = MarketData {
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        ..MarketData::default()
    };
    let run = compile_and_run(
        &source(body),
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{body}: {:?}",
        run.diagnostics
    );
    assert!(
        run.runtime_errors.is_empty(),
        "{body}: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.bars_executed, 4, "{body}");
    assert_eq!(run.plots.len(), 2, "{body}");
    let plot = &run.plots[0];
    assert!(
        plot.style.unsupported.is_empty(),
        "{body}: {:?}",
        plot.style
    );
    assert!(
        plot.style.runtime_scalars.is_empty(),
        "{body}: {:?}",
        plot.style
    );
    let offset = plot
        .style
        .bound_scalars
        .iter()
        .find(|binding| binding.field == "offset")
        .expect("resolved offset");
    assert!(
        matches!(
            &offset.expression,
            InputActiveExpr::Value {
                value: InputValue::Int(2)
            }
        ),
        "{body}: {:?}",
        offset.expression
    );
    assert_eq!(plot.values.len(), 4, "{body}");
    for (actual, expected) in plot.values.iter().zip([2.0, 1.0, 2.0, 1.0]) {
        assert!(
            matches!(actual, Value::Float(value) if *value == expected),
            "{body}: {actual:?}"
        );
    }
    assert_eq!(run.plots[1].values.len(), 4, "{body}");
    assert!(
        run.plots[1]
            .values
            .iter()
            .all(|value| matches!(value, Value::Float(value) if *value == 2.0)),
        "{body}: {:?}",
        run.plots[1].values
    );
}

#[test]
fn every_canonical_cast_preserves_stable_var_and_varip_offsets() {
    for storage in ["var", "varip"] {
        for cast in [
            "int(close)",
            "float(bar_index)",
            "bool(close)",
            "string(close)",
            "color(color.red)",
            "line(na)",
            "label(na)",
            "box(na)",
            "table(na)",
            "linefill(na)",
        ] {
            assert_offset_and_values(&format!(
                "{storage} int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = {cast}\nplot(close, offset=castAuditShift)\nplot(castAuditShift)"
            ));
        }
    }
}

#[test]
fn a_cast_reading_a_local_shadow_does_not_poison_the_outer_slot() {
    for storage in ["var", "varip"] {
        assert_offset_and_values(&format!(
            "{storage} int castAuditShift = 1\nif true\n    castAuditShift = 3\n    castAuditValue = int(castAuditShift)\ncastAuditShift := 2\nplot(close, offset=castAuditShift)\nplot(castAuditShift)"
        ));
    }
}

#[test]
fn cast_arguments_and_controls_keep_conservative_series_safeguards() {
    for storage in ["var", "varip"] {
        for body in [
            format!("castAuditOpaque() => 0\n{storage} int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = int(castAuditOpaque())"),
            format!("{storage} int castAuditShift = 1\ncastAuditRead = int(castAuditShift)\ncastAuditShift := 2"),
            format!("{storage} int castAuditShift = 1\nfor castAuditIndex = 0 to 1\n    castAuditShift := 2\ncastAuditValue = int(close)"),
            format!("castAuditOpaque() => 0\n{storage} int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = castAuditOpaque()"),
            format!("{storage} int castAuditShift = 1\ncastAuditShift := int(close)"),
            format!("{storage} int castAuditShift = 1\nif bool(close > open)\n    castAuditShift := 2"),
            // Calls remain outside the bounded closed-value evaluator.
            format!("{storage} int castAuditShift = 1\nif bool(true)\n    castAuditShift := 2"),
            format!("{storage} int castAuditShift = 1\ncastAuditRead = int(castAuditShift)\nif true\n    castAuditShift = 3\n    castAuditValue = int(castAuditShift)\ncastAuditShift := 2"),
            // A user method ending in `int` is not a bare cast call.
            format!("method int(float castAuditReceiver) => castAuditReceiver\n{storage} int castAuditShift = 1\ncastAuditShift := 2\ncastAuditReceiver = close\ncastAuditValue = castAuditReceiver.int()"),
            // A pure outer drawing cast cannot hide its constructor operand.
            format!("{storage} int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = line(line.new(bar_index, open, bar_index, close))"),
        ] {
            let text = source(&format!("{body}\nplot(close, offset=castAuditShift)"));
            let result = compile(&text, CompileOptions::default());
            assert!(!result.success, "{text}");
            assert!(
                result.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code.as_deref() == Some("E0206")
                        && diagnostic.message.contains("`offset`")
                        && diagnostic.message.ends_with("found series")
                }),
                "{text}: {:?}",
                result.diagnostics
            );
        }
    }
}

#[test]
fn cast_type_arity_redefinition_and_explicit_ceilings_still_fail() {
    for (body, expected) in [
        (
            "var int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = int(close)\nconst int castAuditConst = castAuditShift\nplot(close)",
            &["E0226"][..],
        ),
        (
            "var int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = int(\"invalid\")\nplot(close, offset=castAuditShift)",
            &["E0205"][..],
        ),
        (
            "castAuditOpaque() => 0\nvar int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = int(1, castAuditOpaque())\nplot(close, offset=castAuditShift)",
            &["E0204", "E0206"][..],
        ),
        (
            "int(castAuditParameter) => castAuditParameter\nvar int castAuditShift = 1\ncastAuditShift := 2\ncastAuditValue = int(close)\nplot(close, offset=castAuditShift)",
            &["E0211"][..],
        ),
    ] {
        let text = source(body);
        let result = compile(&text, CompileOptions::default());
        assert!(!result.success, "{text}");
        for code in expected {
            assert!(
                result.diagnostics.iter().any(|diagnostic| {
                    diagnostic.is_error() && diagnostic.code.as_deref() == Some(*code)
                }),
                "{text}: {code}: {:?}",
                result.diagnostics
            );
        }
    }
}
