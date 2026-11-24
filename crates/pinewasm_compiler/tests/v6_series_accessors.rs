//! The `footprint.*` and `volume_row.*` accessor results, and the `ticker.*`
//! overloads that take `series` arguments, are `series` values. These tests
//! check the qualifier, the history it allows, the paint modes derived from it
//! and the per-bar values.

use std::collections::HashMap;

use pinewasm_compiler::{
    compile, compile_and_run, CompileOptions, HostFootprint, HostVolumeRow, MarketData, RunOptions,
};
use pinewasm_hir::PlotColorMode;

/// Four bars that alternate up and down, with a footprint sample whose
/// values change on every bar.
fn footprint_data() -> MarketData {
    let samples = (0..4)
        .map(|bar| {
            let b = bar as f64;
            Some(HostFootprint {
                buy_volume: 30.0 + b,
                sell_volume: 20.0 - b,
                total_volume: 50.0 + 10.0 * b,
                delta: 10.0 + 2.0 * b,
                poc: 101.0 + b,
                vah: 102.0 + b,
                val: 100.0 + b,
                rows: vec![HostVolumeRow {
                    buy_volume: 3.0 + b,
                    sell_volume: 2.0,
                    total_volume: 5.0 + b,
                    delta: 1.0 + b,
                    up_price: 1000.0,
                    down_price: 0.0,
                    has_buy_imbalance: bar % 2 == 0,
                    has_sell_imbalance: bar % 2 == 1,
                }],
            })
        })
        .collect();
    let mut table = HashMap::new();
    table.insert("footprint/XYZ".to_string(), samples);
    MarketData {
        open: vec![10.0, 12.0, 10.0, 12.0],
        high: vec![13.0; 4],
        low: vec![9.0; 4],
        close: vec![11.0; 4],
        volume: vec![100.0; 4],
        timeframe: "1D".into(),
        footprint_data: table,
        ..MarketData::default()
    }
}

fn run(source: &str, data: &MarketData) -> pinewasm_compiler::RunResult {
    let result = compile_and_run(
        source,
        data,
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

fn values(result: &pinewasm_compiler::RunResult, plot: usize) -> String {
    serde_json::to_string(&result.plots[plot].values).unwrap()
}

fn rejected_with(source: &str, code: &str) {
    let result = compile(source, CompileOptions::default());
    assert!(!result.success, "unexpectedly accepted:\n{source}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some(code)),
        "{source}\nexpected {code}: {:?}",
        result.diagnostics
    );
}

const FP_HEADER: &str = "//@version=6\nindicator(\"footprint series\")\nfp = request.footprint(\"XYZ\")\nr = footprint.get_row_by_price(fp, close)\n";

#[test]
fn footprint_accessor_history_reads_the_previous_bar() {
    let result = run(
        &format!(
            "{FP_HEADER}plot(footprint.delta(fp)[1])
plot(footprint.buy_volume(fp)[1])
plot(footprint.sell_volume(fp)[1])
plot(footprint.total_volume(fp)[1])
plot(footprint.poc(fp)[1])
plot(footprint.vah(fp)[1])
plot(footprint.val(fp)[1])
"
        ),
        &footprint_data(),
    );
    for (plot, want) in [
        (0, "[null,10.0,12.0,14.0]"),
        (1, "[null,30.0,31.0,32.0]"),
        (2, "[null,20.0,19.0,18.0]"),
        (3, "[null,50.0,60.0,70.0]"),
        (4, "[null,101.0,102.0,103.0]"),
        (5, "[null,102.0,103.0,104.0]"),
        (6, "[null,100.0,101.0,102.0]"),
    ] {
        assert_eq!(values(&result, plot), want, "plot {plot}");
    }
}

#[test]
fn volume_row_accessor_history_reads_the_previous_bar() {
    let result = run(
        &format!(
            "{FP_HEADER}plot(volume_row.total_volume(r)[1])
plot(volume_row.buy_volume(r)[1])
plot(volume_row.delta(r)[1])
plot(volume_row.up_price(r)[1])
plot(volume_row.has_buy_imbalance(r)[1] ? 1 : 0)
plot(volume_row.has_sell_imbalance(r)[1] ? 1 : 0)
"
        ),
        &footprint_data(),
    );
    for (plot, want) in [
        (0, "[null,5.0,6.0,7.0]"),
        (1, "[null,3.0,4.0,5.0]"),
        (2, "[null,1.0,2.0,3.0]"),
        (3, "[null,1000.0,1000.0,1000.0]"),
        // A missing bool history value reads as false.
        (4, "[0.0,1.0,0.0,1.0]"),
        (5, "[0.0,0.0,1.0,0.0]"),
    ] {
        assert_eq!(values(&result, plot), want, "plot {plot}");
    }
}

#[test]
fn plot_colors_read_from_footprint_accessors_are_series() {
    let result = run(
        &format!(
            "{FP_HEADER}plot(close, color = footprint.delta(fp) > 13 ? color.red : color.blue)
plot(close, color = volume_row.has_buy_imbalance(r) ? color.green : color.gray)
plot(close, color = color.blue)
"
        ),
        &footprint_data(),
    );
    let modes: Vec<_> = result
        .plots
        .iter()
        .map(|plot| plot.style.plot_color_mode)
        .collect();
    use PlotColorMode::{Fixed, Series};
    assert_eq!(modes, vec![Some(Series), Some(Series), Some(Fixed)]);
    for plot in &result.plots[..2] {
        let mut seen: Vec<_> = plot.colors[0]
            .values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap())
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 2, "{:?}", plot.colors);
    }
}

#[test]
fn footprint_accessors_do_not_satisfy_simple_parameters() {
    for (body, code) in [
        ("plot(ta.ema(close, int(footprint.delta(fp))))", "E0219"),
        ("simple float d = footprint.delta(fp)\nplot(d)", "E0226"),
        (
            "simple bool b = volume_row.has_buy_imbalance(r)\nplot(b ? 1 : 0)",
            "E0226",
        ),
        (
            "plot(ta.ema(close, int(volume_row.total_volume(r))))",
            "E0219",
        ),
    ] {
        rejected_with(&format!("{FP_HEADER}{body}\n"), code);
    }
}

/// Every `ticker.*` function, and the `syminfo.prefix()` / `syminfo.ticker()`
/// function forms, called with a `series string` that changes per bar.
const TICKER_SERIES_BODY: &str = r#"s = close > open ? "AAA" : "BBB"
n = close > open ? 2 : 3
plot(ticker.new("X", s) == "new:X:AAA" ? 1 : 0)
plot(ticker.new("X", "Y", session = s) == "new:X:Y:AAA" ? 1 : 0)
plot(ticker.modify(s) == "modify:AAA" ? 1 : 0)
plot(ticker.heikinashi(s) == "heikinashi:AAA" ? 1 : 0)
plot(ticker.kagi(s, 3) == "kagi:AAA:3" ? 1 : 0)
plot(ticker.linebreak("X", n) == "linebreak:X:2" ? 1 : 0)
plot(ticker.pointfigure(s, "hl", "Traditional", 1, 3) == "pointfigure:AAA:hl:Traditional:1:3" ? 1 : 0)
plot(ticker.renko("X", s, n) == "renko:X:AAA:2" ? 1 : 0)
plot(ticker.standard(s) == "standard:AAA" ? 1 : 0)
plot(ticker.inherit("X", s) == "inherit:X:AAA" ? 1 : 0)
plot(syminfo.prefix(s + ":Z") == "AAA" ? 1 : 0)
plot(syminfo.ticker("E:" + s) == "AAA" ? 1 : 0)
plot(str.length(ticker.new("X", s)[1]))
"#;

#[test]
fn ticker_functions_accept_series_arguments_and_evaluate_per_bar() {
    let source = format!("//@version=6\nindicator(\"ticker series\")\n{TICKER_SERIES_BODY}");
    let result = run(&source, &footprint_data());
    // open alternates 10, 12, 10, 12 against close 11: up, down, up, down.
    for plot in 0..12 {
        assert_eq!(
            values(&result, plot),
            "[1.0,0.0,1.0,0.0]",
            "plot {plot}:\n{source}"
        );
    }
    assert_eq!(values(&result, 12), "[null,9.0,9.0,9.0]");
}

#[test]
fn ticker_results_keep_the_qualifier_of_their_arguments() {
    // A series argument selects the `series string` overload, which cannot
    // fill a `simple string` parameter.
    rejected_with(
        "//@version=6\nindicator(\"t\")\ns = close > open ? \"AAA\" : \"BBB\"\nsimple string t = ticker.new(\"X\", s)\nplot(close)\n",
        "E0226",
    );
    // Simple and constant arguments still give a `simple`-usable result.
    let result = run(
        "//@version=6\nindicator(\"t\")\nsimple string t = ticker.new(\"X\", syminfo.ticker)\nsimple string h = ticker.heikinashi(t)\nplot(str.length(h))\n",
        &footprint_data(),
    );
    assert_eq!(result.plots.len(), 1);
}

#[test]
fn ticker_results_from_constant_arguments_are_simple_not_const() {
    // The `ticker.*` and `syminfo.*` functions return `simple string` or
    // `series string`, so even all-literal arguments give a value that cannot
    // fill a `const string`.
    for call in [
        "ticker.new(\"X\", \"Y\")",
        "ticker.modify(\"X:Y\")",
        "ticker.heikinashi(\"X:Y\")",
        "ticker.kagi(\"X:Y\", 3)",
        "ticker.linebreak(\"X:Y\", 3)",
        "ticker.pointfigure(\"X:Y\", \"hl\", \"Traditional\", 1, 3)",
        "ticker.renko(\"X:Y\", \"ATR\", 10)",
        "ticker.standard(\"X:Y\")",
        "ticker.inherit(\"X:Y\", \"Z\")",
        "syminfo.prefix(\"X:Y\")",
        "syminfo.ticker(\"X:Y\")",
    ] {
        rejected_with(
            &format!("//@version=6\nindicator(\"t\")\nconst string t = {call}\nplot(close)\n"),
            "E0226",
        );
        let result = compile(
            &format!(
                "//@version=6\nindicator(\"t\")\nsimple string t = {call}\nplot(str.length(t))\n"
            ),
            CompileOptions::default(),
        );
        assert!(result.success, "{call}: {:?}", result.diagnostics);
    }
}

#[test]
fn series_ticker_ids_are_refused_as_request_symbols() {
    // A request's symbol selector is resolved when the script compiles (a
    // constant, an input or the chart feed), so a symbol that changes
    // mid-history cannot be honoured. It must fail to compile, never run
    // against one of its values.
    let prelude = "//@version=6\nindicator(\"t\")\ns = bar_index < 3 ? \"AAA\" : \"BBB\"\nf(x) => ticker.new(\"X\", x)\nstring held = ticker.new(\"X\", s)\n";
    for (body, code) in [
        (
            "plot(request.security(ticker.new(\"X\", s), \"1D\", close))",
            "E0242",
        ),
        (
            "plot(request.security(ticker.heikinashi(s), \"1D\", close))",
            "E0242",
        ),
        (
            "plot(request.security(ticker.linebreak(\"X\", bar_index + 1), \"1D\", close))",
            "E0242",
        ),
        (
            "plot(request.security(syminfo.prefix(s) + \":Y\", \"1D\", close))",
            "E0242",
        ),
        ("plot(request.security(held, \"1D\", close))", "E0242"),
        ("plot(request.security(f(s), \"1D\", close))", "E0242"),
        (
            "plot(array.size(request.security_lower_tf(ticker.new(\"X\", s), \"1\", close)))",
            "E0246",
        ),
        ("plot(request.dividends(ticker.new(\"X\", s)))", "E0242"),
        (
            "plot(request.financial(ticker.new(\"X\", s), \"TOTAL_REVENUE\", \"FQ\"))",
            "E0242",
        ),
        (
            "plot(request.seed(\"src\", ticker.new(\"X\", s), close))",
            "E0242",
        ),
    ] {
        rejected_with(&format!("{prelude}{body}\n"), code);
    }
}

#[test]
fn ticker_selector_tokens_stay_simple() {
    // `backadjustment` and `settlement_as_close` are `simple` in both
    // overload families of `ticker.new` and `ticker.modify`.
    for call in [
        "ticker.new(\"X\", \"Y\", backadjustment = b)",
        "ticker.new(\"X\", \"Y\", settlement_as_close = c)",
        "ticker.modify(\"X:Y\", backadjustment = b)",
        "ticker.modify(\"X:Y\", settlement_as_close = c)",
    ] {
        rejected_with(
            &format!(
                "//@version=6\nindicator(\"t\")\nb = close > open ? backadjustment.on : backadjustment.off\nc = close > open ? settlement_as_close.on : settlement_as_close.off\nplot(str.length({call}))\n"
            ),
            "E0206",
        );
    }
}

#[test]
fn ticker_box_and_line_counts_take_int_arguments_only() {
    // `ticker.linebreak`'s `number_of_lines` and `ticker.pointfigure`'s
    // `reversal` are `int` in both overloads of each. Pine does not convert a float to an int implicitly, so a
    // float argument is a compile error there, as it is for any `int`
    // parameter.
    let prelude = "//@version=6\nindicator(\"t\")\nn = input.int(7, \"N\")\n";
    for call in [
        "ticker.linebreak(\"X:Y\", 2.5)",
        "ticker.linebreak(\"X:Y\", close)",
        "ticker.linebreak(\"X:Y\", float(n))",
        "ticker.pointfigure(\"X:Y\", \"hl\", \"Traditional\", 1, 2.5)",
        "ticker.pointfigure(\"X:Y\", \"hl\", \"Traditional\", 1, close)",
    ] {
        rejected_with(&format!("{prelude}plot(str.length({call}))\n"), "E0205");
    }
    // An int of any qualifier is accepted, including an `int / int`
    // quotient, which stays `int`. The other numeric ticker parameters accept
    // an `int` or a `float`.
    for call in [
        "ticker.linebreak(\"X:Y\", 3)",
        "ticker.linebreak(\"X:Y\", n)",
        "ticker.linebreak(\"X:Y\", n / 2)",
        "ticker.linebreak(\"X:Y\", bar_index + 1)",
        "ticker.pointfigure(\"X:Y\", \"hl\", \"Traditional\", 2.5, bar_index + 1)",
        "ticker.pointfigure(\"X:Y\", \"hl\", \"ATR\", 14, n / 2)",
        "ticker.kagi(\"X:Y\", 2.5)",
        "ticker.kagi(\"X:Y\", close)",
        "ticker.renko(\"X:Y\", \"Traditional\", 2.5)",
        "ticker.renko(\"X:Y\", \"Traditional\", close)",
    ] {
        let result = compile(
            &format!("{prelude}plot(str.length({call}))\n"),
            CompileOptions::default(),
        );
        assert!(result.success, "{call}: {:?}", result.diagnostics);
    }
}
