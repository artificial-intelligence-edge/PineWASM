//! The built-ins that bind only positionally also accept their parameter
//! names: the ten type casts (`x`), `timestamp` (each overload's names,
//! including `dateString`), `alertcondition` (skipping `title` by name),
//! `str.format_time` (skipping `format` by name), `log.*` (`message` or
//! `formatString`) and the `footprint.*`/`volume_row.*` accessors (`id`,
//! `price`). Every named form must behave exactly like its positional form,
//! and an unknown name is still rejected.
use pinewasm_compiler::{compile, compile_and_run, CompileOptions, MarketData, RunOptions};

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Named binding\")\n{body}\n")
}

fn market() -> MarketData {
    let bars = 4;
    MarketData {
        open: vec![10.0, 11.0, 12.0, 13.0],
        high: vec![12.0, 13.0, 14.0, 15.0],
        low: vec![9.0, 10.0, 11.0, 12.0],
        close: vec![11.0, 12.0, 11.5, 14.0],
        volume: vec![100.0; bars],
        time: (0..bars as i64)
            .map(|i| 1_700_000_000_000 + i * 86_400_000)
            .collect(),
        ..MarketData::default()
    }
}

/// Compile and run `body`, requiring success and no runtime error. Bodies
/// compare a named form with its positional form inside the script and raise
/// `runtime.error` on any difference, so a passing run is the equivalence.
fn assert_runs(body: &str) -> pinewasm_compiler::RunResult {
    let result = compile_and_run(
        &source(body),
        &market(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let compile = result.compile.as_ref().expect("compile result");
    assert!(compile.success, "{body}\n{:?}", compile.diagnostics);
    assert!(
        result.runtime_errors.is_empty(),
        "{body}\n{:?}",
        result.runtime_errors
    );
    assert_eq!(result.bars_executed, 4, "{body}");
    result
}

fn assert_rejected(body: &str, code: &str) {
    let result = compile(&source(body), CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted: {body}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.is_error() && d.code.as_deref() == Some(code)),
        "{body}: expected {code}, got {:?}",
        result.diagnostics
    );
}

#[test]
fn scalar_casts_bind_x_by_name() {
    assert_runs(
        "a = int(x = 7.9)\n\
         b = float(x = 7)\n\
         c = bool(x = 1)\n\
         d = color(x = color.red)\n\
         e = string(x = 12)\n\
         if a != int(7.9) or b != float(7) or c != bool(1) or d != color(color.red) or e != string(12)\n\
         \x20   runtime.error(\"named cast differs\")\n\
         plot(a + b)",
    );
}

#[test]
fn drawing_casts_bind_x_by_name() {
    assert_runs(
        "var line ln = line(x = na)\n\
         var label lb = label(x = na)\n\
         var box bx = box(x = na)\n\
         var table tb = table(x = na)\n\
         var linefill lf = linefill(x = na)\n\
         l2 = line(x = line.new(bar_index, close, bar_index + 1, close))\n\
         plot(na(ln) and na(lb) and na(bx) and na(tb) and na(lf) and not na(l2) ? 1 : 0)",
    );
}

#[test]
fn casts_reject_other_names_and_extra_arguments() {
    assert_rejected("plot(float(value = 1))", "E0228");
    assert_rejected("plot(float(1, 2))", "E0204");
    assert_rejected("plot(float())", "E0204");
}

#[test]
fn timestamp_binds_each_overloads_names() {
    assert_runs(
        "a = timestamp(year = 2024, month = 3, day = 9)\n\
         b = timestamp(day = 9, month = 3, year = 2024, minute = 5)\n\
         c = timestamp(timezone = \"UTC+2\", year = 2024, month = 3, day = 9, hour = 1)\n\
         d = timestamp(\"UTC+2\", year = 2024, month = 3, day = 9, hour = 1)\n\
         e = timestamp(dateString = \"27 Dec 2024 09:30 -0500\")\n\
         if a != timestamp(2024, 3, 9) or b != timestamp(2024, 3, 9, 0, 5) or c != timestamp(\"UTC+2\", 2024, 3, 9, 1) or d != c or e != timestamp(\"27 Dec 2024 09:30 -0500\")\n\
         \x20   runtime.error(\"named timestamp differs\")\n\
         plot(a)",
    );
}

#[test]
fn timestamp_timezone_form_takes_optional_clock_fields() {
    // `hour`, `minute` and `second` are optional in the timezone form too,
    // defaulting to 0.
    assert_runs(
        "a = timestamp(\"UTC+2\", 2024, 3, 9)\n\
         if a != timestamp(\"UTC+2\", 2024, 3, 9, 0, 0, 0)\n\
         \x20   runtime.error(\"omitted clock fields differ\")\n\
         plot(a)",
    );
}

#[test]
fn timestamp_rejects_names_from_another_overload_or_unknown_names() {
    // `date_string` is not a parameter name; the parameter is `dateString`.
    assert_rejected(
        "plot(timestamp(date_string = \"27 Dec 2024 09:30 -0500\"))",
        "E0228",
    );
    assert_rejected("plot(timestamp(year = 2024, month = 3, dia = 9))", "E0228");
    assert_rejected("plot(timestamp(year = 2024, month = 3))", "E0204");
}

#[test]
fn timestamp_names_fix_the_overload_and_its_argument_types() {
    // The names select the overload; a value of the wrong type is rejected
    // rather than reinterpreted as another overload's fields.
    assert_rejected(
        "plot(timestamp(timezone = 2024, year = 3, month = 9, day = 1))",
        "E0205",
    );
    assert_rejected(
        "plot(timestamp(timezone = 2024, year = 3, month = 9))",
        "E0204",
    );
    assert_rejected(
        "plot(timestamp(year = \"UTC+1\", month = 2024, day = 3, hour = 9))",
        "E0205",
    );
    assert_rejected(
        "plot(timestamp(year = \"27 Dec 2024 09:30 -0500\"))",
        "E0204",
    );
    assert_rejected(
        "plot(timestamp(timezone = \"27 Dec 2024 09:30 -0500\"))",
        "E0204",
    );
    assert_rejected("plot(timestamp(dateString = 2024))", "E0205");
}

#[test]
fn timestamp_arity_errors_analyze_each_argument_once() {
    for call in [
        "plot(timestamp(year = undefined_x, month = 3))",
        "plot(timestamp(undefined_x, 3))",
    ] {
        let result = compile(&source(call), CompileOptions::default());
        let unknown = result
            .diagnostics
            .iter()
            .filter(|d| d.code.as_deref() == Some("E0201"))
            .count();
        assert_eq!(unknown, 1, "{call}: {:?}", result.diagnostics);
    }
}

#[test]
fn alertcondition_skips_title_by_name() {
    let named = assert_runs(
        "alertcondition(close > open, message = \"up\")\n\
         plot(close)",
    );
    let positional = assert_runs(
        "alertcondition(close > open, \"\", \"up\")\n\
         plot(close)",
    );
    assert_eq!(named.alerts, positional.alerts);
    assert!(
        !named.alerts.is_empty(),
        "the condition fires on rising bars"
    );
    assert_rejected("alertcondition(close > open, text = \"up\")", "E0228");
    assert_rejected("alertcondition(title = \"t\")", "E0204");
}

#[test]
fn str_format_time_skips_format_by_name() {
    assert_runs(
        "a = str.format_time(time, timezone = \"UTC\")\n\
         b = str.format_time(time, \"yyyy-MM-dd'T'HH:mm:ssZ\", \"UTC\")\n\
         c = str.format_time(time = time, format = \"yyyy\", timezone = \"UTC\")\n\
         if a != b or c != str.format_time(time, \"yyyy\", \"UTC\")\n\
         \x20   runtime.error(\"named format_time differs\")\n\
         plot(close)",
    );
}

#[test]
fn log_functions_accept_either_overload_name() {
    let named = assert_runs(
        "log.info(formatString = \"bar {0}\")\n\
         log.warning(message = \"plain\")\n\
         plot(close)",
    );
    let positional = assert_runs(
        "log.info(\"bar {0}\")\n\
         log.warning(\"plain\")\n\
         plot(close)",
    );
    assert_eq!(named.logs, positional.logs);
    assert_rejected("log.info(text = \"x\")\nplot(close)", "E0204");
}

#[test]
fn footprint_accessors_bind_id_and_price_by_name() {
    // Without footprint data in the run the rows are `na`, and reading a field of an
    // `na` row is a runtime error, so the row accessors are compiled only.
    assert_runs(
        "fp = request.footprint(100)\n\
         a = footprint.buy_volume(id = fp)\n\
         row = footprint.get_row_by_price(price = close, id = fp)\n\
         if na(a) != na(footprint.buy_volume(fp)) or na(row) != na(footprint.get_row_by_price(fp, close))\n\
         \x20   runtime.error(\"named footprint access differs\")\n\
         plot(close)",
    );
    let rows = compile(
        &source(
            "fp = request.footprint(100)\n\
             row = footprint.get_row_by_price(id = fp, price = close)\n\
             plot(not na(row) ? volume_row.total_volume(id = row) : na)",
        ),
        CompileOptions::default(),
    );
    assert!(rows.success, "{:?}", rows.diagnostics);
    assert_rejected(
        "fp = request.footprint(100)\nplot(footprint.delta(fp_id = fp))",
        "E0228",
    );
}

#[test]
fn matrix_fill_skips_bounds_by_name() {
    assert_runs(
        "named = matrix.new<float>(3, 3, 0)\n\
         matrix.fill(named, 1.0, to_row = 1)\n\
         matrix.fill(named, 2.0, from_row = 1, to_column = 1)\n\
         positional = matrix.new<float>(3, 3, 0)\n\
         matrix.fill(positional, 1.0, 0, 1)\n\
         matrix.fill(positional, 2.0, 1, 3, 0, 1)\n\
         diff = 0.0\n\
         for r = 0 to 2\n\
         \x20   for c = 0 to 2\n\
         \x20       diff += math.abs(matrix.get(named, r, c) - matrix.get(positional, r, c))\n\
         if diff != 0 or matrix.get(named, 0, 2) != 1.0 or matrix.get(named, 2, 0) != 2.0 or matrix.get(named, 2, 1) != 0.0\n\
         \x20   runtime.error(\"named matrix.fill differs\")\n\
         plot(close)",
    );
}
