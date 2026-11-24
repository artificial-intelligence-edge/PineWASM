//! `str.tostring` writes arrays and matrices: an array as `[a, b, c]`, a
//! matrix as one bracketed row per line, each element as `str.tostring`
//! writes that scalar (with the same `format`), and `na` elements as "NaN".
//! The bracket and row layout is PineWASM's own format, as these tests check it.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

fn logs(body: &str) -> Vec<String> {
    let source = format!("//@version=6\nindicator(\"Collection strings\")\n{body}\nplot(close)\n");
    let market = MarketData {
        open: vec![10.0],
        high: vec![11.0],
        low: vec![9.0],
        close: vec![10.5],
        volume: vec![100.0],
        time: vec![1_700_000_000_000],
        ..MarketData::default()
    };
    let result = compile_and_run(
        &source,
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{source}\n{:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    result
        .logs
        .iter()
        .map(|entry| entry.message.clone())
        .collect()
}

#[test]
fn arrays_are_bracketed_and_comma_separated() {
    assert_eq!(
        logs(
            "log.info(str.tostring(array.from(1, 22, -3)))\n\
             log.info(str.tostring(array.from(1.5, 2.0, -0.25, 0.123456789)))\n\
             log.info(str.tostring(array.from(true, false)))\n\
             log.info(str.tostring(array.from(\"a\", \"b c\", \"\")))\n\
             log.info(str.tostring(array.new<float>(0)))\n\
             gaps = array.from(1.0, 2.0, 3.0)\n\
             array.set(gaps, 1, na)\n\
             log.info(str.tostring(gaps))"
        ),
        [
            "[1, 22, -3]",
            "[1.5, 2, -0.25, 0.12345679]",
            "[true, false]",
            "[a, b c, ]",
            "[]",
            "[1, NaN, 3]",
        ]
    );
}

#[test]
fn matrices_write_one_bracketed_row_per_line() {
    assert_eq!(
        logs(
            "m = matrix.new<int>(2, 3, 0)\n\
             matrix.set(m, 0, 1, 7)\n\
             matrix.set(m, 1, 2, -4)\n\
             log.info(str.tostring(m))\n\
             log.info(str.tostring(matrix.new<string>(2, 2, \"x\")))\n\
             log.info(str.tostring(matrix.new<float>(0, 0)))"
        ),
        ["[0, 7, 0]\n[0, 0, -4]", "[x, x]\n[x, x]", ""]
    );
}

#[test]
fn a_format_applies_to_every_numeric_element() {
    // Each element is written exactly as the scalar overload writes it.
    let l = logs(
        "log.info(str.tostring(array.from(1.234, 5.6), \"#.00\"))\n\
         log.info(str.tostring(1.234, \"#.00\"))\n\
         log.info(str.tostring(5.6, \"#.00\"))\n\
         log.info(str.tostring(array.from(3, 40), \"000\"))\n\
         log.info(str.tostring(3, \"000\"))\n\
         log.info(str.tostring(40, \"000\"))\n\
         log.info(str.tostring(matrix.new<float>(2, 1, 0.5), format.percent))\n\
         log.info(str.tostring(0.5, format.percent))",
    );
    assert_eq!(l[0], format!("[{}, {}]", l[1], l[2]));
    assert_eq!(l[3], format!("[{}, {}]", l[4], l[5]));
    assert_eq!(l[6], format!("[{}]\n[{}]", l[7], l[7]));
}

#[test]
fn only_documented_element_types_are_accepted() {
    for (body, code) in [
        // A format applies to numeric collections only.
        ("x = str.tostring(array.from(true), \"#\")", "E0205"),
        ("x = str.tostring(array.from(\"a\"), \"#\")", "E0205"),
        // Colors and maps have no string form.
        ("x = str.tostring(array.from(color.red))", "E0205"),
        ("x = str.tostring(map.new<string, float>())", "E0205"),
    ] {
        let source = format!("//@version=6\nindicator(\"Rejected\")\n{body}\nplot(close)\n");
        let result = compile(&source, CompileOptions::default());
        assert!(!result.success, "accepted: {body}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.is_error() && d.code.as_deref() == Some(code)),
            "{body}: expected {code}, got {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn formatting_a_collection_is_charged_per_element_and_pattern_length() {
    // 10,000 elements, each formatted with a 100,000-character pattern: the
    // work is charged up front and exceeds the per-bar instruction budget,
    // instead of running uncharged.
    let source = "//@version=6\nindicator(\"Charged\")\n\
        values = array.new<int>(10000, 1)\n\
        text = str.tostring(values, str.repeat(\"#\", 100000))\n\
        plot(str.length(text))\n";
    let market = MarketData {
        open: vec![10.0],
        high: vec![11.0],
        low: vec![9.0],
        close: vec![10.5],
        volume: vec![100.0],
        time: vec![1_700_000_000_000],
        ..MarketData::default()
    };
    let started = std::time::Instant::now();
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
        result
            .runtime_errors
            .iter()
            .any(|e| format!("{:?}", e.kind).contains("InstructionLimitExceeded")),
        "{:?}",
        result.runtime_errors
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}
