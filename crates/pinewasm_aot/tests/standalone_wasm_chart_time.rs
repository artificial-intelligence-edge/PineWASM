//! The chart timeframe and fixed-timezone calendars in standalone WebAssembly.
//!
//! A standalone module has no host to ask for the chart timeframe, so it
//! takes the timeframe from the request's `timeframe` field and answers
//! `timeframe.in_seconds()`, `timeframe.multiplier`, the `timeframe.is*`
//! predicates, `time_close` and `timenow` itself, and computes the calendar
//! functions of a time in a timezone a string literal names. Every test here
//! runs the `pinewasm_vm` interpreter over the same bars and timeframe and
//! compares bar for bar:
//! the plot lanes, the trades and the net profit.

use pinewasm_aot::{
    compile_standalone_wasm_program_with_config, AotError, StandaloneWasmBuildConfig,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions, RuntimeErrorKind};

fn compile_csil(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::all());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "sample must compile: {:?}",
        result.diagnostics
    );
    result.program.expect("program")
}

/// The build the JavaScript binding makes: a strategy module, signal slice on.
fn build(source: &str) -> Result<Vec<u8>, AotError> {
    build_as(source, StandaloneWasmModuleType::Strategy)
}

/// An indicator module links the plain standalone runtime, a strategy
/// module the broker's.
fn build_as(source: &str, module_type: StandaloneWasmModuleType) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program_with_config(
        &compile_csil(source),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type,
        },
        &StandaloneWasmBuildConfig {
            signal_slice: true,
            ..StandaloneWasmBuildConfig::default()
        },
    )
}

fn custom_section(wasm: &[u8], name: &str) -> Option<serde_json::Value> {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .find_map(|payload| match payload {
            Ok(wasmparser::Payload::CustomSection(reader)) if reader.name() == name => {
                Some(serde_json::from_slice(reader.data()).expect("JSON section"))
            }
            _ => None,
        })
}

/// One instance of a module, kept across evaluations as a host keeps it for
/// resident datasets.
struct Instance {
    store: wasmi::Store<()>,
    exports: wasmi::Instance,
    memory: wasmi::Memory,
}

impl Instance {
    fn new(wasm: &[u8]) -> Self {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, wasm).expect("wasmi accepts the module");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("instantiate with an empty linker");
        let memory = instance.get_memory(&store, "memory").expect("memory");
        Self {
            store,
            exports: instance,
            memory,
        }
    }

    /// The output document, or the error.
    fn evaluate(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        let ptr = self
            .exports
            .get_typed_func::<i32, i32>(&self.store, "alloc")
            .expect("alloc")
            .call(&mut self.store, input.len() as i32)
            .expect("alloc call") as usize;
        self.memory.data_mut(&mut self.store)[ptr..ptr + input.len()].copy_from_slice(input);
        let packed = self
            .exports
            .get_typed_func::<(i32, i32), i64>(&self.store, "evaluate_indicator_cabi")
            .expect("evaluate")
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("evaluate call") as u64;
        if packed == 0 {
            let error = self
                .exports
                .get_typed_func::<(), i64>(&self.store, "last_error")
                .expect("last_error")
                .call(&mut self.store, ())
                .expect("last_error call") as u64;
            return Err(String::from_utf8(self.read(error)).expect("UTF-8 error"));
        }
        Ok(self.read(packed))
    }

    fn read(&self, packed: u64) -> Vec<u8> {
        let (at, len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        self.memory.data(&self.store)[at..at + len].to_vec()
    }
}

/// One evaluation on a fresh instance: the output document, or the error.
fn evaluate(wasm: &[u8], input: &[u8]) -> Result<Vec<u8>, String> {
    Instance::new(wasm).evaluate(input)
}

struct Bar {
    time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
}

/// `count` bars `step` milliseconds apart from `start`, with a deterministic
/// walk that trends both ways.
fn walk(count: usize, start: i64, step: i64) -> Vec<Bar> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut price = 1000.0_f64;
    (0..count)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let drift = [0.004, -0.003, 0.002][(i / 150) % 3];
            let noise = ((state % 1001) as f64 - 500.0) / 50_000.0;
            let open = price;
            price *= 1.0 + drift + noise;
            Bar {
                time: start + i as i64 * step,
                open,
                high: open.max(price) * 1.002,
                low: open.min(price) * 0.998,
                close: price,
            }
        })
        .collect()
}

fn market_data(bars: &[Bar], timeframe: &str) -> MarketData {
    let mut data = MarketData::default();
    for bar in bars {
        data.open.push(bar.open);
        data.high.push(bar.high);
        data.low.push(bar.low);
        data.close.push(bar.close);
        data.volume.push(100.0);
        data.time.push(bar.time);
    }
    data.timeframe = timeframe.to_string();
    data.syminfo.mincontract = 0.001;
    data
}

/// The JSON request over `bars`, with `fields` (a `"key":value,` run) first,
/// on the interpreter's default exchange (UTC, no session) unless `fields`
/// names one.
fn request(bars: &[Bar], fields: &str) -> String {
    let mut exchange = String::new();
    if !fields.contains("\"timezone\"") {
        exchange.push_str("\"timezone\":\"UTC\",");
    }
    if !fields.contains("\"session_hours\"") {
        exchange.push_str("\"session_hours\":\"\",");
    }
    request_raw(bars, &format!("{exchange}{fields}"))
}

/// [`request`] with exactly `fields`, so a test can leave one out.
fn request_raw(bars: &[Bar], fields: &str) -> String {
    let mut out = format!("{{\"version\":1,\"params\":{{}},{fields}\"bars\":[");
    for (i, bar) in bars.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"time\":{},\"open\":{:?},\"high\":{:?},\"low\":{:?},\"close\":{:?},\"volume\":100.0}}",
            bar.time, bar.open, bar.high, bar.low, bar.close
        ));
    }
    out.push_str("]}");
    out
}

/// A `PWSWBIN1` packet: the JSON metadata, then the six lanes.
fn binary_packet(bars: &[Bar], metadata: &[u8]) -> Vec<u8> {
    let start = (16 + metadata.len() + 7) & !7;
    let mut packet = vec![0u8; start];
    packet[..8].copy_from_slice(b"PWSWBIN1");
    packet[8..12].copy_from_slice(&(bars.len() as u32).to_le_bytes());
    packet[12..16].copy_from_slice(&(metadata.len() as u32).to_le_bytes());
    packet[16..16 + metadata.len()].copy_from_slice(metadata);
    push_lanes(&mut packet, bars);
    packet
}

/// A `PWSWBIN2` upload of dataset `id`, or a `PWSWREF1` reference to it.
fn resident_packet(bars: &[Bar], metadata: &[u8], id: u64, reference: bool) -> Vec<u8> {
    let start = (24 + metadata.len() + 7) & !7;
    let mut packet = vec![0u8; start];
    packet[..8].copy_from_slice(if reference { b"PWSWREF1" } else { b"PWSWBIN2" });
    packet[8..12].copy_from_slice(&(bars.len() as u32).to_le_bytes());
    packet[12..16].copy_from_slice(&(metadata.len() as u32).to_le_bytes());
    packet[16..24].copy_from_slice(&id.to_le_bytes());
    packet[24..24 + metadata.len()].copy_from_slice(metadata);
    if !reference {
        push_lanes(&mut packet, bars);
    }
    packet
}

fn push_lanes(packet: &mut Vec<u8>, bars: &[Bar]) {
    for lane in 0..6 {
        for bar in bars {
            let value = [
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                100.0,
                bar.time as f64,
            ][lane];
            packet.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// A plot cell as the module serializes it: a number, or `null` for `na`.
fn cell(value: &Value) -> Option<f64> {
    match value {
        Value::Float(f) if f.is_finite() => Some(*f),
        Value::Int(i) => Some(*i as f64),
        _ => None,
    }
}

/// Run `source` in the interpreter and in its standalone module over the
/// same bars and timeframe, and require the same plot lanes and, from a
/// strategy module, the same trades and net profit. Returns the number of
/// closed trades.
fn assert_matches_interpreter(source: &str, wasm: &[u8], bars: &[Bar], timeframe: &str) -> usize {
    assert_matches_interpreter_on(source, wasm, bars, timeframe, "UTC", "")
}

/// [`assert_matches_interpreter`] on an exchange with `timezone` and
/// declared session `session`.
fn assert_matches_interpreter_on(
    source: &str,
    wasm: &[u8],
    bars: &[Bar],
    timeframe: &str,
    timezone: &str,
    session: &str,
) -> usize {
    let mut data = market_data(bars, timeframe);
    data.syminfo.timezone = timezone.to_string();
    data.syminfo.session_hours = session.to_string();
    let expected = run(&compile_csil(source), &data, &RunOptions::default());
    assert!(
        expected.errors.is_empty(),
        "{timeframe:?} {timezone:?} {session:?}: {:?}",
        expected.errors
    );
    let input = request(
        bars,
        &format!(
            "\"timeframe\":{},\"timezone\":{},\"session_hours\":{},\"include_results\":true,\"mincontract\":0.001,",
            serde_json::Value::from(timeframe),
            serde_json::Value::from(timezone),
            serde_json::Value::from(session),
        ),
    );
    let out = evaluate(wasm, input.as_bytes()).unwrap_or_else(|e| panic!("{timeframe:?}: {e}"));
    let actual: serde_json::Value = serde_json::from_slice(&out).expect("output JSON");
    let plots = actual["plots"].as_array().expect("plots");
    assert_eq!(
        plots.len(),
        expected.plots.len(),
        "{timeframe:?}: plot count"
    );
    for (lane, series) in plots.iter().zip(&expected.plots) {
        let lane = lane.as_array().expect("plot lane");
        assert_eq!(lane.len(), series.values.len());
        for (bar, (actual, expected)) in lane.iter().zip(&series.values).enumerate() {
            assert_eq!(
                actual.as_f64(),
                cell(expected),
                "{timeframe:?}: plot {:?} at bar {bar}",
                series.title
            );
        }
    }
    let Some(trades) = actual["strategy"]["trades"].as_array() else {
        // An indicator module reports plots only.
        return 0;
    };
    let strategy = expected.strategy.expect("strategy outcome");
    assert_eq!(trades.len(), strategy.trades.len(), "{timeframe:?}: trades");
    for (row, trade) in trades.iter().zip(&strategy.trades) {
        assert_eq!(row["entryBar"].as_f64(), Some(f64::from(trade.entry_bar)));
        assert_eq!(row["exitBar"].as_f64(), Some(f64::from(trade.exit_bar)));
        assert_eq!(row["entryPrice"].as_f64(), Some(trade.entry_price));
        assert_eq!(row["exitPrice"].as_f64(), Some(trade.exit_price));
        assert_eq!(row["qty"].as_f64(), Some(trade.qty));
        assert_eq!(row["pnl"].as_f64(), Some(trade.pnl));
    }
    assert_eq!(
        actual["strategy"]["netProfit"].as_f64(),
        Some(strategy.net_profit),
        "{timeframe:?}: net profit"
    );
    strategy.trades.len()
}

const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;
/// 2024-01-01T00:00:00Z.
const JAN_2024: i64 = 1_704_067_200_000;

/// Every chart-timeframe read the module answers, plotted, and orders that
/// change when any of them is wrong.
const CHART_TIMEFRAME: &str = concat!(
    "strategy(\"Chart timeframe\", initial_capital = 100000)\n",
    "plot(timeframe.in_seconds(), \"seconds\")\n",
    "plot(timeframe.in_seconds(timeframe.period), \"seconds, written\")\n",
    "plot(timeframe.multiplier, \"multiplier\")\n",
    "plot(timeframe.multiplier[1], \"multiplier[1]\")\n",
    "plot(timeframe.multiplier[bar_index % 3], \"multiplier[dynamic]\")\n",
    "plot(timeframe.isseconds ? 1 : 0, \"isseconds\")\n",
    "plot(timeframe.isminutes ? 1 : 0, \"isminutes\")\n",
    "plot(timeframe.isdaily ? 1 : 0, \"isdaily\")\n",
    "plot(timeframe.isweekly ? 1 : 0, \"isweekly\")\n",
    "plot(timeframe.ismonthly ? 1 : 0, \"ismonthly\")\n",
    "plot(timeframe.isdwm ? 1 : 0, \"isdwm\")\n",
    "plot(timeframe.isintraday ? 1 : 0, \"isintraday\")\n",
    "plot(timeframe.isticks ? 1 : 0, \"isticks\")\n",
    "plot(time_close, \"time_close\")\n",
    "plot(time_close[2], \"time_close[2]\")\n",
    "plot(time_close[bar_index % 4], \"time_close[dynamic]\")\n",
    "plot(timenow, \"timenow\")\n",
    "float perDay = 86400.0 / timeframe.in_seconds()\n",
    "int every = math.max(1, math.round(math.min(perDay, 24.0)))\n",
    "if bar_index % every == 0 and close > open and timeframe.multiplier < 1000\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if (timeframe.isintraday ? time_close - time > 2 * 3600000 : bar_index % 7 == 0) and close < open\n",
    "    strategy.close(\"L\")\n",
);

#[test]
fn chart_timeframe_reads_match_the_interpreter() {
    let wasm = build(CHART_TIMEFRAME).expect("the chart timeframe is a request field");
    let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
    assert_eq!(
        capabilities["required_fields"],
        serde_json::json!(["timeframe"])
    );
    let bars = walk(400, JAN_2024, HOUR);
    let mut traded = 0;
    // Every unit, a multiplier above one, the empty and padded forms, case,
    // an unknown unit, signs, an unparsable numeral and a numeral too large
    // for `i64`: the interpreter's parse decides each, and the module must
    // agree with it.
    for timeframe in [
        "1",
        "5",
        "60",
        "240",
        "1S",
        "30S",
        "1D",
        "3D",
        "1W",
        "1M",
        "12M",
        "",
        " 60 ",
        "1d",
        "1h",
        "5X",
        "+15",
        "-15",
        "0",
        "D",
        "9223372036854775808",
    ] {
        traded += assert_matches_interpreter(CHART_TIMEFRAME, &wasm, &bars, timeframe);
    }
    assert!(traded > 100, "the fixture must trade: {traded}");
}

/// The calendar functions of `time` in timezone `tz`, as written in a script.
fn calendar_source(tz: &str) -> String {
    format!(
        concat!(
            "strategy(\"Calendar\", initial_capital = 100000)\n",
            "plot(year(time, \"{tz}\"), \"year\")\n",
            "plot(month(time, \"{tz}\"), \"month\")\n",
            "plot(dayofmonth(time, \"{tz}\"), \"dayofmonth\")\n",
            "plot(dayofweek(time, \"{tz}\"), \"dayofweek\")\n",
            "plot(hour(time, \"{tz}\"), \"hour\")\n",
            "plot(minute(time, \"{tz}\"), \"minute\")\n",
            "plot(second(time, \"{tz}\"), \"second\")\n",
            "plot(weekofyear(time, \"{tz}\"), \"weekofyear\")\n",
            "plot(dayofmonth(time[1], \"{tz}\"), \"dayofmonth of the bar before\")\n",
            "if dayofweek(time, \"{tz}\") == dayofweek.monday or hour(time, \"{tz}\") == 23\n",
            "    strategy.entry(\"L\", strategy.long)\n",
            "if weekofyear(time, \"{tz}\") % 2 == 0\n",
            "    strategy.close(\"L\")\n",
        ),
        tz = tz
    )
}

/// Bar times across the calendar's edges: before and after the epoch
/// (negative milliseconds round toward the earlier second), leap days, ISO
/// weeks that belong to the neighbouring year, and the far past and future
/// a time lane holds exactly (below 2^53 ms), then an hourly walk.
fn calendar_bars() -> Vec<Bar> {
    let mut times = vec![
        -8_900_000_000_000_000,
        -62_135_596_800_000, // 0001-01-01
        -315_619_200_000,    // 1960-01-01
        -1,                  // 1969-12-31T23:59:59.999
        0,
        951_825_600_000,   // 2000-02-29T12:00
        1_609_372_800_000, // 2020-12-31 (ISO week 53)
        1_609_459_200_000, // 2021-01-01 (ISO week 53 of 2020)
        1_609_718_400_000, // 2021-01-04 (ISO week 1)
        1_709_251_199_500, // 2024-02-29T23:59:59.5
        1_735_516_800_000, // 2024-12-30 (ISO week 1 of 2025)
        4_107_542_400_000, // 2100-03-01
    ];
    let walk_start = 4_200_000_000_000;
    times.extend((0..300).map(|i| walk_start + i * 7 * HOUR / 3));
    times.push(8_900_000_000_000_000);
    let prices = walk(times.len(), 0, 1);
    times
        .into_iter()
        .zip(prices)
        .map(|(time, bar)| Bar { time, ..bar })
        .collect()
}

#[test]
fn calendar_calls_in_a_fixed_timezone_match_the_interpreter() {
    let bars = calendar_bars();
    let mut traded = 0;
    // The fixed-offset forms `pinewasm_tz::parse_fixed` reads, at its range
    // limits.
    for tz in [
        "UTC",
        "GMT",
        "UTC+3",
        "GMT-5",
        "UTC+05:30",
        "UTC-0930",
        "UTC+14:59",
        "UTC-14:59",
        " UTC+1 ",
    ] {
        let source = calendar_source(tz);
        let wasm = build(&source).unwrap_or_else(|e| panic!("{tz:?}: {e}"));
        // No timeframe read, so the request needs no `timeframe`.
        let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
        assert!(capabilities.get("required_fields").is_none(), "{tz:?}");
        traded += assert_matches_interpreter(&source, &wasm, &bars, "1D");
    }
    assert!(traded > 20, "the fixture must trade: {traded}");
    // Anything else keeps `host.vm`: an IANA zone, whose clocks change and
    // whose rules the module does not carry, and a name the interpreter
    // stops on. Neither is read as UTC.
    for tz in ["America/New_York", "Asia/Tokyo", "UTC+15", "UTC+5:60", ""] {
        let error = build(&calendar_source(tz)).expect_err(tz).to_string();
        assert!(
            error.starts_with("strategy feature unsupported for standalone WebAssembly:"),
            "{tz:?}: {error}"
        );
    }
}

#[test]
fn a_calendar_call_without_a_literal_timezone_still_needs_a_host() {
    // Without a timezone argument a calendar call reads the exchange
    // timezone, which the request does not carry; a timezone chosen at run
    // time is a string the module does not have either.
    for (call, feature) in [
        ("dayofmonth(time)", "dayofmonth"),
        ("dayofmonth", "dayofmonth"),
        ("weekofyear(time)", "weekofyear"),
        (
            "dayofmonth(time, close > open ? \"UTC\" : \"UTC+3\")",
            "dayofmonth",
        ),
    ] {
        let source = format!(
            "strategy(\"Exchange\")\nif {call} == 1\n    strategy.entry(\"L\", strategy.long)\n"
        );
        match build(&source) {
            Err(AotError::StrategyFeatureUnsupported { feature: named }) => {
                assert_eq!(named, feature, "{call}");
            }
            other => panic!("{call}: {other:?}"),
        }
    }
}

/// Served reads in the forms a script writes them: history offsets past the
/// first bar, `na` times, user-defined functions, a period passed through a
/// local, `timeframe.main_period`, Boolean history, literal times at the
/// edges of `i64`, calendar parts of `time_close` and `timenow`, loops,
/// `switch`, arrays, and timeframe reads and calendar calls in one module.
const SERVED_FORMS: &[&str] = &[
    concat!(
        "strategy(\"History\", initial_capital = 100000)\n",
        "plot(dayofmonth(time[1], \"UTC\"), \"dom[1]\")\n",
        "plot(hour(time[bar_index % 3], \"UTC+3\"), \"hour[dynamic]\")\n",
        "plot(time_close[bar_index], \"time_close of the first bar\")\n",
        "plot(time_close[bar_index + 1], \"time_close before the first bar\")\n",
        "plot(timeframe.multiplier[bar_index + 1], \"multiplier before the first bar\")\n",
        "plot(dayofweek(time_close, \"UTC\"), \"dayofweek of time_close\")\n",
        "plot(weekofyear(time_close[1], \"UTC-5\"), \"weekofyear of time_close[1]\")\n",
        "plot(nz(hour(time[1], \"UTC\"), -1), \"nz\")\n",
    ),
    concat!(
        "strategy(\"Functions\", initial_capital = 100000)\n",
        "f(t) => dayofmonth(t, \"UTC\")\n",
        "g() => timeframe.in_seconds()\n",
        "h(x) => x + timeframe.multiplier\n",
        "k(t) => time_close - t\n",
        "plot(f(time), \"f\")\n",
        "plot(g(), \"g\")\n",
        "plot(h(1), \"h\")\n",
        "plot(k(time), \"k\")\n",
    ),
    concat!(
        "strategy(\"Periods\", initial_capital = 100000)\n",
        "tf = timeframe.period\n",
        "plot(timeframe.in_seconds(tf), \"through a local\")\n",
        "plot(timeframe.in_seconds(timeframe.main_period), \"main period\")\n",
    ),
    concat!(
        "strategy(\"Booleans\", initial_capital = 100000)\n",
        "plot(timeframe.isintraday[1] ? 1 : 0, \"isintraday[1]\")\n",
        "plot(timeframe.isdaily[bar_index % 3] ? 1 : -1, \"isdaily[dynamic]\")\n",
        "plot(timenow[1], \"timenow[1]\")\n",
        "plot(year(timenow, \"UTC\"), \"year of timenow\")\n",
        "plot(minute(time_close, \"UTC+05:30\"), \"minute of time_close\")\n",
    ),
    concat!(
        "strategy(\"Edges\", initial_capital = 100000)\n",
        "int t = bar_index % 2 == 0 ? na : time\n",
        "plot(hour(t, \"UTC+3\"), \"hour of na\")\n",
        "plot(weekofyear(-9223372036854775807, \"UTC\"), \"weekofyear of the minimum\")\n",
        "plot(hour(9223372036854775807, \"UTC+14:59\"), \"hour of the maximum\")\n",
        "plot(year(-9223372036854775807, \"UTC-14:59\"), \"year of the minimum\")\n",
    ),
    concat!(
        "strategy(\"State\", initial_capital = 100000)\n",
        "var int fridays = 0\n",
        "if timeframe.isintraday and dayofweek(time, \"UTC\") == dayofweek.friday\n",
        "    fridays += 1\n",
        "plot(fridays, \"fridays\")\n",
        "plot(array.sum(array.from(timeframe.multiplier, timeframe.in_seconds())), \"array\")\n",
        "plot(timeframe.in_seconds() / 7, \"int / int\")\n",
        "plot(timeframe.isdwm ? time_close : time, \"ternary\")\n",
        "s = 0\n",
        "for i = 0 to 2\n",
        "    s += hour(time[i], \"UTC\")\n",
        "plot(s, \"loop\")\n",
        "d = switch dayofweek(time, \"UTC\")\n",
        "    dayofweek.monday => 1\n",
        "    dayofweek.tuesday => 2\n",
        "    => 0\n",
        "plot(d, \"switch\")\n",
    ),
];

/// `timestamp` in UTC and in a fixed-offset timezone literal, with fields
/// that are literals, run-time values, `na`, out of range (months carry into
/// the year, days and hours through the arithmetic) and large enough to
/// leave `i64`, which is `na` in both.
const TIMESTAMP: &str = concat!(
    "strategy(\"Timestamp\", initial_capital = 100000)\n",
    "start = timestamp(\"UTC\", 2024, 1, 3, 0, 0)\n",
    "stop = timestamp(2024, 1, 5, 12, 30, 15)\n",
    "plot(start, \"literal start\")\n",
    "plot(stop, \"literal stop\")\n",
    "plot(timestamp(\"UTC+05:30\", 2024, 2, 29, 23, 59, 59), \"leap day in UTC+5:30\")\n",
    "plot(timestamp(\"GMT-0930\", 1969, 12, 31, 23, 59, 59), \"before the epoch\")\n",
    "plot(timestamp(2024, 13, 1, 0, 0), \"month 13\")\n",
    "plot(timestamp(2024, 0, -1, 25, 61, 61), \"fields out of range\")\n",
    "plot(timestamp(\"UTC-14:59\", 2024, -24, 1, 0, 0), \"negative months\")\n",
    "plot(timestamp(292277026, 12, 31, 0, 0), \"past the last time\")\n",
    "plot(timestamp(\"UTC+3\", 9223372036854775807, 1, 1, 0, 0), \"year leaves i64\")\n",
    "plot(timestamp(\"UTC\", 292277026, 1, 1, 0, 0), \"too close to the limit\")\n",
    "y = year(time, \"UTC\")\n",
    "plot(timestamp(\" UTC-5 \", y, month(time, \"UTC\"), dayofmonth(time, \"UTC\"), bar_index % 24, 0), \"run-time fields\")\n",
    "int field = bar_index % 5 == 0 ? na : bar_index % 28 + 1\n",
    "plot(timestamp(\"UTC+1\", 2024, 1, field, 0, 0), \"an na field\")\n",
    "plot(timestamp(2024, field, 1, 0, 0), \"an na month\")\n",
    "if time >= start and time < stop\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if time >= stop\n",
    "    strategy.close(\"L\")\n",
);

#[test]
fn timestamp_calls_match_the_interpreter() {
    let hourly = walk(120, JAN_2024 - 5 * HOUR, HOUR);
    let edges = calendar_bars();
    let mut traded = 0;
    for module_type in [
        StandaloneWasmModuleType::Strategy,
        StandaloneWasmModuleType::Indicator,
    ] {
        let wasm =
            build_as(TIMESTAMP, module_type).unwrap_or_else(|e| panic!("{module_type:?}: {e}"));
        // No timeframe read, so the request needs no `timeframe`.
        let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
        assert!(capabilities.get("required_fields").is_none());
        traded += assert_matches_interpreter(TIMESTAMP, &wasm, &hourly, "60");
        assert_matches_interpreter(TIMESTAMP, &wasm, &edges, "1D");
    }
    assert!(traded > 0, "the fixture must trade: {traded}");
}

#[test]
fn a_timestamp_without_a_fixed_literal_timezone_still_needs_a_host() {
    // An IANA zone, whose clocks change, and a timezone chosen at run time
    // keep `host.vm`, as for a calendar call.
    for call in [
        "timestamp(\"America/New_York\", 2024, 1, 1, 0, 0)",
        "timestamp(close > open ? \"UTC\" : \"UTC+3\", 2024, 1, 1, 0, 0)",
        "timestamp(syminfo.timezone, 2024, 1, 1, 0, 0)",
    ] {
        let source = format!(
            "strategy(\"Zone\")\nif time > {call}\n    strategy.entry(\"L\", strategy.long)\n"
        );
        match build(&source) {
            // `syminfo.timezone` is named too: the module has no exchange
            // timezone as a string either.
            Err(AotError::StrategyFeatureUnsupported { feature }) => {
                assert!(
                    feature.split(", ").any(|f| f == "timestamp"),
                    "{call}: {feature}"
                );
            }
            other => panic!("{call}: {other:?}"),
        }
    }
}

/// A `timestamp` in a timezone is charged for its literal as a calendar call
/// is: a thousand calls a bar fit with `"UTC"` and do not with a literal of
/// a thousand and one bytes that also reads as UTC.
#[test]
fn a_timestamp_is_charged_for_its_timezone_literal() {
    let bars = walk(2, JAN_2024, HOUR);
    let timeframe = format!("{}1", "0".repeat(999_999));
    let source = |timezone: &str| {
        format!(
            "strategy(\"Budget\")\nint s = 0\nfor i = 1 to 63\n    s += timeframe.in_seconds()\nfor j = 1 to 1000\n    s += timestamp(\"{timezone}\", 2024, 1, 1, 0, 0) % 7\nplot(s)\n"
        )
    };
    let long = format!("UTC{}", " ".repeat(998));
    for (timezone, fits) in [("UTC", true), (long.as_str(), false)] {
        let source = source(timezone);
        let wasm = build(&source).expect("compiles");
        if fits {
            assert_matches_interpreter(&source, &wasm, &bars, &timeframe);
            continue;
        }
        let stopped = run(
            &compile_csil(&source),
            &market_data(&bars, &timeframe),
            &RunOptions::default(),
        );
        assert!(
            stopped.errors.iter().any(|e| {
                e.kind == RuntimeErrorKind::InstructionLimitExceeded && e.bar_index == Some(0)
            }),
            "{:?}",
            stopped.errors
        );
        let fields = format!("\"timeframe\":\"{timeframe}\",");
        let got = evaluate(&wasm, request(&bars, &fields).as_bytes());
        assert!(
            got.as_ref()
                .is_err_and(|e| e.contains("per-bar instruction budget exceeded")),
            "{got:?}"
        );
    }
}

/// `timeframe.change` and `time` over a fixed-width literal, in a module and
/// in the interpreter: shorter than the chart's timeframe (`time` is `na`),
/// the chart's own, a width that divides a day, one that does not and so
/// restarts at midnight, a day, and several days walked from the first bar.
#[test]
fn a_fixed_timeframe_change_matches_the_interpreter() {
    let hourly = walk(200, JAN_2024 + 13 * HOUR, HOUR);
    let daily = walk(120, JAN_2024 + 3 * DAY, DAY);
    // Hourly bars with a weekend-sized hole and an uneven step after it.
    let mut gapped = walk(150, JAN_2024 + 13 * HOUR, HOUR);
    for (i, bar) in gapped.iter_mut().enumerate().skip(60) {
        bar.time += 50 * HOUR + if i >= 100 { 30 * 60_000 } else { 0 };
    }
    for timeframe in ["1", "120S", "60", "240", "420", "D", "1D", "2D", "3D"] {
        let source = format!(
            concat!(
                "strategy(\"Fixed change\", initial_capital = 100000)\n",
                "c = timeframe.change(\"{tf}\")\n",
                "plot(c ? 1 : 0, \"change\")\n",
                "plot(time(\"{tf}\"), \"time\")\n",
                "if c and close > open\n",
                "    strategy.entry(\"L\", strategy.long)\n",
                "if c and close < open\n",
                "    strategy.close(\"L\")\n",
            ),
            tf = timeframe
        );
        for module_type in [
            StandaloneWasmModuleType::Strategy,
            StandaloneWasmModuleType::Indicator,
        ] {
            let wasm = build_as(&source, module_type)
                .unwrap_or_else(|e| panic!("{timeframe} {module_type:?}: {e}"));
            let capabilities =
                custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
            // It compares its timeframe with the chart's, and groups bars
            // on the exchange's clock and session.
            assert_eq!(
                capabilities["required_fields"],
                serde_json::json!(["timeframe", "timezone", "session_hours"]),
                "{timeframe}"
            );
            for (bars, chart) in [
                (&hourly, "60"),
                (&hourly, "240"),
                (&daily, "1D"),
                (&daily, "1W"),
                (&daily, "1M"),
                (&gapped, "60"),
            ] {
                assert_matches_interpreter(&source, &wasm, bars, chart);
            }
        }
    }
}

#[test]
fn a_blank_timeframe_change_is_the_chart_period() {
    // The interpreter's `time` reads a timeframe that trims to nothing as
    // the chart's own: the bar's open time, with no bucket and no charge.
    // So every bar is a change, and a module needs no `timeframe` field.
    let hourly = walk(80, JAN_2024 + 13 * HOUR, HOUR);
    for timeframe in ["", " "] {
        let source = format!(
            "strategy(\"Blank change\")\nplot(timeframe.change(\"{timeframe}\") ? 1 : 0)\nplot(time(\"{timeframe}\"))\n"
        );
        let wasm = build_as(&source, StandaloneWasmModuleType::Strategy)
            .unwrap_or_else(|e| panic!("{timeframe:?}: {e}"));
        let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
        assert!(
            capabilities.get("required_fields").is_none(),
            "{timeframe:?}: the chart's own period needs no timeframe"
        );
        for chart in ["60", "1D"] {
            assert_matches_interpreter(&source, &wasm, &hourly, chart);
        }
    }
}

#[test]
fn a_time_open_lane_beside_a_request_security_lane_matches_the_interpreter() {
    let hourly = walk(200, JAN_2024 + 13 * HOUR, HOUR);
    let source = concat!(
        "strategy(\"Mixed lanes\")\n",
        "plot(timeframe.change(\"240\") ? 1 : 0)\n",
        "plot(request.security(syminfo.tickerid, \"240\", close))\n",
        "plot(time(\"D\"))\n",
        "plot(request.security(syminfo.tickerid, \"D\", high, lookahead = barmerge.lookahead_on))\n",
    );
    let wasm = build_as(source, StandaloneWasmModuleType::Strategy).expect("standalone");
    for chart in ["60", "240"] {
        assert_matches_interpreter(source, &wasm, &hourly, chart);
    }
}

#[test]
fn a_calendar_timeframe_change_still_needs_a_host() {
    // A calendar week or month follows the exchange's trading days, which the
    // request does not carry: such a call keeps `host.vm`, and the refusal
    // names what the script wrote.
    for timeframe in ["W", "2W", "M", "3M", "12M"] {
        let source = format!(
            "strategy(\"Calendar change\")\nif timeframe.change(\"{timeframe}\")\n    strategy.entry(\"L\", strategy.long)\n"
        );
        let error = build(&source).expect_err(timeframe).to_string();
        assert_eq!(
            error, "strategy feature unsupported for standalone WebAssembly: timeframe.change",
            "{timeframe}"
        );
    }
    // A plain `time` call is named as written.
    let error = build("strategy(\"Calendar time\")\nplot(time(\"W\"))\n")
        .expect_err("a calendar week")
        .to_string();
    assert_eq!(
        error,
        "strategy feature unsupported for standalone WebAssembly: time"
    );
}

#[test]
fn every_served_form_matches_the_interpreter_in_either_module_type() {
    let hourly = walk(120, JAN_2024 - 5 * HOUR, HOUR);
    let edges = calendar_bars();
    for source in SERVED_FORMS {
        for module_type in [
            StandaloneWasmModuleType::Strategy,
            StandaloneWasmModuleType::Indicator,
        ] {
            let wasm = build_as(source, module_type)
                .unwrap_or_else(|e| panic!("{module_type:?}: {e}\n{source}"));
            for timeframe in ["60", "1D", "30S"] {
                assert_matches_interpreter(source, &wasm, &hourly, timeframe);
                assert_matches_interpreter(source, &wasm, &edges, timeframe);
            }
        }
    }
}

/// A served call is charged what the interpreter charges for it: its
/// dispatch unit, then a unit per byte of its string argument after the
/// first, so a budget runs out at the same instruction.
#[test]
fn a_served_call_is_charged_for_its_string_argument() {
    let bars = walk(2, JAN_2024, HOUR);
    // A chart timeframe that reads as one minute and is a million bytes
    // long: with it, 63 calls of `timeframe.in_seconds()` a bar fit the
    // per-bar budget of 64 million instructions and 64 do not.
    let timeframe = format!("{}1", "0".repeat(999_999));
    let periods = |calls: u32| {
        format!("strategy(\"Budget\")\nint s = 0\nfor i = 1 to {calls}\n    s += timeframe.in_seconds()\n")
    };
    // After 63 of them, a thousand calendar calls a bar fit with a short
    // timezone literal and do not with a literal of a thousand and one bytes
    // that also reads as UTC. (A string constant is reserved against the
    // run's string budget each time it is pushed, so the extra cannot be
    // shown with the literal alone.)
    let calendars = |timezone: &str| {
        format!(
            "{}for j = 1 to 1000\n    s += hour(time, \"{timezone}\")\n",
            periods(63)
        )
    };
    let long = format!("UTC{}", " ".repeat(998));
    for (body, fits) in [
        (periods(63), true),
        (periods(64), false),
        (calendars("UTC"), true),
        (calendars(&long), false),
    ] {
        let source = format!("{body}plot(s)\n");
        let wasm = build(&source).expect("compiles");
        if fits {
            assert_matches_interpreter(&source, &wasm, &bars, &timeframe);
            continue;
        }
        let stopped = run(
            &compile_csil(&source),
            &market_data(&bars, &timeframe),
            &RunOptions::default(),
        );
        assert!(
            stopped.errors.iter().any(|e| {
                e.kind == RuntimeErrorKind::InstructionLimitExceeded && e.bar_index == Some(0)
            }),
            "{:?}",
            stopped.errors
        );
        let fields = format!("\"timeframe\":\"{timeframe}\",");
        let got = evaluate(&wasm, request(&bars, &fields).as_bytes());
        assert!(
            got.as_ref()
                .is_err_and(|e| e.contains("per-bar instruction budget exceeded")),
            "{got:?}"
        );
    }
}

#[test]
fn the_request_must_carry_a_valid_timeframe() {
    let source = concat!(
        "strategy(\"Needs timeframe\")\n",
        "if timeframe.in_seconds() > 60 and close > open\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "plot(close)\n",
    );
    let wasm = build(source).expect("compiles");
    let bars = walk(50, JAN_2024, HOUR);
    for (fields, error) in [
        ("", "missing required field: timeframe"),
        ("\"timeframe\":60,", "timeframe must be a string"),
        ("\"timeframe\":null,", "timeframe must be a string"),
        (
            "\"timeframe\":\"\\u0036\\u0030\",",
            "timeframe must be ASCII text without escapes",
        ),
        (
            "\"timeframe\":\"６０\",",
            "timeframe must be ASCII text without escapes",
        ),
        (
            "\"timeframe\":\"60\",\"timeframe\":\"60\",",
            "duplicate top-level key: timeframe",
        ),
    ] {
        let got = evaluate(&wasm, request(&bars, fields).as_bytes());
        assert_eq!(got.err().as_deref(), Some(error), "{fields}");
    }
    // A length with no millisecond value, and one that moves a bar's close
    // past `i64`: the interpreter refuses the market data, and the module
    // refuses the request.
    let huge = "999999999999999999D";
    let vm = run(
        &compile_csil(source),
        &market_data(&bars, huge),
        &RunOptions::default(),
    );
    assert!(!vm.errors.is_empty(), "the interpreter refuses {huge}");
    let got = evaluate(
        &wasm,
        request(&bars, &format!("\"timeframe\":\"{huge}\",")).as_bytes(),
    );
    assert!(
        got.as_ref()
            .is_err_and(|e| e.contains("has no representable millisecond duration")),
        "{got:?}"
    );
    // A close past `i64` on a later bar: the interpreter refuses the market
    // data at that bar. A run over the bars before it (`fraction`) is a run
    // over those bars alone, which it accepts.
    let late: Vec<Bar> = [JAN_2024, JAN_2024 + DAY, 9_000_000_000_000_000]
        .into_iter()
        .map(|time| Bar {
            time,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
        })
        .collect();
    let past = "9223000000000000S";
    let refused = run(
        &compile_csil(source),
        &market_data(&late, past),
        &RunOptions::default(),
    );
    assert!(
        format!("{:?}", refused.errors).contains("at bar 2"),
        "{:?}",
        refused.errors
    );
    let fields = format!("\"timeframe\":\"{past}\",");
    assert_eq!(
        evaluate(&wasm, request(&late, &fields).as_bytes())
            .err()
            .as_deref(),
        Some("timeframe close time exceeds the i64 timestamp range at bar 2")
    );
    let prefix = run(
        &compile_csil(source),
        &market_data(&late[..2], past),
        &RunOptions::default(),
    );
    assert!(prefix.errors.is_empty(), "{:?}", prefix.errors);
    evaluate(
        &wasm,
        request(&late, &format!("{fields}\"fraction\":0.67,")).as_bytes(),
    )
    .expect("the two bars before it");

    // A bar time the time lane cannot hold exactly is refused, as binary
    // input refuses it, so no served read sees a rounded time.
    let unsafe_time = [Bar {
        time: 9_007_199_254_740_992,
        open: 1.0,
        high: 1.0,
        low: 1.0,
        close: 1.0,
    }];
    assert_eq!(
        evaluate(
            &wasm,
            request(&unsafe_time, "\"timeframe\":\"1D\",").as_bytes()
        )
        .err()
        .as_deref(),
        Some("bar time must be a safe integer")
    );
}

/// The same field in binary metadata, and in each reference to a resident
/// dataset, which names its own timeframe: every reply is the JSON reply,
/// and so the interpreter's.
#[test]
fn binary_and_resident_requests_carry_the_timeframe() {
    let wasm = build(CHART_TIMEFRAME).expect("compiles");
    let bars = walk(120, JAN_2024, HOUR);
    let json = |timeframe: &str| {
        assert_matches_interpreter(CHART_TIMEFRAME, &wasm, &bars, timeframe);
        let fields = format!(
            "\"timeframe\":\"{timeframe}\",\"include_results\":true,\"mincontract\":0.001,"
        );
        evaluate(&wasm, request(&bars, &fields).as_bytes()).expect("JSON request")
    };
    let metadata = |timeframe: Option<&str>| {
        let mut metadata = serde_json::json!({
            "version": 1,
            "params": {},
            "include_results": true,
            "mincontract": 0.001,
        });
        if let Some(timeframe) = timeframe {
            metadata["timeframe"] = timeframe.into();
        }
        metadata.to_string().into_bytes()
    };
    assert_eq!(
        evaluate(&wasm, &binary_packet(&bars, &metadata(Some("240")))).expect("binary request"),
        json("240")
    );
    assert_eq!(
        evaluate(&wasm, &binary_packet(&bars, &metadata(None)))
            .err()
            .as_deref(),
        Some("missing required field: timeframe")
    );
    let mut resident = Instance::new(&wasm);
    assert_eq!(
        resident
            .evaluate(&resident_packet(&bars, &metadata(Some("60")), 7, false))
            .expect("upload"),
        json("60")
    );
    assert_eq!(
        resident
            .evaluate(&resident_packet(&bars, &metadata(Some("1D")), 7, true))
            .expect("reference"),
        json("1D")
    );
    // A refused reference gives up the dataset, as any failed evaluation does.
    assert_eq!(
        resident
            .evaluate(&resident_packet(&bars, &metadata(None), 7, true))
            .err()
            .as_deref(),
        Some("missing required field: timeframe")
    );
    assert!(resident
        .evaluate(&resident_packet(&bars, &metadata(Some("1D")), 7, true))
        .unwrap_err()
        .contains("stale or missing"));
}

#[test]
fn timeframe_isticks_alone_needs_no_timeframe() {
    // There are no tick charts, so the predicate is false whatever the
    // timeframe; the module answers it without one.
    let source = "strategy(\"Ticks\")\nplot(timeframe.isticks ? 1 : 0)\n";
    let wasm = build(source).expect("compiles");
    let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
    assert!(capabilities.get("required_fields").is_none());
    let out = evaluate(
        &wasm,
        request(&walk(3, JAN_2024, HOUR), "\"include_results\":true,").as_bytes(),
    )
    .expect("no timeframe needed");
    let out: serde_json::Value = serde_json::from_slice(&out).expect("output JSON");
    assert_eq!(out["plots"], serde_json::json!([[0.0, 0.0, 0.0]]));
}

#[test]
fn a_module_that_does_not_read_the_timeframe_ignores_the_field() {
    let source = "strategy(\"No timeframe\")\nif close > open\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n";
    let wasm = build(source).expect("compiles");
    let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
    assert!(capabilities.get("required_fields").is_none());
    let bars = walk(50, JAN_2024, HOUR);
    let plain = evaluate(&wasm, request(&bars, "").as_bytes()).expect("no field");
    for fields in [
        "\"timeframe\":\"60\",",
        "\"timeframe\":60,",
        "\"timeframe\":\"\\u0036\",",
        "\"timeframe\":\"60\",\"timeframe\":1,",
    ] {
        assert_eq!(
            evaluate(&wasm, request(&bars, fields).as_bytes()).as_ref(),
            Ok(&plain),
            "{fields}"
        );
    }
}

#[test]
fn timeframe_period_as_a_string_still_needs_a_host() {
    // The module has the timeframe's numbers, not the string the
    // interpreter compares or chooses between, so those uses stay refused,
    // by name.
    for (condition, features) in [
        (
            "timeframe.period == \"D\"",
            "string comparison (== / !=), timeframe.period",
        ),
        (
            "timeframe.in_seconds(close > open ? timeframe.period : \"60\") > 60",
            "timeframe.in_seconds, timeframe.period",
        ),
    ] {
        let source = format!(
            "strategy(\"Period\")\nif {condition}\n    strategy.entry(\"L\", strategy.long)\n"
        );
        match build(&source) {
            Err(AotError::StrategyFeatureUnsupported { feature }) => {
                assert_eq!(feature, features, "{condition}");
            }
            other => panic!("{condition}: {other:?}"),
        }
    }
}

#[test]
fn a_strategy_refusal_names_the_call_not_the_argument_the_compiler_supplied() {
    // `ticker.standard()` reads `syminfo.tickerid` for its omitted
    // argument; the script wrote only the call.
    let supplied = "strategy(\"Supplied\")\nif str.length(ticker.standard()) > 3\n    strategy.entry(\"L\", strategy.long)\n";
    match build(supplied) {
        Err(AotError::StrategyFeatureUnsupported { feature }) => {
            assert_eq!(feature, "str.length, ticker.standard");
        }
        other => panic!("{other:?}"),
    }
    // Written out, the argument is the script's own and is named too.
    let written = "strategy(\"Written\")\nif str.length(ticker.standard(syminfo.tickerid)) > 3\n    strategy.entry(\"L\", strategy.long)\n";
    match build(written) {
        Err(AotError::StrategyFeatureUnsupported { feature }) => {
            assert_eq!(feature, "str.length, syminfo.tickerid, ticker.standard");
        }
        other => panic!("{other:?}"),
    }
}

/// A strategy whose filter length follows the chart timeframe, whose
/// momentum counts UTC calendar days, and whose orders are confined to a
/// date window by `time_close`: the reads that keep such scripts out of
/// batch parameter runs.
const TIMEFRAME_SCALED: &str = concat!(
    "//@version=6\n",
    "strategy(\"Timeframe scaled\", initial_capital = 10000, default_qty_type = strategy.percent_of_equity, default_qty_value = 95, commission_type = strategy.commission.percent, commission_value = 0.1)\n",
    "length = input.int(20, \"Length\", minval = 2)\n",
    "days = input.int(10, \"Momentum days\", minval = 2, maxval = 60)\n",
    "startDate = input.time(timestamp(\"01 Feb 2024 00:00 +0000\"), \"Start\")\n",
    "endDate = input.time(timestamp(\"01 Aug 2024 00:00 +0000\"), \"End\")\n",
    "float barSeconds = timeframe.in_seconds()\n",
    "int scaled = barSeconds > 14400 ? math.max(2, math.round(length * 14400.0 / barSeconds)) : length\n",
    "float alpha = 2.0 / (scaled + 1)\n",
    "var float basis = na\n",
    "basis := na(basis) ? close : basis + alpha * (close - basis)\n",
    "var array<float> dailyCloses = array.new<float>()\n",
    "if bar_index > 0 and dayofmonth(time, \"UTC\") != dayofmonth(time[1], \"UTC\")\n",
    "    array.push(dailyCloses, close[1])\n",
    "    if array.size(dailyCloses) > days\n",
    "        array.shift(dailyCloses)\n",
    "bool rising = array.size(dailyCloses) == days and close > array.get(dailyCloses, 0)\n",
    "bool inWindow = time >= startDate and time_close < endDate\n",
    "if inWindow and rising and close > basis and strategy.position_size == 0\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if strategy.position_size > 0 and (close < basis or time_close >= endDate)\n",
    "    strategy.close(\"L\", immediately = time_close >= endDate)\n",
    "plot(basis, \"Basis\")\n",
);

#[test]
fn a_timeframe_scaled_strategy_matches_the_interpreter_on_each_chart() {
    let wasm = build(TIMEFRAME_SCALED).expect("compiles for batch parameter runs");
    let mut traded = 0;
    for (timeframe, step, count) in [
        ("240", 4 * HOUR, 2000),
        ("60", HOUR, 6000),
        ("1D", DAY, 400),
    ] {
        let bars = walk(count, JAN_2024 - 30 * DAY, step);
        traded += assert_matches_interpreter(TIMEFRAME_SCALED, &wasm, &bars, timeframe);
    }
    assert!(traded > 10, "the fixture must trade: {traded}");
}

/// A strategy whose orders follow each grouping a module serves.
const GROUPED: &str = concat!(
    "strategy(\"Grouped\", initial_capital = 100000)\n",
    "d = timeframe.change(\"D\")\n",
    "plot(d ? 1 : 0, \"day\")\n",
    "plot(time(\"D\"), \"day open\")\n",
    "plot(request.security(syminfo.tickerid, \"D\", high), \"day high\")\n",
    "if d and close > open\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if d and close < open\n",
    "    strategy.close(\"L\")\n",
);

/// [`GROUPED`] with intraday and multi-day buckets too.
const GROUPED_INTRADAY: &str = concat!(
    "strategy(\"Grouped intraday\", initial_capital = 100000)\n",
    "c = timeframe.change(\"240\")\n",
    "plot(c ? 1 : 0, \"four hours\")\n",
    "plot(time(\"120\"), \"two-hour open\")\n",
    "plot(timeframe.change(\"2D\") ? 1 : 0, \"two days\")\n",
    "plot(request.security(syminfo.tickerid, \"240\", close), \"four-hour close\")\n",
    "if c and close > open\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if c and close < open\n",
    "    strategy.close(\"L\")\n",
);

#[test]
fn a_module_that_groups_bars_requires_the_exchange_fields() {
    let bars = walk(50, JAN_2024, HOUR);
    let wasm = build(GROUPED).expect("standalone");
    let capabilities = custom_section(&wasm, "pinewasm_capabilities").expect("capabilities");
    // `time("D")` compares its timeframe with the chart's too.
    assert_eq!(
        capabilities["required_fields"],
        serde_json::json!(["timeframe", "timezone", "session_hours"])
    );
    for (fields, missing) in [
        ("\"timezone\":\"UTC\",\"session_hours\":\"\",", "timeframe"),
        ("\"timeframe\":\"60\",\"session_hours\":\"\",", "timezone"),
        (
            "\"timeframe\":\"60\",\"timezone\":\"UTC\",",
            "session_hours",
        ),
    ] {
        let error = evaluate(&wasm, request_raw(&bars, fields).as_bytes()).expect_err(missing);
        assert_eq!(error, format!("missing required field: {missing}"));
    }
    for (fields, message) in [
        (
            "\"timeframe\":\"60\",\"timezone\":5,\"session_hours\":\"\",",
            "timezone must be a string",
        ),
        (
            "\"timeframe\":\"60\",\"timezone\":\"UTC\",\"session_hours\":\"9-5\",",
            "session_hours must be a session specification such as 24x7 or 0930-1600:23456",
        ),
    ] {
        let error = evaluate(&wasm, request_raw(&bars, fields).as_bytes()).expect_err(message);
        assert_eq!(error, message);
    }
}

#[test]
fn a_fixed_offset_exchange_groups_bars_as_the_interpreter_does() {
    let hourly = walk(400, JAN_2024 + 3 * HOUR, HOUR);
    let daily = walk(120, JAN_2024 + 14 * HOUR + 30 * 60_000, DAY);
    let grouped = build(GROUPED).expect("standalone");
    let intraday = build(GROUPED_INTRADAY).expect("standalone");
    let mut traded = 0;
    for timezone in ["UTC-5", "UTC+05:30", "GMT+14", "UTC"] {
        // Around the clock: every grouping is settled. (The intraday
        // requests sit below a daily chart's period, which the interpreter
        // refuses, so they run on hourly bars only.)
        for (bars, chart) in [(&hourly, "60"), (&daily, "1D")] {
            traded +=
                assert_matches_interpreter_on(GROUPED, &grouped, bars, chart, timezone, "24x7");
        }
        traded += assert_matches_interpreter_on(
            GROUPED_INTRADAY,
            &intraday,
            &hourly,
            "60",
            timezone,
            "24x7",
        );
        // A day session settles days.
        traded +=
            assert_matches_interpreter_on(GROUPED, &grouped, &daily, "1D", timezone, "0930-1600");
    }
    // No session declared on a UTC exchange: the midnight walk.
    traded += assert_matches_interpreter_on(GROUPED_INTRADAY, &intraday, &hourly, "60", "UTC", "");
    assert!(traded > 20, "the fixtures must trade: {traded}");
}

/// The error the interpreter stops `source` with, over `bars` on the
/// exchange `timezone` with session `session`.
fn interpreter_error(
    source: &str,
    bars: &[Bar],
    chart: &str,
    timezone: &str,
    session: &str,
) -> String {
    let mut data = market_data(bars, chart);
    data.syminfo.timezone = timezone.to_string();
    data.syminfo.session_hours = session.to_string();
    let outcome = run(&compile_csil(source), &data, &RunOptions::default());
    outcome
        .errors
        .first()
        .unwrap_or_else(|| panic!("{timezone:?} {session:?}: the interpreter ran"))
        .message
        .clone()
}

fn module_error(wasm: &[u8], bars: &[Bar], chart: &str, timezone: &str, session: &str) -> String {
    let fields = format!(
        "\"timeframe\":{},\"timezone\":{},\"session_hours\":{},",
        serde_json::Value::from(chart),
        serde_json::Value::from(timezone),
        serde_json::Value::from(session),
    );
    evaluate(wasm, request(bars, &fields).as_bytes()).expect_err("the module ran")
}

#[test]
fn an_unsettled_session_is_refused_by_the_module_and_the_interpreter() {
    let hourly = walk(100, JAN_2024 + 13 * HOUR, HOUR);
    let wasm = build(GROUPED_INTRADAY).expect("standalone");
    for (timezone, session) in [("UTC-4", "0930-1600"), ("UTC-5", ""), ("UTC", "1700-1600")] {
        let interpreter = interpreter_error(GROUPED_INTRADAY, &hourly, "60", timezone, session);
        assert!(
            interpreter.contains("does not group these bars"),
            "{interpreter}"
        );
        assert_eq!(
            module_error(&wasm, &hourly, "60", timezone, session),
            "Indicator failure: where this market's session starts each requested bucket is not settled in PineWASM yet",
            "{timezone:?} {session:?}"
        );
    }
    // A day of an overnight session is unsettled too, without a lane.
    let wasm = build(GROUPED).expect("standalone");
    let interpreter = interpreter_error(GROUPED, &hourly, "60", "UTC", "1700-1600");
    assert!(
        interpreter.contains("does not group these bars"),
        "{interpreter}"
    );
    assert!(module_error(&wasm, &hourly, "60", "UTC", "1700-1600").contains("not settled"));
}

#[test]
fn an_exchange_whose_clocks_change_is_refused_by_the_module() {
    // The interpreter follows New York's clock changes; the module carries
    // no zone rules, so it refuses rather than group on a wrong clock.
    let hourly = walk(100, JAN_2024 + 13 * HOUR, HOUR);
    let wasm = build(GROUPED).expect("standalone");
    let mut data = market_data(&hourly, "60");
    data.syminfo.timezone = "America/New_York".to_string();
    data.syminfo.session_hours = "24x7".to_string();
    let outcome = run(&compile_csil(GROUPED), &data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(
        module_error(&wasm, &hourly, "60", "America/New_York", "24x7"),
        "Indicator failure: standalone WebAssembly groups bars only on a fixed-offset exchange clock, and this request's timezone is not one"
    );
}

#[test]
fn a_grouping_that_never_runs_cannot_fail_the_evaluation() {
    // As in the interpreter, a site is settled only when it executes.
    let source = concat!(
        "strategy(\"Never\", initial_capital = 100000)\n",
        "x = bar_index < 0 ? time(\"240\") : 0\n",
        "plot(x, \"never\")\n",
    );
    let hourly = walk(60, JAN_2024 + 13 * HOUR, HOUR);
    let wasm = build(source).expect("standalone");
    for (timezone, session) in [("UTC-4", "0930-1600"), ("America/New_York", "24x7")] {
        assert_matches_interpreter_on(source, &wasm, &hourly, "60", timezone, session);
    }
}

#[test]
fn a_request_no_longer_than_the_charts_runs_on_any_session_and_zone() {
    // Each bar is its own bucket, so neither the module nor the interpreter
    // needs the session or the clock; both run, bar for bar alike.
    let source = concat!(
        "strategy(\"Own period\", initial_capital = 100000)\n",
        "c = timeframe.change(\"60\")\n",
        "plot(c ? 1 : 0, \"own change\")\n",
        "plot(time(\"60\"), \"own open\")\n",
        "plot(na(time(\"30\")) ? 1 : 0, \"lower\")\n",
        "plot(request.security(syminfo.tickerid, timeframe.period, close), \"period close\")\n",
        "plot(request.security(syminfo.tickerid, \"60\", high), \"hour high\")\n",
        "if c and close > open\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if c and close < open\n",
        "    strategy.close(\"L\")\n",
    );
    let wasm = build(source).expect("standalone");
    let hourly = walk(120, JAN_2024 + 14 * HOUR, HOUR);
    let mut traded = 0;
    for (timezone, session) in [
        ("UTC-5", "0930-1600"),
        ("America/New_York", "0930-1600"),
        ("America/New_York", "24x7"),
        ("UTC", "1700-1600"),
    ] {
        traded += assert_matches_interpreter_on(source, &wasm, &hourly, "60", timezone, session);
    }
    assert!(traded > 10, "the fixture must trade: {traded}");
}
