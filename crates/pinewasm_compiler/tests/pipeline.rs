//! End-to-end pipeline tests: fixtures, expected-output checks, API totality.

use pinewasm_compiler::{
    compile, compile_and_run, compile_with_libraries, version_info, CompileOptions, MarketData,
    PhaseTimings, RunOptions, Value, MAX_LIBRARY_COUNT, MAX_LIBRARY_PATH_LEN,
    MAX_LIBRARY_SOURCE_BYTES, MAX_SOURCE_LEN,
};
use std::fs;
use std::path::PathBuf;

fn fixtures_dir(kind: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests")
        .join(kind)
}

fn read_fixtures(kind: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = fs::read_dir(fixtures_dir(kind))
        .expect("fixtures directory exists")
        .map(|e| e.expect("readable dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "pine"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let text = fs::read_to_string(&p).expect("readable fixture");
            (name, text)
        })
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no fixtures found in {kind}");
    entries
}

fn sample_data() -> MarketData {
    MarketData {
        open: vec![9.0, 11.0, 12.0, 10.5],
        high: vec![10.5, 12.5, 12.0, 11.0],
        low: vec![8.5, 10.5, 10.0, 9.5],
        close: vec![10.0, 12.0, 11.0, 10.75],
        volume: vec![1000.0, 1500.0, 800.0, 1200.0],
        ..MarketData::default()
    }
}

#[test]
fn all_compile_pass_fixtures_compile_and_run() {
    for (name, source) in read_fixtures("compile-pass") {
        let result = compile(&source, CompileOptions::all());
        assert!(
            result.success,
            "fixture {name} failed to compile: {:?}",
            result.diagnostics
        );
        assert!(result.ast_debug.is_some(), "{name}: missing AST dump");
        assert!(result.hir_debug.is_some(), "{name}: missing HIR dump");
        assert!(result.mir_debug.is_some(), "{name}: missing MIR dump");
        assert!(result.csil_text.is_some(), "{name}: missing CSIL dump");

        let run = compile_and_run(
            &source,
            &sample_data(),
            CompileOptions::default(),
            &RunOptions::default(),
        );
        assert!(
            run.runtime_errors.is_empty(),
            "fixture {name} hit runtime errors: {:?}",
            run.runtime_errors
        );
        assert_eq!(run.bars_executed, 4, "{name}: wrong bar count");
    }
}

#[test]
fn all_compile_fail_fixtures_produce_errors_without_panicking() {
    for (name, source) in read_fixtures("compile-fail") {
        let result = compile(&source, CompileOptions::all());
        assert!(!result.success, "fixture {name} unexpectedly compiled");
        assert!(
            result
                .diagnostics
                .iter()
                .any(pinewasm_span::Diagnostic::is_error),
            "fixture {name} produced no error diagnostics"
        );
        assert!(
            result.program.is_none(),
            "{name}: program present despite errors"
        );
        // Every error diagnostic carries a code and (for these fixtures) a span.
        for d in result.diagnostics.iter().filter(|d| d.is_error()) {
            assert!(d.code.is_some(), "{name}: diagnostic without code: {d:?}");
        }
    }
}

/// The input-length crossover fixture
/// (`tests/compile-pass/golden_cross_input_lengths.pine`): an EMA/SMA crossover
/// whose `ta.ema`/`ta.sma` lengths come from `input.int(...)`.
/// An input can be used as a `simple int` length (fixed for the run); PineWASM
/// bakes the input's default as the compile-time engine window. This pins that
/// the crossover fixture compiles clean with no `E0219`.
#[test]
fn golden_cross_input_lengths_compile() {
    let source =
        fs::read_to_string(fixtures_dir("compile-pass").join("golden_cross_input_lengths.pine"))
            .expect("input-length crossover fixture");
    let result = compile(&source, CompileOptions::all());
    assert!(
        result.success,
        "input-length crossover should compile with input lengths: {:?}",
        result.diagnostics
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.is_error() && d.code.as_deref() == Some("E0219")),
        "no E0219 expected for input lengths: {:?}",
        result.diagnostics
    );
}

/// Difference from the prior close: close [10, 12, 11] → plot [na, 2, -1].
#[test]
fn golden_close_difference_example() {
    let source = "//@version=6\nindicator(\"Example\")\nx = close - close[1]\nplot(x)\n";
    let close = vec![10.0, 12.0, 11.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result.runtime_errors.is_empty());
    assert_eq!(result.plots.len(), 1);
    let json = serde_json::to_string(&result.plots[0].values).unwrap();
    assert_eq!(json, "[null,2.0,-1.0]");
    // An indicator carries no strategy section on the wire.
    assert!(result.strategy.is_none());
    let wire = serde_json::to_string(&result).unwrap();
    assert!(!wire.contains("\"strategy\""), "wire: {wire}");
}

#[test]
fn display_style_and_visual_channels_serialize_for_hosts() {
    let source = "//@version=6\nindicator(\"Styled\")\n\
        plot(close, title = \"Columns\", color = close > open ? color.green : color.new(color.red, 40), linewidth = 4, style = plot.style_columns, histbase = -1, display = display.pane)\n\
        barcolor(close > open ? color.green : color.red, title = \"Bars\")\n";
    let result = compile_and_run(
        source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );

    let wire = serde_json::to_value(&result).expect("run result serializes");
    let plot = &wire["plots"][0];
    assert_eq!(plot["style"]["kind"], "plot");
    assert_eq!(plot["style"]["plotStyle"], "columns");
    assert_eq!(plot["style"]["lineWidth"], 4);
    assert_eq!(plot["style"]["histogramBase"], -1.0);
    assert_eq!(plot["style"]["display"], "pane");
    assert_eq!(plot["colors"][0]["role"], "color");
    assert_eq!(plot["colors"][0]["values"].as_array().unwrap().len(), 4);

    let visual = &wire["visuals"][0];
    assert_eq!(visual["style"]["kind"], "barcolor");
    assert_eq!(visual["title"], "Bars");
    assert_eq!(visual["colors"][0]["role"], "color");
}

/// End-to-end test for a **runtime** history offset (`x[n]` with `n`
/// an int expression). Compiles and runs the whole pipeline and hand-checks
/// the plotted values against the feed.
#[test]
fn golden_dynamic_history_offset() {
    // close = [10, 12, 11, 10.75], open = [9, 11, 12, 10.5].
    let source = "//@version=6\nindicator(\"D\")\nn = 1\n\
        a = close[n]\n\
        b = (close - open)[n]\n\
        c = close[bar_index]\n\
        plot(a)\nplot(b)\nplot(c)\n";
    let data = MarketData {
        open: vec![9.0, 11.0, 12.0, 10.5],
        high: vec![10.5, 12.5, 12.0, 11.0],
        low: vec![8.5, 10.5, 10.0, 9.5],
        close: vec![10.0, 12.0, 11.0, 10.75],
        volume: vec![0.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 3);
    // a = close[1]: bar 0 na, then the prior close.
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[null,10.0,12.0,11.0]"
    );
    // b = (close - open)[1]: per-bar (close-open) = [1, 1, -1, 0.25]; [1] → shift.
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[null,1.0,1.0,-1.0]"
    );
    // c = close[bar_index]: reads bar 0's close (10) every bar.
    assert_eq!(
        serde_json::to_string(&result.plots[2].values).unwrap(),
        "[10.0,10.0,10.0,10.0]"
    );
}

/// End-to-end test for the `strategy(...)` declaration: the honored
/// parameters const-fold through the whole pipeline into the run's strategy
/// section, and the per-bar equity series is seeded at `initial_capital`.
#[test]
fn golden_strategy_declaration() {
    let source = "//@version=6\n\
        strategy(\"S\", initial_capital=10000)\n\
        plot(close)\n";
    let close = vec![10.0, 12.0, 11.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result.runtime_errors.is_empty());
    let strat = result.strategy.as_ref().expect("strategy section present");
    assert_eq!(strat.initial_capital, 10000.0);
    // The script places no orders: flat position, no trades,
    // and equity held at initial_capital across every executed bar.
    assert!(strat.trades.is_empty());
    assert_eq!(strat.position.size, 0.0);
    assert_eq!(strat.equity, vec![10000.0, 10000.0, 10000.0]);
    assert_eq!(strat.net_profit, 0.0);
    assert_eq!(strat.open_profit, 0.0);
    // The section serializes on the wire for a strategy script.
    let wire = serde_json::to_string(&result).unwrap();
    assert!(wire.contains("\"initialCapital\":10000"), "wire: {wire}");
}

/// End-to-end test for the order primitives + the `strategy.*` read
/// surface, pinning `tests/compile-pass/strategy_demo.pine`: a market entry
/// on bar 0 fills at bar 1's open, an exit bracket rides alongside it, and the
/// `strategy.*` getter plots read the resulting broker state back into the
/// script.
#[test]
fn golden_demo_strategy_orders_and_getters() {
    let source = fs::read_to_string(fixtures_dir("compile-pass").join("strategy_demo.pine"))
        .expect("strategy fixture");

    let result = compile(&source, CompileOptions::all());
    // The fixture is written to compile with no diagnostics.
    assert!(
        result.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert!(result.success, "diagnostics: {:?}", result.diagnostics);

    // The fixture exercises the whole strategy surface: the order primitives
    // (a market entry + an exit bracket) and the strategy.* getter series.
    let csil = result.csil_text.as_deref().expect("emit_csil on");
    assert!(
        csil.contains("ORDER strategy.entry"),
        "fixture should place a market entry:\n{csil}"
    );
    assert!(
        csil.contains("ORDER strategy.exit"),
        "fixture should place an exit bracket:\n{csil}"
    );
    assert!(
        csil.contains("READ_BUILTIN STRATEGY.POSITION_SIZE"),
        "fixture should read strategy.position_size:\n{csil}"
    );
    assert!(
        csil.contains("READ_BUILTIN STRATEGY.EQUITY"),
        "fixture should read strategy.equity:\n{csil}"
    );

    let run = compile_and_run(
        &source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.bars_executed, 4);

    let strat = run.strategy.as_ref().expect("strategy section present");
    assert_eq!(strat.initial_capital, 10000.0);
    // sample_data opens are [9, 11, 12, 10.5]. The bar-0 entry fills at bar 1's
    // open (11). The exit bracket's take-profit limit is 13.0; bar 2 trades up
    // to high 12.0 and bar 3 to high 11.0, so neither the 13.0 limit nor the 9.0
    // stop is ever touched — the long stays open through the final bar.
    assert_eq!(strat.position.size, 1.0, "one long contract open");
    assert_eq!(strat.position.avg_price, 11.0, "filled at bar 1 open");
    assert!(
        strat.trades.is_empty(),
        "bracket never triggered => no close"
    );
    assert_eq!(strat.net_profit, 0.0, "no realized profit while open");
    // open_profit marks the open long to the final close (10.75): 10.75 - 11.0.
    assert!(
        (strat.open_profit - (-0.25)).abs() < 1e-9,
        "open profit {} != -0.25",
        strat.open_profit
    );
    // The strategy.* getter plots read that same state back: eight plots, and
    // the position-size series is flat on bar 0 (`-0.0`: signed-zero flat) then
    // 1.0 from bar 1 on, once the entry has filled at that bar's open.
    assert_eq!(run.plots.len(), 8);
    assert_eq!(
        serde_json::to_string(&run.plots[0].values).unwrap(),
        "[-0.0,1.0,1.0,1.0]",
        "position_size: flat on bar 0, long from the bar-1 fill onward"
    );
    // The section serializes on the wire.
    let wire = serde_json::to_string(&run).unwrap();
    assert!(wire.contains("\"initialCapital\":10000"), "wire: {wire}");
}

/// The `strategy()`/`indicator()` mutual-exclusion rule surfaces as a compile
/// error (`E0240`) at the pipeline boundary, so no program is produced.
#[test]
fn strategy_and_indicator_mutual_exclusion_fails_compile() {
    let source = "strategy(\"S\")\nindicator(\"I\")\nplot(close)\n";
    let result = compile(source, CompileOptions::all());
    assert!(!result.success);
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some("E0240")));
}

/// End-to-end test for user-defined functions: a multi-line body
/// with a local and history on that local (`delta`), and two call sites of a
/// `var` counter that must keep independent state.
#[test]
fn golden_user_defined_functions() {
    let source = "//@version=6\n\
        indicator(\"F\")\n\
        delta(src) =>\n    \
            prev = src\n    \
            src - prev[1]\n\
        counter(step) =>\n    \
            var c = 0\n    \
            c := c + step\n    \
            c\n\
        plot(delta(close))\n\
        plot(counter(1))\n\
        plot(counter(2))\n";
    let close = vec![10.0, 12.0, 11.0, 10.75];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 3);
    // delta(close) == close - close[1]: na, 2, -1, -0.25.
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[null,2.0,-1.0,-0.25]"
    );
    // counter(1): 1, 2, 3, 4 — its own var slot (plot coerces the int counter
    // to float).
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[1.0,2.0,3.0,4.0]"
    );
    // counter(2): 2, 4, 6, 8 — an independent var slot, proving per-call-site
    // instantiation.
    assert_eq!(
        serde_json::to_string(&result.plots[2].values).unwrap(),
        "[2.0,4.0,6.0,8.0]"
    );
}

/// End-to-end test for user-defined methods: a pure method, a mutating method,
/// method chaining, two independent objects, a method calling another method,
/// per-call-site `var` state, and a var object mutated across bars.
#[test]
fn golden_udt_methods() {
    let source = "//@version=6\n\
        indicator(\"M\")\n\
        type Point\n    \
            float x = 0.0\n    \
            float y = 0.0\n\
        method length(Point this) =>\n    \
            math.sqrt(this.x * this.x + this.y * this.y)\n\
        method move(Point this, dx, dy) =>\n    \
            this.x := this.x + dx\n    \
            this.y := this.y + dy\n    \
            this\n\
        method scale(Point this, k) =>\n    \
            this.x := this.x * k\n    \
            this.y := this.y * k\n    \
            this\n\
        type Counter\n    \
            int base = 0\n\
        method tick(Counter this, step) =>\n    \
            var int n = 0\n    \
            n := n + step\n    \
            this.base + n\n\
        a = Point.new(3.0, 4.0)\n\
        plot(a.length())\n\
        b = Point.new(1.0, 2.0)\n\
        b.move(1.0, 1.0).scale(2.0)\n\
        plot(b.x)\n\
        plot(b.y)\n\
        ctr = Counter.new(100)\n\
        plot(ctr.tick(1))\n\
        plot(ctr.tick(10))\n\
        var acc = Point.new(0.0, 0.0)\n\
        acc.move(close, 0.0)\n\
        plot(acc.x)\n";
    let close = vec![10.0, 12.0, 11.0, 10.75];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 6);
    // length() on (3,4) → 5 every bar.
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[5.0,5.0,5.0,5.0]"
    );
    // chaining: (1,2).move(1,1)=(2,3).scale(2)=(4,6): x=4, y=6.
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[4.0,4.0,4.0,4.0]"
    );
    assert_eq!(
        serde_json::to_string(&result.plots[2].values).unwrap(),
        "[6.0,6.0,6.0,6.0]"
    );
    // tick(1): base 100 + accumulating n by 1 → 101,102,103,104. Fresh
    // per-call-site var state.
    assert_eq!(
        serde_json::to_string(&result.plots[3].values).unwrap(),
        "[101.0,102.0,103.0,104.0]"
    );
    // tick(10): independent var slot → 110,120,130,140.
    assert_eq!(
        serde_json::to_string(&result.plots[4].values).unwrap(),
        "[110.0,120.0,130.0,140.0]"
    );
    // acc.x accumulates close across bars via a var object: 10, 22, 33, 43.75.
    assert_eq!(
        serde_json::to_string(&result.plots[5].values).unwrap(),
        "[10.0,22.0,33.0,43.75]"
    );
}

/// End-to-end test for collections-of-UDTs: an `array<Point>` built
/// with pushes, a field read through `array.get`, a field mutation through a
/// retrieved element visible through the array (reference semantics), and a
/// `map<string, Point>` of object values.
#[test]
fn golden_collections_of_udts() {
    let source = "//@version=6\n\
        indicator(\"C\")\n\
        type Point\n    \
            float x = 0.0\n    \
            float y = 0.0\n\
        objs = array.new<Point>()\n\
        array.push(objs, Point.new(3.0, 4.0))\n\
        array.push(objs, Point.new(5.0, 6.0))\n\
        first = array.get(objs, 0)\n\
        plot(first.x + first.y)\n\
        first.x := 99.0\n\
        again = array.get(objs, 0)\n\
        plot(again.x)\n\
        plot(array.size(objs))\n\
        m = map.new<string, Point>()\n\
        map.put(m, \"a\", Point.new(1.0, 2.0))\n\
        map.put(m, \"b\", Point.new(3.0, 4.0))\n\
        pa = map.get(m, \"a\")\n\
        pb = map.get(m, \"b\")\n\
        plot(pa.x + pb.y)\n\
        plot(map.size(m))\n";
    let close = vec![10.0, 12.0, 11.0, 10.75];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 5);
    // first.x + first.y from the first pushed Point (3,4) → 7.
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[7.0,7.0,7.0,7.0]"
    );
    // After `first.x := 99`, a fresh get sees the mutation (reference
    // semantics through the array).
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[99.0,99.0,99.0,99.0]"
    );
    // array size is 2.
    assert_eq!(
        serde_json::to_string(&result.plots[2].values).unwrap(),
        "[2.0,2.0,2.0,2.0]"
    );
    // map<string, Point>: pa.x(1) + pb.y(4) = 5.
    assert_eq!(
        serde_json::to_string(&result.plots[3].values).unwrap(),
        "[5.0,5.0,5.0,5.0]"
    );
    // map size is 2.
    assert_eq!(
        serde_json::to_string(&result.plots[4].values).unwrap(),
        "[2.0,2.0,2.0,2.0]"
    );
}

/// End-to-end test for tuple destructuring: a `ta.bb` 3-tuple is
/// destructured, and the three plotted bands satisfy the middle-is-sma and
/// symmetric-band identities exactly.
#[test]
fn golden_tuple_destructuring() {
    let source = "//@version=6\n\
        indicator(\"T\")\n\
        [middle, upper, lower] = ta.bb(close, 3, 2.0)\n\
        plot(middle)\n\
        plot(upper - lower)\n\
        plot(ta.sma(close, 3))\n";
    let close = vec![10.0, 12.0, 8.0, 14.0, 10.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 5],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 3);
    // middle == sma(close, 3): warm-up na, then hand-computed means.
    // windows: [10,12,8]→10, [12,8,14]→11.333…, [8,14,10]→10.666…
    let middle: Vec<Option<f64>> = plot_floats(&result.plots[0]);
    let sma: Vec<Option<f64>> = plot_floats(&result.plots[2]);
    assert_eq!(middle, sma, "bb middle must equal ta.sma elementwise");
    assert_eq!(middle[0], None);
    assert_eq!(middle[1], None);
    approx(middle[2], 10.0);
    // upper − lower == 2 · (2 · population stdev) = 4 · stdev. On the first
    // full window [10,12,8] the mean is 10, stdev = sqrt((0+4+4)/3).
    let width = plot_floats(&result.plots[1]);
    let stdev = ((0.0_f64 + 4.0 + 4.0) / 3.0).sqrt();
    approx(width[2], 4.0 * stdev);
}

/// End-to-end test for colors: a per-bar color is selected by a
/// condition, then compared to a named constant, driving a plotted 1.0/0.0.
/// This threads color literals, `color.*` constants, the ternary `Select`
/// over colors, and color equality through the whole pipeline.
#[test]
fn golden_colors() {
    let source = "//@version=6\n\
        indicator(\"C\")\n\
        picked = close > open ? color.green : #FF5352\n\
        plot(picked == color.green ? 1.0 : 0.0)\n";
    // close [10,12,11,10.75] vs open [9,11,12,10.5] → close>open [T,T,F,T].
    // picked = green on up bars, and the literal #FF5352 (a color that differs
    // from color.green) on the down bar; the plot is 1.0 where picked ==
    // green, else 0.0.
    let json = run_single_plot(source);
    assert_eq!(json, "[1.0,1.0,0.0,1.0]");
}

/// Colors cross the wasm/JSON boundary as `#RRGGBB[AA]` hex strings (the
/// documented wire format). A `str.tostring` cannot stringify a color, so we
/// assert the wire mapping directly on `Value::Color`.
#[test]
fn color_value_serializes_as_hex_on_the_wire() {
    use pinewasm_compiler::Value;
    assert_eq!(
        serde_json::to_string(&Value::Color {
            r: 0xFF,
            g: 0x53,
            b: 0x52,
            t: 0.0,
            input_origin: None
        })
        .unwrap(),
        "\"#FF5352\""
    );
    assert_eq!(
        serde_json::to_string(&Value::Color {
            r: 0x80,
            g: 0x80,
            b: 0x80,
            t: 50.0,
            input_origin: None
        })
        .unwrap(),
        "\"#80808080\""
    );
}

/// Convert a plot series to `Option<f64>`s for value assertions.
fn plot_floats(series: &pinewasm_compiler::PlotSeries) -> Vec<Option<f64>> {
    let json = serde_json::to_string(&series.values).unwrap();
    let parsed: Vec<Option<f64>> = serde_json::from_str(&json).unwrap();
    parsed
}

fn approx(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value, found na");
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected ~{expected}, found {actual}"
    );
}

/// A function containing `ta.sma` called at two sites keeps **independent
/// windows**: `smooth(close)` and `smooth(open)` produce divergent output,
/// proving per-call-site state (not shared).
#[test]
fn golden_function_ta_state_is_per_call_site() {
    let source = "//@version=6\n\
        indicator(\"F\")\n\
        smooth(src) => ta.sma(src, 2)\n\
        plot(smooth(close))\n\
        plot(smooth(open))\n";
    let data = MarketData {
        open: vec![1.0, 2.0, 3.0],
        high: vec![1.0, 2.0, 3.0],
        low: vec![1.0, 2.0, 3.0],
        close: vec![10.0, 20.0, 30.0],
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result.runtime_errors.is_empty());
    assert_eq!(result.plots.len(), 2);
    // sma(close, 2): na, (10+20)/2=15, (20+30)/2=25.
    assert_eq!(
        serde_json::to_string(&result.plots[0].values).unwrap(),
        "[null,15.0,25.0]"
    );
    // sma(open, 2): na, 1.5, 2.5 — a different window, not the close one.
    assert_eq!(
        serde_json::to_string(&result.plots[1].values).unwrap(),
        "[null,1.5,2.5]"
    );
}

/// A host-facing example that must compile and execute without
/// diagnostics or runtime errors.
#[test]
fn representative_host_example_compiles_and_runs() {
    let source = r#"//@version=6
indicator("Momentum")
var count = 0
count := count + 1
length = input.int(3, title="SMA Length", minval=1)
src = input.source(close, title="Source")
bull = close > open ? color.green : #EF5350
scale(n) =>
    total = 0.0
    for i = 1 to n
        total := total + i
    total
fast = ta.sma(src, 3)
change = ta.change(src)
regime = switch
    close > open => 1.0
    close < open => -1.0
    => 0.0
marker = barstate.islast ? fast : na
prices = array.new_float()
array.push(prices, high)
array.push(prices, low)
upFast = change > 0 ? fast : na
downFast = change <= 0 ? fast * 0.5 : na
plot(upFast, "fast (up bars)")
plot(downFast, "half fast (down bars)")
plot((high + low) / 2, "median")
plot(change > 0 ? math.abs(change) : 0.0, "up momentum")
plot(scale(length), "triangular(len)")
plot(regime, "regime")
plot(marker, "last-bar marker")
plot(array.avg(prices), "hl avg (array)")
if barstate.islast
    tag = label.new(bar_index, close, str.format("close={0}", close), style = label.style_label_down, textcolor = color.white)
    label.set_size(tag, size.large)
"#;

    let result = compile(source, CompileOptions::all());
    // The sample must compile with no errors *and* no warnings, so it compiles
    // with no diagnostics.
    assert!(
        result.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        result.diagnostics
    );
    assert!(result.success, "diagnostics: {:?}", result.diagnostics);

    // The sample exercises a broad feature surface: a named argument
    // (`title=`/`minval=` on `input.int`), a color (the `color.green` named
    // constant and the `#EF5350` literal in the `bull` ternary), inputs
    // (`input.int` and `input.source`), a `barstate.*` predicate read (the
    // `barstate.islast` marker), and a reference-typed `array<float>` collection
    // (`array.new_float`/`array.push`/`array.avg`). Assert those appear in the
    // CSIL so the assertions stay in step if the sample changes.
    let csil = result.csil_text.as_deref().expect("emit_csil on");
    assert!(
        csil.contains("READ_INPUT"),
        "sample should read an input:\n{csil}"
    );
    assert!(
        csil.contains("INPUT 0 input.int"),
        "sample should declare an input.int:\n{csil}"
    );
    assert!(
        csil.contains("INPUT 1 input.source"),
        "sample should declare an input.source:\n{csil}"
    );
    assert!(
        csil.contains("PUSH_COLOR"),
        "sample should push a color:\n{csil}"
    );
    assert!(
        csil.contains("READ_BUILTIN BARSTATE.ISLAST"),
        "sample should read the barstate.islast predicate:\n{csil}"
    );
    assert!(
        csil.contains("CALL_COLLECTION array.new_float"),
        "sample should construct an array:\n{csil}"
    );
    assert!(
        csil.contains("CALL_COLLECTION array.push"),
        "sample should push to the array:\n{csil}"
    );
    assert!(
        csil.contains("CALL_COLLECTION array.avg"),
        "sample should aggregate the array:\n{csil}"
    );
    assert!(
        csil.contains("REGISTER_DRAWING label"),
        "sample should register a label drawing:\n{csil}"
    );

    let run = compile_and_run(
        source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        run.runtime_errors
    );
    assert_eq!(run.bars_executed, 4);
    // Eight syntactic plot calls, one slot each in source order: the two `if`
    // arms, the unconditional median, the ternary/math.abs momentum plot, the
    // loop-function output, the subjectless-switch regime, the barstate.islast
    // marker, and the array `hl avg` aggregate.
    assert_eq!(run.plots.len(), 8);
    // scale(length) runs a `for i = 1 to length` loop summing 1+2+3 = 6 every
    // bar (length is the input.int default 3), exercising the user-defined
    // function + bounded loop path fed by an `input`-qualified bound.
    assert_eq!(
        serde_json::to_string(&run.plots[4].values).unwrap(),
        "[6.0,6.0,6.0,6.0]"
    );
    // regime is a subjectless switch over close vs open
    // (close [10,12,11,10.75] vs open [9,11,12,10.5] → up,up,down,up).
    assert_eq!(
        serde_json::to_string(&run.plots[5].values).unwrap(),
        "[1.0,1.0,-1.0,1.0]"
    );
    // marker = barstate.islast ? fast : na. In a history-only run `islast` is
    // true only on the final bar, so the marker is `na` on bars 0..3 and
    // fast's last value on bar 3: ta.sma(close, 3) = (12+11+10.75)/3 = 11.25.
    assert_eq!(
        serde_json::to_string(&run.plots[6].values).unwrap(),
        "[null,null,null,11.25]"
    );
    // prices is a fresh `array<float>` each bar holding [high, low]; array.avg
    // is their mean, i.e. (high + low) / 2. sample high/low give
    // [(10.5+8.5)/2, (12.5+10.5)/2, (12.0+10.0)/2, (11.0+9.5)/2].
    assert_eq!(
        serde_json::to_string(&run.plots[7].values).unwrap(),
        "[9.5,11.5,11.0,10.25]"
    );
    // The `if barstate.islast` guard creates exactly one label drawing — on the
    // final bar only — so the `drawings` DTO carries a single live label (and no
    // lines/boxes) that hosts render alongside the plots. Its text is the
    // `str.format`-rendered close on the last bar (10.75 in the sample).
    assert_eq!(run.drawings.labels.len(), 1);
    assert!(run.drawings.lines.is_empty());
    assert!(run.drawings.boxes.is_empty());
    let label = &run.drawings.labels[0];
    assert_eq!(label.kind, "label");
    assert_eq!(
        label.fields.get("text"),
        Some(&Value::String("close=10.75".to_string()))
    );
    // `label.set_size` mutated the live object, so the serialized field reflects
    // the setter, not the `size.normal` default.
    assert_eq!(
        label.fields.get("size"),
        Some(&Value::String("size.large".to_string()))
    );
}

#[test]
fn label_new_and_set_size_accept_series_numeric_point_sizes() {
    let source = "indicator(\"Numeric label size\")\nlabel_id = label.new(bar_index, close, \"x\", size=bar_index + 4)\nlabel.set_size(label_id, bar_index + 6)\nlabel.new(chart.point.from_index(bar_index, close), \"point\", size=bar_index + 8)\nplot(close)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let run = compile_and_run(
        source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.drawings.labels.len(), 8);
    assert_eq!(
        run.drawings
            .labels
            .iter()
            .map(|label| label.fields.get("size").cloned())
            .collect::<Vec<_>>(),
        vec![
            Some(Value::Int(6)),
            Some(Value::Int(8)),
            Some(Value::Int(7)),
            Some(Value::Int(9)),
            Some(Value::Int(8)),
            Some(Value::Int(10)),
            Some(Value::Int(9)),
            Some(Value::Int(11))
        ]
    );
}

#[test]
fn label_new_and_set_size_reject_float_values() {
    for source in [
        "indicator(\"Numeric label size\")\nlabel.new(bar_index, close, \"x\", size=4.5)\nplot(close)\n",
        "indicator(\"Numeric label size\")\nlabel.new(chart.point.from_index(bar_index, close), \"x\", size=4.5)\nplot(close)\n",
        "indicator(\"Numeric label size\")\nlabel_id = label.new(bar_index, close, \"x\")\nlabel.set_size(label_id, 4.5)\nplot(close)\n",
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(!result.success);
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some("E0237")
                && diagnostic.message.contains("text size expects int or string")
        }), "{:?}", result.diagnostics);
    }
}

#[test]
fn box_and_table_text_size_accept_series_numeric_point_sizes() {
    let source = "indicator(\"Numeric drawing text size\")\nbox_id = box.new(bar_index, high, bar_index + 1, low, text=\"x\", text_size=bar_index + 4)\nbox.set_text_size(box_id, bar_index + 6)\nbox.new(chart.point.from_index(bar_index, high), chart.point.from_index(bar_index + 1, low), text=\"p\", text_size=bar_index + 8)\nvar table_id = table.new(position.top_right, 1, 1)\ntable.cell(table_id, 0, 0, \"x\", text_size=bar_index + 4)\ntable.cell_set_text_size(table_id, 0, 0, bar_index + 6)\nplot(close)\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let run = compile_and_run(
        source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.drawings.boxes.len(), 8);
    assert_eq!(
        run.drawings
            .boxes
            .iter()
            .map(|drawing| drawing.fields.get("text_size").cloned())
            .collect::<Vec<_>>(),
        vec![
            Some(Value::Int(6)),
            Some(Value::Int(8)),
            Some(Value::Int(7)),
            Some(Value::Int(9)),
            Some(Value::Int(8)),
            Some(Value::Int(10)),
            Some(Value::Int(9)),
            Some(Value::Int(11))
        ]
    );
    let table_cell = &run.drawings.tables[0].cells[0];
    assert_eq!(table_cell.attrs.get("text_size"), Some(&Value::Int(9)));
}

#[test]
fn box_and_table_text_size_reject_float_values() {
    for source in [
        "indicator(\"Numeric drawing text size\")\nbox.new(bar_index, high, bar_index + 1, low, text_size=4.5)\nplot(close)\n",
        "indicator(\"Numeric drawing text size\")\nbox.new(chart.point.from_index(bar_index, high), chart.point.from_index(bar_index + 1, low), text_size=4.5)\nplot(close)\n",
        "indicator(\"Numeric drawing text size\")\nbox_id = box.new(bar_index, high, bar_index + 1, low)\nbox.set_text_size(box_id, 4.5)\nplot(close)\n",
        "indicator(\"Numeric drawing text size\")\ntable_id = table.new(position.top_right, 1, 1)\ntable.cell(table_id, 0, 0, \"x\", text_size=4.5)\nplot(close)\n",
        "indicator(\"Numeric drawing text size\")\ntable_id = table.new(position.top_right, 1, 1)\ntable.cell_set_text_size(table_id, 0, 0, 4.5)\nplot(close)\n",
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(!result.success);
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_deref() == Some("E0237")
                && diagnostic.message.contains("text size expects int or string")
        }), "{:?}", result.diagnostics);
    }
}

#[test]
fn compile_result_serializes_to_json() {
    let result = compile("indicator(\"J\")\nplot(close)\n", CompileOptions::all());
    let json = serde_json::to_string(&result).expect("CompileResult must serialize");
    assert!(json.contains("\"success\":true"));
    assert!(json.contains("ilText"));
    assert!(!json.contains("csilText"));
}

#[test]
fn compile_options_accept_both_wire_spellings_but_serialize_stably() {
    let established: CompileOptions = serde_json::from_str(r#"{"emitIl":true}"#).unwrap();
    let expanded: CompileOptions = serde_json::from_str(r#"{"emitCsil":true}"#).unwrap();
    assert!(established.emit_csil);
    assert!(expanded.emit_csil);

    let json = serde_json::to_string(&expanded).unwrap();
    assert!(json.contains("\"emitIl\":true"));
    assert!(!json.contains("emitCsil"));
}

#[test]
fn timing_and_version_wire_keys_remain_stable() {
    let timing = PhaseTimings {
        csil_ms: 1.25,
        ..PhaseTimings::default()
    };
    let timing_json = serde_json::to_string(&timing).unwrap();
    assert!(timing_json.contains("\"ilMs\":1.25"));
    assert!(!timing_json.contains("csilMs"));

    let version_json = serde_json::to_string(&version_info()).unwrap();
    assert!(version_json.contains("\"ilVersion\":2"));
    assert!(!version_json.contains("csilVersion"));
    assert!(version_json.contains("\"renderingContractVersion\":1"));
    // The pinned IANA Time Zone Database release timezone names are read with.
    assert!(
        version_json.contains("\"tzdataVersion\":\"2026e\""),
        "{version_json}"
    );
}

#[test]
fn diagnostics_have_spans_pointing_at_the_problem() {
    let source = "//@version=6\nindicator(\"X\")\nplot(unknown_name + 1)\n";
    let result = compile(source, CompileOptions::default());
    assert!(!result.success);
    let diag = result.diagnostics.iter().find(|d| d.is_error()).unwrap();
    let span = diag.primary_span().expect("semantic errors carry spans");
    assert_eq!(
        &source[span.start as usize..span.end as usize],
        "unknown_name"
    );
}

#[test]
fn version_info_is_populated() {
    let v = version_info();
    assert_eq!(v.csil_version, 2);
    assert_eq!(v.language_version, 6);
    assert_eq!(v.rendering_contract_version, 1);
    assert_eq!(
        pinewasm_compiler::rendering_contract()["schemaVersion"],
        v.rendering_contract_version
    );
    assert_eq!(v.compiler_version, pinewasm_compiler::COMPILER_VERSION);
    let parts: Vec<_> = v.compiler_version.split('.').collect();
    assert_eq!(parts.len(), 3, "compiler version is MAJOR.MINOR.PATCH");
    assert!(parts.iter().all(|part| !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit())
        && (part.len() == 1 || !part.starts_with('0'))));
    if let Some(revision) = v.source_revision {
        assert_eq!(revision.len(), 40);
        assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn oversized_source_is_rejected_gracefully() {
    let big = "x".repeat((1 << 20) + 1);
    let result = compile(&big, CompileOptions::default());
    assert!(!result.success);
    assert!(result.diagnostics[0]
        .message
        .contains("maximum supported size"));
}

#[test]
fn the_facade_and_the_parser_refuse_an_oversized_source_identically() {
    // The facade's cap is the parser's, so a host that parses directly and a
    // host that compiles meet the same limit and the same diagnostic.
    assert_eq!(MAX_SOURCE_LEN, pinewasm_parse::MAX_SOURCE_LEN);
    let at_cap = " ".repeat(MAX_SOURCE_LEN);
    assert!(pinewasm_parse::parse(&at_cap).diagnostics.is_empty());
    assert!(compile(&at_cap, CompileOptions::default())
        .diagnostics
        .iter()
        .all(|d| d.code.as_deref() != Some("E0001")));

    let over = " ".repeat(MAX_SOURCE_LEN + 1);
    let compiled = compile(&over, CompileOptions::default());
    assert!(!compiled.success);
    assert_eq!(compiled.diagnostics.len(), 1);
    assert_eq!(compiled.diagnostics[0].code.as_deref(), Some("E0001"));
    assert_eq!(
        compiled.diagnostics,
        pinewasm_parse::parse(&over).diagnostics
    );
    assert_eq!(compiled.diagnostics, pinewasm_parse::lex(&over).diagnostics);
}

/// A source inside the size cap that lexes past the parser's token budget:
/// short lines alternating between two indentation levels, six tokens for
/// every five bytes.
fn source_over_the_token_budget() -> String {
    let pair = "a\n\ta\n";
    let source = pair.repeat(MAX_SOURCE_LEN / pair.len());
    assert!(source.len() <= MAX_SOURCE_LEN);
    assert!(pinewasm_parse::lex(&source).tokens.len() > pinewasm_parse::MAX_TOKENS);
    source
}

#[test]
fn a_source_over_the_token_budget_is_refused_before_it_is_parsed() {
    let result = compile(&source_over_the_token_budget(), CompileOptions::all());
    assert!(!result.success);
    assert!(result.program.is_none());
    assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
    assert_eq!(result.diagnostics[0].code.as_deref(), Some("E0001"));
    assert!(
        result.diagnostics[0]
            .message
            .contains("tokens; the maximum supported count"),
        "{}",
        result.diagnostics[0].message
    );
}

#[test]
fn a_library_over_the_token_budget_is_refused_on_its_own_buffer() {
    let main = "//@version=6\nindicator(\"M\")\nimport acme/Dense/1 as dense\nplot(close)\n";
    let libraries = vec![("acme/Dense/1".to_owned(), source_over_the_token_budget())];
    let result = compile_with_libraries(main, &libraries, CompileOptions::default());
    assert!(!result.success);
    // The import site says which library failed ...
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| { d.code.as_deref() == Some("E0243") && d.message.contains("acme/Dense/1") }),
        "{:?}",
        result.diagnostics
    );
    // ... and the parser's refusal follows it, as the library's own
    // diagnostics do.
    let refusal = result
        .diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("E0001"))
        .expect("the token budget diagnostic");
    assert!(
        refusal
            .message
            .contains("tokens; the maximum supported count"),
        "{}",
        refusal.message
    );
}

#[test]
fn library_payload_limits_apply_before_import_resolution() {
    let too_many = vec![("acme/Lib/1".to_owned(), String::new()); MAX_LIBRARY_COUNT + 1];
    let result = compile_with_libraries("", &too_many, CompileOptions::default());
    assert!(!result.success);
    assert!(result.diagnostics[0]
        .message
        .contains("maximum supported count"));

    let long_path = vec![("x".repeat(MAX_LIBRARY_PATH_LEN + 1), String::new())];
    let result = compile_with_libraries("", &long_path, CompileOptions::default());
    assert!(!result.success);
    assert!(result.diagnostics[0].message.contains("library path"));

    let oversized_unused = vec![("acme/Unused/1".to_owned(), "x".repeat(MAX_SOURCE_LEN + 1))];
    let result = compile_with_libraries("", &oversized_unused, CompileOptions::default());
    assert!(!result.success);
    assert!(result.diagnostics[0]
        .message
        .contains("library `acme/Unused/1`"));

    let source = "x".repeat(MAX_SOURCE_LEN);
    let aggregate = (0..=(MAX_LIBRARY_SOURCE_BYTES / MAX_SOURCE_LEN))
        .map(|index| (format!("acme/Lib{index}/1"), source.clone()))
        .collect::<Vec<_>>();
    let result = compile_with_libraries("", &aggregate, CompileOptions::default());
    assert!(!result.success);
    assert!(result.diagnostics[0]
        .message
        .contains("maximum supported total"));
}

#[test]
fn empty_source_compiles_to_empty_program() {
    let result = compile("", CompileOptions::default());
    // No statements, no indicator: still structurally fine (warning only).
    assert!(result.success, "diagnostics: {:?}", result.diagnostics);
    let run = compile_and_run(
        "",
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty());
    assert!(run.plots.is_empty());
}

mod robustness {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// compile() is total over arbitrary input.
        #[test]
        fn compile_never_panics(source in "\\PC*") {
            let _ = compile(&source, CompileOptions::all());
        }

        /// compile_and_run() is total over arbitrary input and data.
        #[test]
        fn compile_and_run_never_panics(
            source in "(indicator\\(\"a\"\\)|plot|close|open|x|=|\\[|\\]|\\(|\\)|[0-9]|\\n|    |if | > ){0,30}",
            close in proptest::collection::vec(-1e6f64..1e6, 0..8),
        ) {
            let data = MarketData {
                open: close.clone(),
                high: close.clone(),
                low: close.clone(),
                close: close.clone(),
                volume: vec![1.0; close.len()],
                ..MarketData::default()
            };
            let _ = compile_and_run(&source, &data, CompileOptions::default(), &RunOptions::default());
        }
    }
}

/// End-to-end: a `for` loop summing `1..=bar_index` accumulates the triangular
/// numbers across bars, exercising the full parse → HIR → MIR → CSIL → VM
/// pipeline for loops (back-edges, per-bar reset, loop-local counter).
#[test]
fn for_loop_end_to_end_triangular_numbers() {
    let source = "//@version=6\nindicator(\"Tri\")\nvar s = 0\ns := 0\nfor i = 1 to bar_index\n    s := s + i\nplot(s)\n";
    let close = vec![10.0, 11.0, 12.0, 13.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 4],
        ..MarketData::default()
    };
    let result = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let json = serde_json::to_string(&result.plots[0].values).unwrap();
    // bar 0: for 1 to 0 descends over {1,0} → 1; then 1, 3, 6.
    assert_eq!(json, "[1.0,1.0,3.0,6.0]");
}

// ---------------------------------------------------------------------------
// if / switch expressions
// ---------------------------------------------------------------------------

/// Run a single-plot script against `sample_data()` and return the JSON of
/// the first plot's values, asserting no errors.
fn run_single_plot(source: &str) -> String {
    let result = compile_and_run(
        source,
        &sample_data(),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let errors: Vec<_> = result.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    serde_json::to_string(&result.plots[0].values).unwrap()
}

/// `if` used as a value picks the taken branch's value. close > open on
/// bars [T, T, F, T] over close [10,12,11,10.75], open [9,11,12,10.5].
#[test]
fn if_expression_selects_branch_value() {
    let json = run_single_plot(
        "//@version=6\nindicator(\"x\")\np = if close > open\n    close\nelse\n    open\nplot(p)\n",
    );
    assert_eq!(json, "[10.0,12.0,12.0,10.75]");
}

/// A missing `else` yields `na` of the then-branch type on unmatched bars.
#[test]
fn if_expression_missing_else_is_na() {
    let json =
        run_single_plot("//@version=6\nindicator(\"x\")\np = if close > 11\n    close\nplot(p)\n");
    assert_eq!(json, "[null,12.0,null,null]");
}

/// Nested if-expressions compose: outer close>open, inner close>11.5.
#[test]
fn nested_if_expression() {
    let src = "//@version=6\nindicator(\"x\")\np = if close > open\n    if close > 11.5\n        1.0\n    else\n        2.0\nelse\n    3.0\nplot(p)\n";
    // close>open [T,T,F,T]; close>11.5 [F,T,-,F] → [2,1,3,2].
    assert_eq!(run_single_plot(src), "[2.0,1.0,3.0,2.0]");
}

/// A value branch may run leading statements before its trailing value.
#[test]
fn if_expression_multi_statement_branch() {
    let src = "//@version=6\nindicator(\"x\")\np = if close > open\n    a = close * 2\n    a + 1\nelse\n    0.0\nplot(p)\n";
    // close*2+1 on up bars, else 0: [21,25,0,22.5].
    assert_eq!(run_single_plot(src), "[21.0,25.0,0.0,22.5]");
}

/// Mixed int/float branches unify to float.
#[test]
fn if_expression_mixed_int_float_branches_widen() {
    let src =
        "//@version=6\nindicator(\"x\")\np = if close > open\n    1\nelse\n    0.5\nplot(p)\n";
    assert_eq!(run_single_plot(src), "[1.0,1.0,0.5,1.0]");
}

/// `switch` with a subject compares each label to the once-evaluated subject
/// and falls through to `default` when no label matches.
#[test]
fn switch_expression_with_subject_and_default() {
    let src = "//@version=6\nindicator(\"x\")\np = switch bar_index\n    0 => 100.0\n    1 => 200.0\n    => 999.0\nplot(p)\n";
    assert_eq!(run_single_plot(src), "[100.0,200.0,999.0,999.0]");
}

/// A subject switch with no matching arm and no default yields `na`.
#[test]
fn switch_expression_no_match_is_na() {
    let src = "//@version=6\nindicator(\"x\")\np = switch bar_index\n    0 => 100.0\n    1 => 200.0\nplot(p)\n";
    assert_eq!(run_single_plot(src), "[100.0,200.0,null,null]");
}

/// A subjectless switch matches the first true boolean arm.
#[test]
fn subjectless_switch_expression() {
    let src = "//@version=6\nindicator(\"x\")\np = switch\n    close > open => 1.0\n    close < open => -1.0\n    => 0.0\nplot(p)\n";
    // close>open [T,T,F,T]; bar 2 close<open → -1.
    assert_eq!(run_single_plot(src), "[1.0,1.0,-1.0,1.0]");
}

/// The switch subject is evaluated exactly once: a stateful `ta.change`
/// subject advances its state one bar at a time. Evaluating it twice would
/// corrupt the reading; the single hidden-slot store guarantees one eval.
#[test]
fn switch_subject_evaluated_once() {
    // subject = ta.change(close) = close - close[1]: [na, 2, -1, -0.25].
    // Compare to 2.0 → bar 1 matches (10.0), else 20.0. If the subject were
    // evaluated per-comparison the ta state would desync; here it is stable.
    let src = "//@version=6\nindicator(\"x\")\nc = ta.change(close)\np = switch c\n    2.0 => 10.0\n    => 20.0\nplot(p)\n";
    assert_eq!(run_single_plot(src), "[20.0,10.0,20.0,20.0]");
}

/// `switch` as a statement: arm bodies run for effect (no value produced).
#[test]
fn switch_statement_runs_arm_effects() {
    let src = "//@version=6\nindicator(\"x\")\nvar float p = 0.0\nswitch bar_index\n    0 => p := 5.0\n    => p := 9.0\nplot(p)\n";
    assert_eq!(run_single_plot(src), "[5.0,9.0,9.0,9.0]");
}

/// `if` used as a value as the tail of a compound RHS expression: the
/// desugared control flow runs before the assignment and the read composes.
#[test]
fn if_expression_in_compound_rhs() {
    let src = "//@version=6\nindicator(\"x\")\nbase = 100.0\np = base + if close > open\n    1.0\nelse\n    2.0\nplot(p)\n";
    // base + (close>open ? 1 : 2): [101,101,102,101].
    assert_eq!(run_single_plot(src), "[101.0,101.0,102.0,101.0]");
}

/// End-to-end test for strings: concatenation, `str.tostring` with
/// a format, substring/upper, `replace_all`, contains, pos, and tonumber all
/// compose into one plotted numeric result.
#[test]
fn golden_strings() {
    let source = "//@version=6\n\
        indicator(\"S\")\n\
        label = \"close=\" + str.tostring(close, \"#.##\")\n\
        tag = str.upper(str.substring(label, 0, 5))\n\
        found = str.contains(label, \"=\")\n\
        digits = str.length(str.replace_all(label, \".\", \"\"))\n\
        plot(found ? digits + str.pos(tag, \"S\") : 0)\n";
    let close = vec![10.0, 12.5];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 2],
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
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    let values = plot_floats(&result.plots[0]);
    // bar 0: label "close=10" → replace_all removes no dots → len 8; tag
    //        upper(substring(_,0,5)) = "CLOSE"; pos("CLOSE","S") = 3 → 8+3.
    // bar 1: label "close=12.5" → remove "." → "close=125" len 9; 9+3.
    approx(values[0], 11.0);
    approx(values[1], 12.0);
}

#[test]
fn max_bars_back_function_call_compiles_runs_and_is_a_noop() {
    // A top-level `max_bars_back(source, N)` call must compile and run through
    // the whole pipeline (it lowers to nothing), leaving output identical to
    // the same script without the call.
    let with_call =
        "//@version=6\nindicator(\"Ex\")\nmax_bars_back(close, 1000)\nx = close - close[1]\nplot(x)\n";
    let without = "//@version=6\nindicator(\"Ex\")\nx = close - close[1]\nplot(x)\n";
    let close = vec![10.0, 12.0, 11.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 3],
        ..MarketData::default()
    };
    let a = compile_and_run(
        with_call,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    let b = compile_and_run(
        without,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        a.diagnostics.iter().all(|d| !d.is_error()),
        "diagnostics: {:?}",
        a.diagnostics
    );
    assert!(a.runtime_errors.is_empty(), "{:?}", a.runtime_errors);
    assert_eq!(plot_floats(&a.plots[0]), plot_floats(&b.plots[0]));
}

#[test]
fn history_offset_beyond_max_bars_back_blocks_compilation_e0233() {
    // 5000 bars is the most a history buffer holds: no declaration reaches a
    // deeper literal offset.
    for source in [
        "//@version=6\nindicator(\"Ex\")\nplot(close[5001])\n",
        "//@version=6\nindicator(\"Ex\", max_bars_back=5000)\nplot(close[5001])\n",
    ] {
        let result = compile(source, CompileOptions::default());
        assert!(!result.success);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code.as_deref() == Some("E0233")),
            "{:?}",
            result.diagnostics
        );
    }
}

/// `bars` bars whose close is the bar index, so a history read is checked
/// against the index it should have reached.
fn counting_bars(bars: usize) -> MarketData {
    let close: Vec<f64> = (0..bars).map(|bar| bar as f64).collect();
    MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; bars],
        ..MarketData::default()
    }
}

#[test]
fn literal_history_offset_beyond_the_default_buffer_needs_no_declaration() {
    // A literal offset states how much history it needs, so the script reads
    // 501 bars back with nothing declared, on a built-in series and on a
    // user variable alike. The first 501 bars have no such history yet.
    const OFFSET: usize = 501;
    const BARS: usize = 520;
    let source = "//@version=6\nindicator(\"Ex\")\n\
                  doubled = close * 2\n\
                  plot(close[501])\n\
                  plot(doubled[501])\n";
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert_eq!(compiled.program.unwrap().metadata.max_bars_back, 501);

    let run = compile_and_run(
        source,
        &counting_bars(BARS),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed as usize, BARS);
    let builtin = plot_floats(&run.plots[0]);
    let variable = plot_floats(&run.plots[1]);
    for bar in 0..BARS {
        let expected = bar.checked_sub(OFFSET).map(|back| back as f64);
        assert_eq!(builtin[bar], expected, "close[501] on bar {bar}");
        assert_eq!(
            variable[bar],
            expected.map(|value| value * 2.0),
            "doubled[501] on bar {bar}"
        );
    }
}

#[test]
fn literal_history_offset_is_the_same_program_as_the_declaration_it_replaces() {
    // Reading 501 bars back is one request for a 501-bar buffer, however the
    // script makes it: the literal alone compiles to what the literal plus
    // `max_bars_back=501` compiles to.
    let implied = "//@version=6\nindicator(\"Ex\")\nx = close * 2\nplot(x[501])\n";
    let declared =
        "//@version=6\nindicator(\"Ex\", max_bars_back=501)\nx = close * 2\nplot(x[501])\n";
    let implied = compile(implied, CompileOptions::default());
    let declared = compile(declared, CompileOptions::default());
    assert!(implied.success, "{:?}", implied.diagnostics);
    assert!(declared.success, "{:?}", declared.diagnostics);
    let (implied, declared) = (implied.program.unwrap(), declared.program.unwrap());
    assert_eq!(implied.metadata.max_bars_back, 501);
    assert_eq!(
        implied.metadata.max_bars_back,
        declared.metadata.max_bars_back
    );
    assert_eq!(implied.history_depth(), declared.history_depth());
    // The same instructions; only the source offsets they map to differ.
    let code = |program: &pinewasm_compiler::Program| -> Vec<_> {
        program
            .functions
            .iter()
            .map(|function| function.code.clone())
            .collect()
    };
    assert_eq!(code(&implied), code(&declared));
}

#[test]
fn literal_history_offset_raises_the_ceiling_for_dynamic_offsets() {
    // The ceiling a run-time offset is checked against is the largest request
    // the script makes. With nothing but the default (500) an offset of 600
    // stops the run; a literal `[600]` elsewhere in the script asks for that
    // much history, so the same dynamic read is then within the buffer.
    let dynamic = "lag = input.int(600)\nx = close * 2\nplot(x[lag])\n";
    let without = format!("//@version=6\nindicator(\"Ex\")\n{dynamic}");
    let with = format!("//@version=6\nindicator(\"Ex\")\n{dynamic}plot(close[600])\n");
    let bars = counting_bars(620);

    let stopped = compile_and_run(
        &without,
        &bars,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert_eq!(
        stopped.runtime_errors.len(),
        1,
        "{:?}",
        stopped.runtime_errors
    );
    assert_eq!(
        stopped.runtime_errors[0].kind,
        pinewasm_compiler::RuntimeErrorKind::HistoryBufferLimitExceeded
    );

    let run = compile_and_run(
        &with,
        &bars,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        run.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        run.diagnostics
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    let dynamic = plot_floats(&run.plots[0]);
    assert_eq!(dynamic[599], None);
    assert_eq!(dynamic[600], Some(0.0));
    assert_eq!(dynamic[619], Some(38.0));
}

/// Strip the trailing `; start..end` span comments from a CSIL dump so two
/// programs that differ only in source byte offsets compare equal on their
/// opcodes/operands.
fn csil_without_spans(csil: &str) -> String {
    csil.lines()
        .map(|line| match line.split_once(';') {
            Some((code, _)) => code.trim_end(),
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- collection syntax sugar lowers identically to the base form ----

#[test]
fn method_call_lowers_to_identical_csil_as_namespace_form() {
    // `a.push(x)` ≡ `array.push(a, x)`: the method form and the namespace form
    // must lower to byte-identical CSIL.
    let method = "//@version=6\nindicator(\"M\")\n\
                  a = array.new_float(0)\n\
                  a.push(1.0)\n\
                  plot(a.get(0) + a.size())\n";
    let namespace = "//@version=6\nindicator(\"M\")\n\
                     a = array.new_float(0)\n\
                     array.push(a, 1.0)\n\
                     plot(array.get(a, 0) + array.size(a))\n";
    let m = compile(method, CompileOptions::all());
    let n = compile(namespace, CompileOptions::all());
    assert!(m.success, "{:?}", m.diagnostics);
    assert!(n.success, "{:?}", n.diagnostics);
    assert_eq!(
        csil_without_spans(m.csil_text.as_deref().unwrap()),
        csil_without_spans(n.csil_text.as_deref().unwrap())
    );
}

#[test]
fn generic_constructor_lowers_to_identical_csil_as_typed_form() {
    // `array.new<float>(...)` ≡ `array.new_float(...)`; `map.new<string, int>()`
    // ≡ `map.new_string_int()`.
    let generic = "//@version=6\nindicator(\"G\")\n\
                   a = array.new<float>(1, 2.0)\n\
                   m = map.new<string, int>()\n\
                   dummy = map.put(m, \"k\", 3)\n\
                   plot(array.get(a, 0) + map.get(m, \"k\"))\n";
    let typed = "//@version=6\nindicator(\"G\")\n\
                 a = array.new_float(1, 2.0)\n\
                 m = map.new_string_int()\n\
                 dummy = map.put(m, \"k\", 3)\n\
                 plot(array.get(a, 0) + map.get(m, \"k\"))\n";
    let g = compile(generic, CompileOptions::all());
    let t = compile(typed, CompileOptions::all());
    assert!(g.success, "{:?}", g.diagnostics);
    assert!(t.success, "{:?}", t.diagnostics);
    assert_eq!(
        csil_without_spans(g.csil_text.as_deref().unwrap()),
        csil_without_spans(t.csil_text.as_deref().unwrap())
    );
}

/// End-to-end test for two drawing-style constant namespaces
/// (`position.*`/`alert.freq_*`): the table-positioning and alert-frequency
/// tokens round-trip as distinct const strings through checker → CSIL → VM,
/// like the other drawing-style constants. Uses the `table_alerts.pine`
/// fixture's expressions inline so the plotted numbers are value-asserted.
#[test]
fn golden_position_and_alert_freq_constants() {
    let source = "//@version=6\n\
        indicator(\"Table Alerts\")\n\
        pos = position.middle_center\n\
        freq = alert.freq_once_per_bar_close\n\
        plot(str.length(pos) + str.length(freq))\n\
        plot(pos == \"position.middle_center\" ? 1.0 : 0.0)\n\
        plot(pos == \"position.top_left\" ? 1.0 : 0.0)\n\
        plot(freq == \"alert.freq_once_per_bar_close\" ? 1.0 : 0.0)\n\
        plot(pos == \"middle_center\" ? 1.0 : 0.0)\n";
    let close = vec![10.0, 12.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 2],
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
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    // The stored position value is "middle_center", so len = 13. The alert
    // value keeps its full length, len("alert.freq_once_per_bar_close") = 29,
    // yielding 42.
    for bar in 0..2 {
        approx(plot_floats(&result.plots[0])[bar], 42.0);
        approx(plot_floats(&result.plots[1])[bar], 0.0); // not spelled "position.middle_center"
        approx(plot_floats(&result.plots[2])[bar], 0.0); // pos does not equal position.top_left
        approx(plot_floats(&result.plots[3])[bar], 1.0); // freq equals its full constant name
        approx(plot_floats(&result.plots[4])[bar], 1.0); // position value equals "middle_center"
    }
}

/// End-to-end test for the event streams: `log.*` and
/// `alert()`/`alertcondition()` surface through the compiler `RunResult` into
/// the `logs` and `alerts` sections with formatted messages and correct
/// per-bar firing.
#[test]
fn golden_log_and_alert_event_streams() {
    let source = "//@version=6\n\
        indicator(\"Events\")\n\
        log.info(\"close={0}\", close)\n\
        log.error(\"tick {0}\", bar_index)\n\
        alert(\"beat\")\n\
        alert(\"dup\")\n\
        alertcondition(close > 10.0, \"cross\", \"above ten\")\n\
        plot(close)\n";
    let close = vec![8.0, 15.0];
    let data = MarketData {
        open: close.clone(),
        high: close.clone(),
        low: close.clone(),
        close,
        volume: vec![0.0; 2],
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
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    // Two log calls per bar over two bars = 4 entries, formatted.
    assert_eq!(result.logs.len(), 4);
    assert_eq!(result.logs[0].message, "close=8");
    assert_eq!(result.logs[1].message, "tick 0");
    assert_eq!(result.logs[2].message, "close=15");
    assert_eq!(result.logs[3].message, "tick 1");
    // once-per-bar dedup is PER CALL SITE: the two distinct alert() calls each
    // fire every bar (beat, dup) × 2 bars = 4; alertcondition fires only on bar
    // 1 (close 15 > 10). = 5 total.
    let alerts: Vec<(u32, &str)> = result
        .alerts
        .iter()
        .map(|a| (a.bar, a.message.as_str()))
        .collect();
    assert_eq!(
        alerts,
        vec![
            (0, "beat"),
            (0, "dup"),
            (1, "beat"),
            (1, "dup"),
            (1, "above ten"),
        ]
    );
    assert_eq!(result.alerts[4].freq, "alert.freq_once_per_bar_close");
}

/// Recursively assert every JSON number fits in a JS-safe integer range
/// (`|n| <= 2^53 - 1`, `Number.MAX_SAFE_INTEGER`). This is exactly the
/// constraint `serde_wasm_bindgen`'s `json_compatible` serializer enforces at
/// the wasm boundary: an i64 outside it aborts the whole run with
/// `"… can't be represented as a JavaScript number"`.
fn assert_js_safe(value: &serde_json::Value, path: &str) {
    // `Number.MAX_SAFE_INTEGER` is 2^53 - 1, not 2^53.
    const MAX_SAFE: i64 = (1 << 53) - 1;
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                assert!(i.abs() <= MAX_SAFE, "JS-unsafe integer at {path}: {i}");
            } else if let Some(u) = n.as_u64() {
                assert!(u <= MAX_SAFE as u64, "JS-unsafe integer at {path}: {u}");
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                assert_js_safe(item, &format!("{path}[{i}]"));
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                assert_js_safe(v, &format!("{path}.{k}"));
            }
        }
        _ => {}
    }
}

/// Regression: `ta.bb` (and any engine function with an `f64`-bit-pattern const
/// param) must produce a wholly JS-safe `RunResult`. The mult `2.0` is baked as
/// `2.0f64.to_bits()` = `2^62`, far above `Number.MAX_SAFE_INTEGER`; without
/// that handling it serializes as a bare number inside
/// `program.ta_states[].params`, which aborts the wasm boundary with
/// `"4611686018427387904 can't be represented as a JavaScript number"`. Also pins the tuple-`na` warm-up: the first 19 bars of
/// a length-20 `ta.bb` are `na` (→ JSON `null`) on all three outputs.
#[test]
fn golden_bollinger_tuple_na_is_js_safe() {
    let source = "//@version=6\n\
        indicator(\"Bollinger Bands\", overlay=true)\n\
        [middle, upper, lower] = ta.bb(close, 20, 2.0)\n\
        plot(middle, \"Basis\")\n\
        plot(upper, \"Upper\")\n\
        plot(lower, \"Lower\")\n";
    // 25 rising bars: the first 19 are na, the rest warm.
    let close: Vec<f64> = (0..25).map(|i| 100.0 + f64::from(i)).collect();
    let data = MarketData {
        open: close.clone(),
        high: close.iter().map(|c| c + 1.0).collect(),
        low: close.iter().map(|c| c - 1.0).collect(),
        close: close.clone(),
        volume: vec![1000.0; close.len()],
        ..MarketData::default()
    };
    let result = compile_and_run(source, &data, CompileOptions::all(), &RunOptions::default());
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.plots.len(), 3, "basis/upper/lower");

    // Warm-up: the first 19 bars (length - 1) are na on every tuple output.
    for plot in &result.plots {
        for (bar, value) in plot.values.iter().take(19).enumerate() {
            assert!(
                matches!(value, Value::Na),
                "plot {:?} bar {bar} should be na, got {value:?}",
                plot.title
            );
        }
        assert!(
            !matches!(plot.values[19], Value::Na),
            "plot {:?} bar 19 should be warm",
            plot.title
        );
    }

    // The whole wire payload — including the embedded compiled program, where
    // the mult bit pattern lives — must be representable as JS numbers.
    let wire = serde_json::to_value(&result).expect("serialize");
    assert_js_safe(&wire, "run");

    // And the tuple `na` cells are JSON `null`, not a sentinel number.
    let basis = serde_json::to_string(&result.plots[0].values).unwrap();
    assert!(basis.starts_with("[null,null,"), "basis: {basis}");
    assert!(!basis.contains("4611686018427387904"), "basis: {basis}");
}

/// The opt-in in-process translation validation of the CSIL pair-erasure
/// passes accepts every compile-pass fixture and changes nothing, and it is
/// not part of the JSON option surface.
#[test]
fn validated_passes_compile_every_fixture_identically() {
    let validated = CompileOptions {
        validate_passes: true,
        ..CompileOptions::all()
    };
    for (name, source) in read_fixtures("compile-pass") {
        let plain = compile(&source, CompileOptions::all());
        let checked = compile(&source, validated);
        assert!(checked.success, "{name}: {:?}", checked.diagnostics);
        assert_eq!(checked.program, plain.program, "{name}");
        assert_eq!(checked.diagnostics, plain.diagnostics, "{name}");
    }
    assert!(!CompileOptions::default().validate_passes);
    let json = serde_json::to_string(&validated).unwrap();
    assert!(!json.contains("validate"), "{json}");
    let parsed: CompileOptions = serde_json::from_str(r#"{"validatePasses":true}"#).unwrap();
    assert!(!parsed.validate_passes, "not settable over the wire");
}
