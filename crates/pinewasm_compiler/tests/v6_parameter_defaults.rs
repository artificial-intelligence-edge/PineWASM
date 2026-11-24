//! Default values and runtime rules checked by these tests:
//!
//! - a `bool` is never `na`, so `bool` arrays and matrices start out `false`;
//! - `str.trim` returns an empty string for an `na` source;
//! - an empty symbol requests the chart's symbol;
//! - `request.currency_rate` returns 1 when both currencies are the same;
//! - a method may be declared on an enum type;
//! - `str.format_time` writes the `yy`, `a`, `h` and `S` tokens;
//! - apostrophes quote literal text in `str.format` and `log.*` messages.

use pinewasm_compiler::{
    compile, compile_and_run, compile_and_run_with_libraries, CompileOptions, MarketData,
    RunOptions,
};

fn market() -> MarketData {
    let closes = [101.0, 99.0, 102.5];
    MarketData {
        open: vec![100.0; closes.len()],
        high: closes.iter().map(|c| c + 1.0).collect(),
        low: closes.iter().map(|c| c - 2.0).collect(),
        close: closes.to_vec(),
        volume: vec![1000.0; closes.len()],
        time: (0..closes.len() as i64)
            .map(|bar| 1_700_000_000_000 + bar * 86_400_000)
            .collect(),
        ..MarketData::default()
    }
}

/// Compile and run `body`, which signals a wrong result with `runtime.error`,
/// and return the values of its first plot.
fn run(body: &str) -> serde_json::Value {
    let source = format!("//@version=6\nindicator(\"Default values\")\n{body}\n");
    let result = compile_and_run(
        &source,
        &market(),
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
        "{source}\n{:?}",
        result.runtime_errors
    );
    serde_json::to_value(&result.plots[0].values).unwrap()
}

#[test]
fn bool_arrays_and_matrices_start_false() {
    run("a = array.new_bool(2)\n\
         b = array.new<bool>(3)\n\
         m = matrix.new<bool>(2, 2)\n\
         if a.get(0) or a.get(1) or b.get(2) or m.get(1, 1)\n\
         \x20   runtime.error(\"a default bool element is true\")\n\
         if not (not a.get(0) and a.get(1) == false and not b.get(0) and not m.get(0, 0))\n\
         \x20   runtime.error(\"a default bool element is not false\")\n\
         explicit = array.new_bool(1, true)\n\
         if not explicit.get(0)\n\
         \x20   runtime.error(\"an explicit initial value is ignored\")\n\
         plot(close)");
}

#[test]
fn str_trim_of_na_is_an_empty_string() {
    // `na()` is also true for an empty string, so the result is checked by
    // value and length: an `na` result fails both comparisons.
    run("string missing = na\n\
         trimmed = str.trim(missing)\n\
         if not (trimmed == \"\" and str.length(trimmed) == 0)\n\
         \x20   runtime.error(\"str.trim(na) is not empty\")\n\
         if str.trim(\" \\t padded \\n\") != \"padded\"\n\
         \x20   runtime.error(\"str.trim no longer trims\")\n\
         plot(close)");
}

#[test]
fn an_empty_symbol_requests_the_chart_symbol() {
    let values = run("same = request.security(\"\", \"\", close)\n\
                      if same != close\n\
                      \x20   runtime.error(\"an empty symbol is not the chart\")\n\
                      plot(same)");
    assert_eq!(values, serde_json::json!([101.0, 99.0, 102.5]));
    // The same through constant bindings.
    let values = run("symbol = \"\"\n\
                      const string none = \"\"\n\
                      a = request.security(symbol, \"\", close)\n\
                      b = request.security(none, \"\", close)\n\
                      if a != close or b != close\n\
                      \x20   runtime.error(\"a constant empty symbol is not the chart\")\n\
                      plot(a)");
    assert_eq!(values, serde_json::json!([101.0, 99.0, 102.5]));
    // The lower-timeframe request accepts it as the chart feed, the same as
    // `syminfo.tickerid`.
    for symbol in ["\"\"", "syminfo.tickerid", "none"] {
        let source = format!(
            "//@version=6\nindicator(\"Lower\")\nconst string none = \"\"\n\
             intrabar = request.security_lower_tf({symbol}, \"1\", close)\n\
             plot(array.size(intrabar))\n"
        );
        let result = compile(&source, CompileOptions::default());
        assert!(result.success, "{symbol}: {:?}", result.diagnostics);
    }
}

#[test]
fn a_currency_converts_to_itself_at_one() {
    let values = run("plot(request.currency_rate(\"USD\", \"USD\"))\n\
                      if request.currency_rate(from = \"EUR\", to = \"EUR\") != 1\n\
                      \x20   runtime.error(\"EUR to EUR is not 1\")\n\
                      if not na(request.currency_rate(\"EUR\", \"USD\", ignore_invalid_currency = true))\n\
                      \x20   runtime.error(\"a rate without host data is not na\")");
    assert_eq!(values, serde_json::json!([1.0, 1.0, 1.0]));
}

#[test]
fn methods_may_be_declared_on_enums() {
    let values = run("enum Trend\n\
                      \x20   up = \"Rising\"\n\
                      \x20   down = \"Falling\"\n\
                      method sign(Trend this) => this == Trend.up ? 1 : -1\n\
                      method flip(Trend this) => this == Trend.up ? Trend.down : Trend.up\n\
                      trend = close > open ? Trend.up : Trend.down\n\
                      if Trend.up.sign() != 1 or Trend.down.flip() != Trend.up\n\
                      \x20   runtime.error(\"a method on an enum member is wrong\")\n\
                      plot(trend.sign() * trend.flip().sign())");
    assert_eq!(values, serde_json::json!([-1.0, -1.0, -1.0]));
    let unknown = compile(
        "//@version=6\nindicator(\"Unknown\")\nmethod f(Missing this) => 1\nplot(close)\n",
        CompileOptions::default(),
    );
    assert!(unknown
        .diagnostics
        .iter()
        .any(|d| d.is_error() && d.code.as_deref() == Some("E0237")));
}

#[test]
fn an_imported_enum_member_takes_its_methods_directly() {
    let library = "//@version=6\nlibrary(\"Trends\")\n\
                   export enum Trend\n    up\n    down\n\
                   export method sign(Trend this) => this == Trend.up ? 1 : -1\n";
    let main = "//@version=6\nindicator(\"Importer\")\nimport user/Trends/1 as lib\n\
                plot(lib.Trend.up.sign() * 10 + lib.Trend.down.sign())\n";
    let result = compile_and_run_with_libraries(
        main,
        &[("user/Trends/1".to_string(), library.to_string())],
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.compile.as_ref().is_some_and(|c| c.success),
        "{:?}",
        result.diagnostics
    );
    assert_eq!(
        serde_json::to_value(&result.plots[0].values).unwrap(),
        serde_json::json!([9.0, 9.0, 9.0])
    );
}

#[test]
fn str_format_time_writes_the_documented_tokens() {
    run("afternoon = 1704459845007\n\
         midnight = 1704501000000\n\
         text = str.format_time(afternoon, \"yy-M-d h:mm:ss.SSS a|hh|S|SS\", \"UTC\")\n\
         if text != \"24-1-5 1:04:05.007 PM|01|7|07\"\n\
         \x20   runtime.error(text)\n\
         late = str.format_time(midnight, \"h a HH 'at' yyyy\", \"UTC\")\n\
         if late != \"12 AM 00 at 2024\"\n\
         \x20   runtime.error(late)\n\
         plot(close)");
}

#[test]
fn apostrophes_quote_literal_text_in_format_strings() {
    run(
        "text = str.format(\"'{'{0}'}' costs {1} it''s '{1}' ok\", 7, 2)\n\
         if text != \"{7} costs 2 it's {1} ok\"\n\
         \x20   runtime.error(text)\n\
         unclosed = str.format(\"a 'b {0}\", 1)\n\
         if unclosed != \"a b {0}\"\n\
         \x20   runtime.error(unclosed)\n\
         plot(close)",
    );
}
