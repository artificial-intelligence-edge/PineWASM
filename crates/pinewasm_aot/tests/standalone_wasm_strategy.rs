//! Native `strategy()` → standalone WebAssembly lowering over synthetic fixtures.
//!
//! Four pinned claims, in dependency order:
//!
//! 1. **Artifact isolation** — indicator-sourced artifacts (standalone WebAssembly and standalone)
//!    use the non-broker runtime variants. Pinned hashes catch accidental
//!    broker linkage or other byte-level movement in those artifact families.
//! 2. **Differential** — for synthetic strategy fixtures (order management,
//!    a stop-exit script, `process_orders_on_close`, and a crossover), the standalone WebAssembly
//!    artifact's posture lane — reconstructed losslessly from its OUT
//!    signal events — equals `sign(strategy.position_size)` from the
//!    interpreter bar-for-bar, on the same bars under the same
//!    default market data. Focused scenarios in `differential.rs` extend this gate.
//! 3. **Conformance** — the emitted signal series is valid under the
//!    `posture-events` semantics (first event nonzero, no 0-after-0,
//!    strictly increasing bar-matched times, price = event bar's close),
//!    the metadata carries the posture-event signal object, and the
//!    strategy feature gates refuse by name.
//! 4. **Trade ledger** — with `trade_results`, the compact `broker-trades-v1`
//!    document equals the interpreter's strategy outcome field for field, on
//!    every order type, sizing, commission type and fill timing, and each row's
//!    P&L recomputes from the row alone. It carries no posture events, so its
//!    size follows the retained rows and not the length of the run. Requests
//!    without the flag, and indicator-sourced artifacts, keep their bytes.

use pinewasm_aot::{
    compile_standalone_wasm_program, compile_standalone_wasm_program_with_config, AotError,
    StandaloneWasmBuildConfig, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};

// ---------------------------------------------------------------------------
// Synthetic fixtures. These stay self-contained and use only generated market data.

const ORDER_PRIMITIVES: &str = include_str!("../../../tests/compile-pass/strategy_orders.pine");
const PROCESS_ON_CLOSE: &str = concat!(
    "strategy(\"Close pass\", process_orders_on_close = true)\n",
    "if close > open\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if close < open\n",
    "    strategy.close(\"L\")\n",
    "plot(close)\n",
);
const STOP_EXIT: &str = concat!(
    "strategy(\"Stop exit\", initial_capital = 10000)\n",
    "basis = ta.sma(close, 10)\n",
    "if close > basis\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "strategy.exit(\"X\", \"L\", stop = close - 2.0)\n",
    "plot(close)\n",
);
const CROSSOVER: &str = concat!(
    "strategy(\"Crossover\", initial_capital = 10000)\n",
    "fast = ta.sma(close, 5)\n",
    "slow = ta.sma(close, 20)\n",
    "if fast > slow\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if fast < slow\n",
    "    strategy.close(\"L\")\n",
    "plot(close)\n",
);

/// A higher-timeframe trend filter: the module resamples the bars itself.
const SECURITY_FILTER: &str = concat!(
    "strategy(\"Security\", initial_capital = 10000)\n",
    "trend = request.security(syminfo.tickerid, \"3D\", close)\n",
    "fast = ta.sma(close, 5)\n",
    "if not na(trend) and fast > trend\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if not na(trend) and fast < trend\n",
    "    strategy.close(\"L\")\n",
    "plot(close)\n",
);

/// A higher-timeframe moving average: a compound request evaluated by the
/// module's own secondary context.
const SECURITY_EXPRESSION: &str = concat!(
    "strategy(\"Security Expr\", initial_capital = 10000)\n",
    "trend = request.security(syminfo.tickerid, \"3D\", ta.ema(close, 4))\n",
    "if not na(trend) and close > trend\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if not na(trend) and close < trend\n",
    "    strategy.close(\"L\")\n",
    "plot(close)\n",
);

/// Orders decided by a rolling window kept in an array: the module's native
/// heap, allocating beside its broker.
const ARRAY_SIGNAL: &str = concat!(
    "strategy(\"Array signal\", initial_capital = 10000)\n",
    "var float[] window = array.new_float(0)\n",
    "array.push(window, close)\n",
    "if array.size(window) > 8\n",
    "    array.shift(window)\n",
    "mean = array.avg(window)\n",
    "if close > mean\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if close < mean\n",
    "    strategy.close(\"L\")\n",
    "plot(mean)\n",
);

/// Orders decided by a user-defined type's fields, with a fresh array every
/// bar so the heap keeps allocating for the whole run.
const OBJECT_SIGNAL: &str = concat!(
    "strategy(\"Object signal\", initial_capital = 10000)\n",
    "type Trend\n",
    "    float level\n",
    "    int streak\n",
    "var Trend trend = Trend.new(close, 0)\n",
    "recent = array.new_float(0)\n",
    "array.push(recent, close)\n",
    "array.push(recent, close[1])\n",
    "array.push(recent, close[2])\n",
    "trend.streak := close > trend.level ? trend.streak + 1 : 0\n",
    "trend.level := array.max(recent)\n",
    "if trend.streak >= 2\n",
    "    strategy.entry(\"S\", strategy.short)\n",
    "if trend.streak == 0 and close < array.min(recent) + 1\n",
    "    strategy.close(\"S\")\n",
    "plot(trend.level)\n",
);

/// A synthetic strategy: a signal computed from arrays, and
/// around it a label, a statistics table fed by a calendar call, and orders
/// that carry a conditional comment and built alert message. None of the display code
/// can reach an order or a plot value. The calendar call is the condition
/// of a branch that only feeds the table, so the branch is skipped and the
/// call goes with it.
const DISPLAY_AROUND_ORDERS: &str = concat!(
    "strategy(\"Display\", initial_capital = 10000)\n",
    "var float[] window = array.new_float(0)\n",
    "array.push(window, close)\n",
    "if array.size(window) > 6\n",
    "    array.shift(window)\n",
    "mean = array.avg(window)\n",
    "goLong = close > mean\n",
    "goFlat = close < mean\n",
    "var float[] monthly = array.new_float(0)\n",
    "if month != month[1]\n",
    "    array.push(monthly, close)\n",
    "var table stats = table.new(position.top_right, 1, 1)\n",
    "if barstate.islast\n",
    "    table.cell(stats, 0, 0, str.format(\"{0}\", array.stdev(monthly)))\n",
    "if goLong\n",
    "    label.new(bar_index, low, \"Buy \" + str.tostring(close))\n",
    "reason = goFlat ? \"Below\" : \"Above\"\n",
    "if goLong\n",
    "    strategy.entry(\"L\", strategy.long, comment = reason, ",
    "alert_message = \"Open | \" + syminfo.ticker + \"@\" + str.tostring(close) + \" (\" + timeframe.period + \")\")\n",
    "if goFlat\n",
    "    strategy.close(\"L\", comment = reason, ",
    "alert_message = \"Close | \" + syminfo.ticker)\n",
    "plot(mean, \"Mean\")\n",
);

fn strategy_options() -> StandaloneWasmOptions {
    StandaloneWasmOptions {
        // A strategy source designates nothing — the broker's
        // posture lane is the signal source.
        signal_plot: None,
        module_type: StandaloneWasmModuleType::Strategy,
    }
}

/// The build the JavaScript binding makes: the signal slice on.
fn sliced() -> StandaloneWasmBuildConfig {
    StandaloneWasmBuildConfig {
        signal_slice: true,
        ..StandaloneWasmBuildConfig::default()
    }
}

fn compile_strategy_sliced(source: &str) -> Vec<u8> {
    compile_standalone_wasm_program_with_config(
        &compile_csil(source),
        &strategy_options(),
        &sliced(),
    )
    .expect("sliced strategy standalone WebAssembly emit")
}

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

fn compile_strategy_standalone_wasm(source: &str) -> Vec<u8> {
    compile_standalone_wasm_program(
        &compile_csil(source),
        &StandaloneWasmOptions {
            // A strategy source designates nothing — the broker's
            // posture lane is the signal source.
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("strategy standalone WebAssembly emit")
}

// ---------------------------------------------------------------------------
// Market data: ~500 daily bars with pronounced multi-bar trends and
// reversals so channel breakouts, stops, and mean-reversion entries all
// trigger. Times are UTC-midnight-aligned (bar 0 = 2022-01-07T00:00Z) so
// the risk-session key advances daily on both sides.

const BAR0_TIME: i64 = 1_641_600_000_000;
const MS_PER_DAY: i64 = 86_400_000;
const BARS: usize = 500;

struct Bar {
    time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

fn synthetic_bars() -> Vec<Bar> {
    // Piecewise ~110-bar trend legs (±0.5–0.75/day, ±55–80 points per leg)
    // with a fast wobble: strong enough for 55-bar breakouts, 50/200
    // crossovers, ATR-band flips behind a 200-bar regime gate, and stop
    // crossings at the reversals.
    let mut base = 130.0f64;
    let mut out = Vec::with_capacity(BARS);
    for i in 0..BARS {
        let t = i as f64;
        base += match (i / 110) % 4 {
            0 => 0.65,
            1 => -0.55,
            2 => 0.75,
            _ => -0.5,
        };
        let wobble = 3.0 * (t / 4.5).sin();
        let close = base + wobble + 1.0 * (t * 1.3).cos();
        let open = base + wobble - 0.9 * (t * 0.7).sin();
        let high = open.max(close) + 1.5 + 0.5 * (t * 0.9).sin().abs();
        let low = open.min(close) - 1.5 - 0.5 * (t * 1.1).cos().abs();
        // Periodic weak and strong closes inside the bar's range, so
        // strategies that enter near each bar's low and exit near its high
        // (ibs < 0.1 entries, ibs > 0.9 exits) actually trade on this feed.
        let close = match i % 13 {
            0 => low + 0.03 * (high - low),
            4 => high - 0.03 * (high - low),
            _ => close,
        };
        out.push(Bar {
            time: BAR0_TIME + (i as i64) * MS_PER_DAY,
            open,
            high,
            low,
            close,
            volume: 1_000.0 + (i % 89) as f64 * 10.0,
        });
    }
    out
}

fn market_data(bars: &[Bar]) -> MarketData {
    let mut data = MarketData::default();
    for b in bars {
        data.open.push(b.open);
        data.high.push(b.high);
        data.low.push(b.low);
        data.close.push(b.close);
        data.volume.push(b.volume);
        data.time.push(b.time);
    }
    data
}

fn wire(bars: &[Bar]) -> String {
    let mut out = String::from("{\"version\":1,\"params\":{},\"bars\":[");
    for (i, b) in bars.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"time\":{},\"open\":{:?},\"high\":{:?},\"low\":{:?},\"close\":{:?},\"volume\":{:?}}}",
            b.time, b.open, b.high, b.low, b.close, b.volume
        ));
    }
    out.push_str("]}");
    out
}

// ---------------------------------------------------------------------------
// The standalone WebAssembly host (wasmi, empty linker), as in tests/standalone_wasm.rs.

struct StandaloneWasm {
    store: wasmi::Store<()>,
    instance: wasmi::Instance,
    memory: wasmi::Memory,
}

impl StandaloneWasm {
    fn new(wasm: &[u8]) -> StandaloneWasm {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, wasm).expect("wasmi accepts the module");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("instantiate with an empty linker");
        let memory = instance
            .get_memory(&store, "memory")
            .expect("memory export");
        StandaloneWasm {
            store,
            instance,
            memory,
        }
    }

    fn evaluate(&mut self, input: &str) -> Result<Vec<u8>, String> {
        self.evaluate_bytes(input.as_bytes())
    }

    fn evaluate_bytes(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&self.store, "alloc")
            .expect("alloc export");
        let ptr = alloc
            .call(&mut self.store, input.len() as i32)
            .expect("alloc call") as u32;
        assert_ne!(ptr, 0, "alloc must not fail");
        self.memory.data_mut(&mut self.store)[ptr as usize..ptr as usize + input.len()]
            .copy_from_slice(input);
        let packed = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, "evaluate_indicator_cabi")
            .expect("evaluate export")
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("evaluate call") as u64;
        if packed == 0 {
            let packed = self
                .instance
                .get_typed_func::<(), i64>(&self.store, "last_error")
                .expect("last_error export")
                .call(&mut self.store, ())
                .expect("last_error call") as u64;
            assert_ne!(packed, 0, "a 0 return must set last_error");
            let (eptr, elen) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
            let mem = self.memory.data(&self.store);
            return Err(String::from_utf8(mem[eptr..eptr + elen].to_vec()).expect("UTF-8"));
        }
        let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        Ok(mem[out_ptr..out_ptr + out_len].to_vec())
    }

    /// Current linear-memory size in bytes (the growth axis of the
    /// broker-reclaim pin below; wasm memory never shrinks, so equality over
    /// many cycles means the module stopped allocating new pages).
    fn memory_bytes(&self) -> usize {
        self.memory.data(&self.store).len()
    }

    /// One evaluation under the full documented host protocol: alloc →
    /// write → evaluate → copy OUT → dealloc(out) → dealloc(in) — exactly
    /// the sequence a resident host drives between
    /// calls. [`StandaloneWasm::evaluate`] deliberately skips the deallocs (a leaky host
    /// must also be safe); this variant exists so the reclaim pin measures
    /// the module's own retention, not the harness's.
    fn evaluate_reclaiming(&mut self, input: &str) -> Vec<u8> {
        self.evaluate_bytes_reclaiming(input.as_bytes())
    }

    fn evaluate_bytes_reclaiming(&mut self, input: &[u8]) -> Vec<u8> {
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&self.store, "alloc")
            .expect("alloc export");
        let ptr = alloc
            .call(&mut self.store, input.len() as i32)
            .expect("alloc call") as u32;
        assert_ne!(ptr, 0, "alloc must not fail");
        self.memory.data_mut(&mut self.store)[ptr as usize..ptr as usize + input.len()]
            .copy_from_slice(input);
        let packed = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, "evaluate_indicator_cabi")
            .expect("evaluate export")
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("evaluate call") as u64;
        if packed == 0 {
            let error = self
                .instance
                .get_typed_func::<(), i64>(&self.store, "last_error")
                .unwrap()
                .call(&mut self.store, ())
                .unwrap() as u64;
            let (p, n) = ((error >> 32) as usize, (error & 0xffff_ffff) as usize);
            panic!(
                "evaluation failed: {}",
                String::from_utf8_lossy(&self.memory.data(&self.store)[p..p + n])
            );
        }
        let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let out = self.memory.data(&self.store)[out_ptr..out_ptr + out_len].to_vec();
        let dealloc = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&self.store, "dealloc")
            .expect("dealloc export");
        dealloc
            .call(&mut self.store, (out_ptr as i32, out_len as i32))
            .expect("dealloc(out)");
        dealloc
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("dealloc(in)");
        out
    }
}

/// One OUT signal under posture-events: `(time, direction ∈ {1,0,-1}, price)`.
type Signal = (i64, i64, f64);

/// Parse an OUT document, asserting the three-field shape, widened direction
/// domain, and flat-event well-formedness.
fn posture_signals(out: &[u8], bars: &[Bar], slice_end: usize) -> Vec<Signal> {
    let doc: serde_json::Value = serde_json::from_slice(out).expect("OUT is valid JSON");
    assert_eq!(doc["version"], 1, "OUT version");
    let signals: Vec<Signal> = doc["signals"]
        .as_array()
        .expect("signals array")
        .iter()
        .map(|s| {
            let element = s.as_object().expect("signal object");
            assert_eq!(element.len(), 3, "exactly {{time, direction, price}}: {s}");
            let direction = s["direction"].as_i64().expect("integer direction");
            assert!(
                matches!(direction, -1..=1),
                "posture-events direction must be 1, -1, or 0, got {direction}"
            );
            (
                s["time"].as_i64().expect("integer time"),
                direction,
                s["price"].as_f64().expect("price"),
            )
        })
        .collect();

    // The first event is nonzero and a 0 event never follows a 0
    // event (closing nothing is a contract violation).
    if let Some(first) = signals.first() {
        assert_ne!(first.1, 0, "first event must be nonzero");
    }
    for pair in signals.windows(2) {
        assert!(!(pair[0].1 == 0 && pair[1].1 == 0), "0 event after 0 event");
        assert!(pair[0].0 < pair[1].0, "times strictly increasing");
    }
    // Each time matches an evaluated-window bar; the price is
    // that bar's close exactly (ryu shortest-roundtrip survives serde's
    // float_roundtrip parse bit-for-bit).
    for (time, _, price) in &signals {
        let bar = bars[..slice_end]
            .iter()
            .find(|b| b.time == *time)
            .expect("signal time matches an evaluated bar");
        assert_eq!(*price, bar.close, "price is the event bar's close");
    }
    signals
}

/// Reconstruct the per-bar posture lane from sparse posture events — the
/// inverse of edge detection, lossless by construction (posture is constant
/// between events).
fn posture_lane(signals: &[Signal], bars: &[Bar]) -> Vec<i8> {
    let mut lane = vec![0i8; bars.len()];
    let mut cur = 0i8;
    let mut next = 0usize;
    for (i, b) in bars.iter().enumerate() {
        if next < signals.len() && signals[next].0 == b.time {
            cur = signals[next].1 as i8;
            next += 1;
        }
        lane[i] = cur;
    }
    assert_eq!(next, signals.len(), "every signal lands on exactly one bar");
    lane
}

/// `sign(position at bar close)` per bar from the interpreter.
///
/// Deliberately NOT `plot(strategy.position_size)`: the interpreter's position
/// getter reads the pre-body snapshot, so under `process_orders_on_close` a same-bar
/// close-pass fill is invisible to it until the next bar — while the standalone WebAssembly
/// posture lane samples the broker *after* the close pass, so the event bar is
/// the bar on which the position actually changed. The exact
/// end-of-bar position for every fill model is the closed-trade ledger: a
/// trade's position exists from the end of its entry (fill) bar through the
/// end of the bar before its exit (fill) bar, so its contribution is
/// `sign · qty` over `[entry_bar, exit_bar)`. The still-open position at
/// run end contributes from its own entry bar, read off an appended
/// `strategy.opentrades.entry_bar_index(0)` plot (na there = the lot opened
/// on the final bar's close pass, invisible to every snapshot).
fn interpreter_posture(source: &str, data: &MarketData) -> Vec<i8> {
    let with_plot = format!(
        "{source}\nplot(strategy.opentrades == 0 ? na : \
         strategy.opentrades.entry_bar_index(0), 'OpenEntryRef')\n"
    );
    let program = compile_csil(&with_plot);
    let outcome = run(&program, data, &RunOptions::default());
    assert!(
        outcome.errors.is_empty(),
        "interpreter run failed: {:?}",
        outcome.errors
    );
    let strategy = outcome.strategy.as_ref().expect("strategy outcome");
    let n = data.bars();
    let mut net = vec![0.0f64; n];
    for trade in &strategy.trades {
        assert!(trade.sign != 0.0, "closed trade carries a direction sign");
        let signed = trade.sign * trade.qty;
        for cell in net
            .iter_mut()
            .take(trade.exit_bar as usize)
            .skip(trade.entry_bar as usize)
        {
            *cell += signed;
        }
    }
    let position = strategy.position.size;
    if position != 0.0 {
        let open_entry = match outcome
            .plots
            .last()
            .expect("appended plot")
            .values
            .last()
            .expect("final bar")
        {
            Value::Float(e) => *e as usize,
            Value::Int(e) => *e as usize,
            // na: the open lot entered on the final bar's close pass.
            _ => n - 1,
        };
        for cell in net.iter_mut().skip(open_entry) {
            *cell += position;
        }
    }
    let lane: Vec<i8> = net
        .iter()
        .map(|v| {
            if *v > 0.0 {
                1
            } else if *v < 0.0 {
                -1
            } else {
                0
            }
        })
        .collect();
    // Reconciliation: the reconstructed final posture must agree with the
    // outcome's own open position (catches a mis-attributed open lot).
    let expected_last = if position > 0.0 {
        1
    } else if position < 0.0 {
        -1
    } else {
        0
    };
    assert_eq!(
        lane.last().copied().unwrap_or(0),
        expected_last,
        "interpreter reconstruction does not reconcile with the final position"
    );
    lane
}

// ---------------------------------------------------------------------------
// 1. Byte identity for indicator-sourced artifacts.

/// Baselines for the current output of each indicator artifact. If these move,
/// an indicator artifact's bytes moved with them: an isolation regression, not
/// a number to update casually. These hashes pin the current merged output.
const BASELINE: [(&str, &str, &str); 3] = [
    (
        "standalone-wasm-posture-strategy-type",
        "indicator(\"X\")\nplot(close > open ? 1 : -1)\n",
        "f9da2e363517384ad18138026911cf55521ae333208698a83ffeadbf72043abf",
    ),
    (
        "standalone-wasm-ta-strategy-type",
        "indicator(\"T\")\nplot(ta.ema(close, 14) > close ? 1 : -1)\n",
        "e296bf490736195811e0030cf0eab16851989fafd2749593a425065d522e09ce",
    ),
    (
        "standalone-wasm-indicator-type",
        "indicator(\"X\")\nplot(close > open ? 1 : -1)\n",
        "2bc75cb5ec44cc4b5ebe29b1a2a89b4289e92e3712eb48f3191677fa69561484",
    ),
];

#[test]
fn indicator_artifact_hashes_are_stable() {
    let mut identities = Vec::new();
    let mut expected_identities = Vec::new();
    for (name, source, expected) in BASELINE {
        let module_type = if name == "standalone-wasm-indicator-type" {
            StandaloneWasmModuleType::Indicator
        } else {
            StandaloneWasmModuleType::Strategy
        };
        let wasm = compile_standalone_wasm_program(
            &compile_csil(source),
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type,
            },
        )
        .expect("indicator-sourced standalone WebAssembly emit");
        identities.push((name, sha256_hex(&wasm)));
        expected_identities.push((name, expected.to_owned()));
    }

    // The standalone (chart-interface) tier splices the base/ta blobs; pin both.
    // These are chart modules: each carries `bar_step` and its chart fuel
    // windows (`body/chart_fuel.rs`).
    let pure = compile_csil("indicator(\"P\")\nplot(math.exp(close / 1000))\n");
    let standalone = pinewasm_aot::compile_standalone_program(&pure).expect("standalone");
    identities.push(("standalone-base", sha256_hex(&standalone.wasm)));
    expected_identities.push((
        "standalone-base",
        "c70948a40b4a4a15ad798c452c17e510c94b14ee75f32703bcb45ab15d5eac1c".to_owned(),
    ));
    let ta = compile_csil("indicator(\"T\")\nplot(ta.ema(close, 14) > close ? 1 : -1)\n");
    let ta_standalone = pinewasm_aot::compile_standalone_program(&ta).expect("ta standalone");
    identities.push(("standalone-ta", sha256_hex(&ta_standalone.wasm)));
    expected_identities.push((
        "standalone-ta",
        "1112061f35599ec92df25cf0f028506d2ac128fc2b9abcc1a15718cd1ddf61ad".to_owned(),
    ));
    assert_eq!(identities, expected_identities, "artifact bytes moved");
}

// ---------------------------------------------------------------------------
// 2. The differential smoke gate.

#[test]
fn aot_posture_matches_the_interpreter_position_sign_bar_for_bar() {
    let bars = synthetic_bars();
    let data = market_data(&bars);
    // The security scripts group bars, so the request names the exchange:
    // the interpreter's default, UTC with no session declared.
    let input = wire(&bars).replacen(
        "\"bars\":",
        "\"timeframe\":\"1D\",\"timezone\":\"UTC\",\"session_hours\":\"\",\"bars\":",
        1,
    );
    let mut saw_zero_event = false;
    let mut saw_nonzero_event = false;

    for (name, source) in [
        // stop= exits: intrabar stop fills follow PineWASM's four-tick broker walk.
        ("order-primitives", ORDER_PRIMITIVES),
        // process_orders_on_close: the same-bar close-tick fill pass.
        ("process-on-close", PROCESS_ON_CLOSE),
        // A second stop=-exit script, regime-gated.
        ("stop-exit", STOP_EXIT),
        ("crossover", CROSSOVER),
        // A same-symbol higher-timeframe request gating the orders.
        ("security-filter", SECURITY_FILTER),
        ("security-expression", SECURITY_EXPRESSION),
        // Arrays and objects deciding the orders: the native heap in the
        // broker's arena band.
        ("array-signal", ARRAY_SIGNAL),
        ("object-signal", OBJECT_SIGNAL),
        // The same, compiled from its slice: display code erased.
        ("display-around-orders", DISPLAY_AROUND_ORDERS),
    ] {
        let wasm = compile_strategy_sliced(source);
        pinewasm_aot::audit_standalone_wasm(&wasm).unwrap_or_else(|e| {
            panic!("{name}: emitted bytes fail the standalone WebAssembly audit: {e}")
        });
        let out = StandaloneWasm::new(&wasm)
            .evaluate(&input)
            .unwrap_or_else(|e| panic!("{name}: evaluate failed: {e}"));
        let signals = posture_signals(&out, &bars, bars.len());
        let aot = posture_lane(&signals, &bars);
        let expected = interpreter_posture(source, &data);
        assert_eq!(expected.len(), bars.len(), "{name}: interpreter bar count");
        for (bar, (aot_p, ref_p)) in aot.iter().zip(&expected).enumerate() {
            assert_eq!(
                aot_p, ref_p,
                "{name}: posture diverges at bar {bar} (AOT {aot_p} vs interpreter {ref_p})"
            );
        }
        saw_zero_event |= signals.iter().any(|s| s.1 == 0);
        saw_nonzero_event |= !signals.is_empty();
        assert!(
            !signals.is_empty(),
            "{name}: the synthetic feed must produce at least one event"
        );
    }

    assert!(saw_nonzero_event, "fixtures produced no events at all");
    assert!(
        saw_zero_event,
        "fixtures produced no close-to-flat (0) event — the flat capability is untested"
    );
}

#[test]
fn a_strategy_that_compiled_whole_keeps_its_bytes() {
    // The slice and the native heap are a second attempt, made only for a
    // strategy the whole-program compile refuses. Everything that built
    // before builds to the same module.
    for source in [
        ORDER_PRIMITIVES,
        PROCESS_ON_CLOSE,
        STOP_EXIT,
        CROSSOVER,
        SECURITY_FILTER,
    ] {
        let program = compile_csil(source);
        let unsliced = compile_standalone_wasm_program_with_config(
            &program,
            &strategy_options(),
            &StandaloneWasmBuildConfig {
                signal_slice: false,
                ..StandaloneWasmBuildConfig::default()
            },
        )
        .expect("whole program");
        assert_eq!(compile_strategy_standalone_wasm(source), unsliced);
    }
}

#[test]
fn display_code_around_orders_is_erased_and_plots_are_kept() {
    let program = compile_csil(DISPLAY_AROUND_ORDERS);
    // Without the slice the calendar roll, the table text, the label and
    // the order text all need a host.
    let error = compile_standalone_wasm_program_with_config(
        &program,
        &strategy_options(),
        &StandaloneWasmBuildConfig {
            signal_slice: false,
            ..StandaloneWasmBuildConfig::default()
        },
    )
    .expect_err("the whole program needs a host");
    match error {
        AotError::StrategyFeatureUnsupported { feature } => {
            for name in ["month", "str.format", "str.tostring", "syminfo.ticker"] {
                assert!(feature.contains(name), "{name} missing from: {feature}");
            }
        }
        other => panic!("expected StrategyFeatureUnsupported, got {other:?}"),
    }

    // A strategy reports every plot on request, so plot values stay live:
    // the full results carry the interpreter's own numbers.
    let bars = synthetic_bars();
    let data = market_data(&bars);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let mut input: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
    input["include_results"] = true.into();
    let wasm = compile_strategy_sliced(DISPLAY_AROUND_ORDERS);
    let out = StandaloneWasm::new(&wasm)
        .evaluate(&input.to_string())
        .expect("full results");
    let actual: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let plotted = actual["plots"][0].as_array().expect("plot lane");
    assert_eq!(plotted.len(), bars.len());
    for (bar, (got, want)) in plotted.iter().zip(&expected.plots[0].values).enumerate() {
        let want = match want {
            Value::Float(v) => Some(*v),
            Value::Int(v) => Some(*v as f64),
            _ => None,
        };
        assert_eq!(got.as_f64(), want, "plot value at bar {bar}");
    }
    let strategy = expected.strategy.expect("strategy outcome");
    assert!(!strategy.trades.is_empty(), "the feed must trade");
    assert_eq!(
        actual["strategy"]["trades"].as_array().unwrap().len(),
        strategy.trades.len()
    );
    assert_eq!(
        actual["strategy"]["netProfit"].as_f64(),
        Some(strategy.net_profit)
    );
}

#[test]
fn a_resident_heap_strategy_reclaims_its_memory_every_evaluation() {
    // A strategy's heap allocates from the arena band its broker uses, and
    // the next run's broker reset rewinds the band whole. OBJECT_SIGNAL
    // makes a fresh array on every bar, so a heap that kept a region of its
    // own, or a band that was not rewound, would grow on every call.
    let bars = synthetic_bars();
    let input = wire(&bars);
    let mut host = StandaloneWasm::new(&compile_strategy_standalone_wasm(OBJECT_SIGNAL));
    let first = host.evaluate_reclaiming(&input);
    assert!(!posture_signals(&first, &bars, bars.len()).is_empty());
    assert_eq!(host.evaluate_reclaiming(&input), first);
    let settled = host.memory_bytes();
    for trial in 0..100 {
        assert_eq!(
            host.evaluate_reclaiming(&input),
            first,
            "resident heap strategy drifted at trial {trial}"
        );
        assert_eq!(
            host.memory_bytes(),
            settled,
            "linear memory grew at trial {trial}"
        );
    }
}

#[test]
fn a_standalone_strategy_collects_its_heap_where_the_interpreter_does() {
    // OBJECT_SIGNAL leaves a three-element array behind on every bar. The
    // interpreter collects those at bar boundaries; a module that never
    // collected spent a 60-element budget around bar 20 of 500 and failed
    // the evaluation the interpreter completes. The module now finishes
    // under exactly the budgets the interpreter finishes under, with the
    // interpreter's postures, and fails under the rest.
    let program = compile_csil(OBJECT_SIGNAL);
    let bars = synthetic_bars();
    let data = market_data(&bars);
    let input = wire(&bars);
    let posture = interpreter_posture(OBJECT_SIGNAL, &data);
    let mut finished = 0;
    for budget in (1..=60).chain([1_000]) {
        let wasm = compile_standalone_wasm_program_with_config(
            &program,
            &strategy_options(),
            &StandaloneWasmBuildConfig {
                max_collection_elements: budget,
                ..sliced()
            },
        )
        .expect("strategy standalone WebAssembly emit");
        let reference = run(
            &program,
            &data,
            &RunOptions {
                max_collection_elements: budget as usize,
                ..RunOptions::default()
            },
        );
        match (
            reference.errors.first(),
            StandaloneWasm::new(&wasm).evaluate(&input),
        ) {
            (None, Ok(out)) => {
                let signals = posture_signals(&out, &bars, bars.len());
                assert_eq!(posture_lane(&signals, &bars), posture, "budget {budget}");
                finished += 1;
            }
            (Some(error), Err(message)) => {
                assert_eq!(
                    error.kind,
                    pinewasm_vm::RuntimeErrorKind::CollectionLimitExceeded,
                    "budget {budget}"
                );
                assert!(
                    message.contains("native heap error"),
                    "budget {budget}: {message}"
                );
            }
            (vm, module) => panic!(
                "budget {budget}: the interpreter {} and the module {}",
                vm.map_or("finished".to_string(), |error| format!(
                    "stopped: {error:?}"
                )),
                module.map_or_else(
                    |error| format!("stopped: {error}"),
                    |_| "finished".to_string()
                )
            ),
        }
    }
    assert!(finished > 1 && finished < 61, "{finished}");
}

// ---------------------------------------------------------------------------
// 3. Conformance: metadata, signal ownership, and feature gates.

fn custom_section(wasm: &[u8], name: &str) -> Option<Vec<u8>> {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(reader) = payload.expect("module parses") {
            if reader.name() == name {
                return Some(reader.data().to_vec());
            }
        }
    }
    None
}

#[test]
fn strategy_modules_carry_the_posture_events_signal_metadata() {
    let wasm = compile_strategy_standalone_wasm(ORDER_PRIMITIVES);
    let metadata = custom_section(&wasm, "module_metadata").expect("module_metadata section");
    let doc: serde_json::Value = serde_json::from_slice(&metadata).expect("metadata JSON");
    assert_eq!(doc["type"], "strategy");
    assert_eq!(doc["abi_version"], 1);
    assert_eq!(doc["signal"]["source"], "strategy");
    assert_eq!(doc["signal"]["semantics"], "posture-events");
    assert_eq!(doc["signal"]["price_source"], "close");
}

#[test]
fn a_strategy_source_refuses_plot_designation() {
    // The broker posture lane is the signal source; naming a plot is a
    // refusal, never a silently-ignored field.
    let program = compile_csil(ORDER_PRIMITIVES);
    let error = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: Some(0),
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect_err("plot designation on a strategy source");
    assert!(
        matches!(error, AotError::StandaloneWasmNoDesignatedPlot(_)),
        "{error:?}"
    );
}

#[test]
fn strategy_feature_gates_refuse_by_name() {
    let feature_of = |source: &str| -> String {
        let error = compile_standalone_wasm_program(
            &compile_csil(source),
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Strategy,
            },
        )
        .expect_err("gated feature must refuse");
        match error {
            AotError::StrategyFeatureUnsupported { feature } => feature,
            other => panic!("expected StrategyFeatureUnsupported, got {other:?}"),
        }
    };

    assert_eq!(
        feature_of("strategy(\"S\", currency = currency.USDT)\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n"),
        "currency"
    );

    assert_eq!(
        feature_of(
            "strategy(\"S\", calc_on_order_fills = true)\n\
             strategy.entry(\"L\", strategy.long)\nplot(close)\n"
        ),
        "calc_on_order_fills"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\", calc_on_every_history_tick = true)\n\
             strategy.entry(\"L\", strategy.long)\nplot(close)\n"
        ),
        "calc_on_every_history_tick"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\", calc_bars_count = 100)\n\
             strategy.entry(\"L\", strategy.long)\nplot(close)\n"
        ),
        "calc_bars_count"
    );
    // Numeric trade accessors lower natively; only the string-valued ids
    // and comments still refuse, each by its own name.
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(str.length(strategy.closedtrades.exit_id(0)))\n"
        ),
        "strategy.closedtrades.exit_id"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(str.length(strategy.opentrades.entry_comment(0)))\n"
        ),
        "strategy.opentrades.entry_comment"
    );
    compile_strategy_standalone_wasm(
        "strategy(\"S\")\n\
         plot(strategy.opentrades.entry_price(0))\n\
         plot(strategy.closedtrades.exit_bar_index(0))\n",
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(strategy.convert_to_account(close))\n"
        ),
        "strategy.convert_to_account"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(strategy.convert_to_symbol(close))\n"
        ),
        "strategy.convert_to_symbol"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(strategy.default_entry_qty(close))\n"
        ),
        "strategy.default_entry_qty"
    );
    // The two string-typed snapshot getters have no native string story and
    // refuse by their own names (analysis.rs `strategy_snap_field` None arm).
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(str.length(strategy.account_currency))\n"
        ),
        "strategy.account_currency"
    );
    assert_eq!(
        feature_of(
            "strategy(\"S\")\n\
             plot(str.length(strategy.position_entry_name))\n"
        ),
        "strategy.position_entry_name"
    );

    // calc_on_every_tick is accepted-and-inert: it only affects realtime
    // ticks, which standalone WebAssembly evaluation does not have.
    compile_strategy_standalone_wasm(
        "strategy(\"S\", calc_on_every_tick = true)\n\
         strategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
}

#[test]
fn strategy_host_features_are_named_in_pine_not_in_csil_opcodes() {
    // The `host.vm` catch-all publishes its `feature` string verbatim
    // to a caller as something to find in their own script. It used to
    // be `format!("{instr:?}")`, so ordinary Pine produced "CallCollection",
    // "NewObject", "CallPure, StrConcat". Those name nothing anyone wrote.
    //
    // This pins the whole class by construct. The rule every case encodes:
    // the string must be Pine a reader can search for, never a Rust variant.
    //
    // Each case feeds the construct to an order. Code that can reach no
    // order and no plot value is erased before this refusal is decided
    // (`display_code_around_orders_is_erased_and_plots_are_kept`), so what
    // is named is what the orders need.
    let options = StandaloneWasmOptions {
        signal_plot: None,
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let feature_of = |body: &str| -> String {
        let source = format!("strategy(\"S\")\n{body}");
        let error = compile_standalone_wasm_program_with_config(
            &compile_csil(&source),
            &options,
            &sliced(),
        )
        .expect_err("host-site feature must refuse");
        match error {
            AotError::StrategyFeatureUnsupported { feature } => feature,
            other => panic!("expected StrategyFeatureUnsupported, got {other:?}"),
        }
    };
    let gated =
        |value: &str| format!("{value}if x > 1\n    strategy.entry(\"L\", strategy.long)\n");

    // An array operation the native heap has no lowering for, was
    // "CallCollection".
    assert_eq!(
        feature_of(&gated(
            "xs = array.from(close, open)\nx = array.median(xs)\n"
        )),
        "array.from, array.median"
    );
    // A drawing read back into the signal, was "NewObject".
    assert_eq!(
        feature_of(&gated(
            "t = label.new(bar_index, close, \"x\")\nx = label.get_y(t)\n"
        )),
        "label, user-defined type fields"
    );
    // A built string as an order id, was "CallPure, StrConcat".
    assert_eq!(
        feature_of("strategy.entry(\"L\" + str.tostring(bar_index), strategy.long)\n"),
        "str.tostring, string concatenation (+)"
    );
    // Comments are observable in the ledger. Unsupported string work must
    // refuse explicitly, rather than silently removing the comment.
    for body in [
        "strategy.close(\"L\", comment = \"Exit \" + str.tostring(bar_index))\n",
        "strategy.exit(\"X\", \"L\", profit = 1, comment_profit = str.tostring(close))\n",
    ] {
        let feature = feature_of(body);
        assert!(feature.contains("str.tostring"), "{feature}");
    }
    // Other-symbol data, was "ReadSecurity".
    assert_eq!(
        feature_of(&gated("x = request.security(\"AAPL\", \"D\", close)\n")),
        "request.security"
    );
    // A calendar builtin already named itself; a plot value is kept, so it
    // is named there too.
    assert_eq!(feature_of(&gated("x = month\n")), "month");
    assert_eq!(
        feature_of("strategy.entry(\"L\", strategy.long)\nplot(month)\n"),
        "month"
    );

    // The same constructs where nothing they compute reaches an order or a
    // plot value no longer refuse at all: unused arrays, a retained label,
    // built alert text, and the event streams.
    for body in [
        "var float[] xs = array.new_float(0)\narray.push(xs, close)\n",
        "var label t = na\nt := label.new(bar_index, close, \"x\")\n",
        "strategy.exit(\"X\", \"L\", profit = 1, alert_message = \"n=\" + str.tostring(bar_index))\n",
        "alert(\"hi\", alert.freq_once_per_bar)\n",
        "log.info(\"hi\")\n",
        "xs = array.from(close, open)\nlabel.new(bar_index, close, str.tostring(array.stdev(xs)))\n",
    ] {
        let source =
            format!("strategy(\"S\")\nstrategy.entry(\"L\", strategy.long)\n{body}plot(close)\n");
        compile_standalone_wasm_program_with_config(&compile_csil(&source), &options, &sliced())
            .unwrap_or_else(|error| panic!("{body:?} must build: {error:?}"));
    }

    // The invariant, stated as a check rather than as prose: no refusal in
    // this class may contain a Rust CSIL variant name. `CsilInstr` variants are
    // UpperCamelCase with no `.` or space; every Pine name has one or is
    // lowercase.
    for body in [
        gated("xs = array.from(close, open)\nx = array.median(xs)\n"),
        gated("t = label.new(bar_index, close, \"x\")\nx = label.get_y(t)\n"),
        gated("x = request.security(\"AAPL\", \"D\", close)\n"),
    ] {
        let feature = feature_of(&body);
        for name in feature.split(", ") {
            assert!(
                !name
                    .chars()
                    .next()
                    .expect("non-empty feature name")
                    .is_uppercase(),
                "`{name}` reads as a CSIL variant name, not as Pine (from: {body:?})"
            );
        }
    }
}

#[test]
fn a_resident_strategy_instance_is_byte_stable_across_a_thousand_evaluations() {
    // Resident-instance statelessness at the strict baseline (the indicator
    // twin lives in tests/standalone_wasm.rs): the broker is rebuilt fresh per run
    // (`pinewasm_rt_broker_init`) and its allocations land in an arena the next scan
    // rewinds, so a thousand evaluations on one resident instance are
    // byte-identical. A 120-bar window keeps the thousand-fold loop fast;
    // the short-window crossover churns through entries AND close-to-flat
    // exits inside it, so the pinned bytes carry a real posture-events
    // series — zeros included — not the empty document.
    let bars: Vec<Bar> = synthetic_bars().into_iter().take(120).collect();
    let input = wire(&bars);
    // The feed closes on its low every 13th bar and near its high four bars
    // later (see `synthetic_bars`), so this close-near-extremes loop
    // round-trips roughly every 13 bars from bar 0 — no warm-up.
    let wasm = compile_strategy_standalone_wasm(
        "strategy(\"Churn\")\n\
         range_ = high - low\n\
         ibs = range_ == 0 ? 0.5 : (close - low) / range_\n\
         if ibs < 0.1 and strategy.position_size == 0\n\
         \tstrategy.entry(\"L\", strategy.long)\n\
         if ibs > 0.9 and strategy.position_size > 0\n\
         \tstrategy.close(\"L\")\n\
         plot(close)\n",
    );
    let mut host = StandaloneWasm::new(&wasm);
    let first = host.evaluate(&input).expect("first evaluation");
    assert!(
        !posture_signals(&first, &bars, bars.len()).is_empty(),
        "the stability window must contain trades for the pin to mean much"
    );
    for trial in 0..1_000 {
        assert_eq!(
            host.evaluate(&input).expect("repeat evaluation"),
            first,
            "resident strategy instance drifted at trial {trial}"
        );
    }
}

#[test]
fn a_resident_strategy_instance_reclaims_broker_memory_every_evaluation() {
    // The regression this pins: the emitted glue LIFO-drops the input JSON
    // before `run()`, so `pinewasm_rt_broker_init` allocated the broker at the arena
    // band's base — and the NEXT call's input was then bump-allocated on top
    // of that dead state, which nothing ever reclaimed (`standalone_wasm_scan` only trims
    // above the live input). A resident instance leaked the entire per-run
    // broker heap every evaluation — measured ~33 pages/cycle on a 2000-bar
    // feed — and hit the declared 4096-page memory maximum mid-study, while
    // the byte-stability pin above stayed green (it compares output bytes
    // only). The fix: `pinewasm_rt_broker_init` forgets the stale broker/postures and
    // rewinds the whole band to its floor before building the fresh broker.
    //
    // Full 500-bar feed and the full host protocol (dealloc(out) then
    // dealloc(in), as a resident standalone WebAssembly host drives it): after a
    // settling cycle, linear memory must stop growing entirely — wasm memory
    // never shrinks, so byte-size equality over 200 cycles IS the reclaim.
    let bars = synthetic_bars();
    let input = wire(&bars);
    let wasm = compile_strategy_standalone_wasm(
        "strategy(\"Churn\")\n\
         range_ = high - low\n\
         ibs = range_ == 0 ? 0.5 : (close - low) / range_\n\
         if ibs < 0.1 and strategy.position_size == 0\n\
         \tstrategy.entry(\"L\", strategy.long)\n\
         if ibs > 0.9 and strategy.position_size > 0\n\
         \tstrategy.close(\"L\")\n\
         plot(close)\n",
    );
    let mut host = StandaloneWasm::new(&wasm);
    let first = host.evaluate_reclaiming(&input);
    assert!(
        !posture_signals(&first, &bars, bars.len()).is_empty(),
        "the reclaim window must contain trades for the pin to mean much"
    );
    // The second call's peak legitimately exceeds the first's once: until its
    // `pinewasm_rt_broker_init` rewinds the band, memory holds the previous run's
    // (dead) broker AND the new input side by side. From then on the layout
    // repeats exactly, so the size after call 2 is the fixed point.
    assert_eq!(
        host.evaluate_reclaiming(&input),
        first,
        "second call drifted"
    );
    let settled = host.memory_bytes();
    for trial in 0..200 {
        assert_eq!(
            host.evaluate_reclaiming(&input),
            first,
            "resident strategy instance drifted at trial {trial}"
        );
        assert_eq!(
            host.memory_bytes(),
            settled,
            "linear memory grew at trial {trial}: the broker band was not reclaimed"
        );
    }
}

#[test]
fn a_strategy_that_never_trades_serializes_the_empty_document() {
    // `signals: []` is a valid success, byte-exactly the canonical
    // empty OUT document — never an error, never a null.
    let bars = synthetic_bars();
    let out = StandaloneWasm::new(&compile_strategy_standalone_wasm(
        "strategy(\"Idle\")\nplot(close)\n",
    ))
    .evaluate(&wire(&bars))
    .expect("evaluate");
    assert_eq!(out, b"{\"version\":1,\"signals\":[]}");
}

#[test]
fn strategy_out_satisfies_the_posture_event_contract() {
    // The posture-event conformance pin over a synthetic strategy fixture:
    // the direction domain, ordering and bar matching, close pricing, and
    // flat-event well-formedness are all asserted
    // inside `posture_signals`; the cardinality corollaries are pinned here.
    let bars = synthetic_bars();
    let out = StandaloneWasm::new(&compile_strategy_standalone_wasm(ORDER_PRIMITIVES))
        .evaluate(&wire(&bars))
        .expect("evaluate");
    let signals = posture_signals(&out, &bars, bars.len());
    assert!(
        !signals.is_empty(),
        "the synthetic fixture trades on this feed"
    );
    assert!(
        signals.len() <= bars.len(),
        "at most one event per bar implies len(signals) <= len(bars)"
    );
    let mut seen = std::collections::BTreeSet::new();
    for (time, _, _) in &signals {
        assert!(seen.insert(*time), "two events share a bar");
    }
    assert!(
        signals.iter().any(|s| s.1 == 0),
        "the flat capability must appear for this contract pin"
    );
}

/// The broker's published const pool must be the POST-projection pool.
///
/// `StandaloneWasmProjection::prepare` APPENDS constants for a finite `input.string`'s
/// options (projection.rs), and `compile_standalone_wasm_program` shadows `program` with
/// the projected clone before `emit_module` reaches `broker_config_bytes`.
/// That wiring is load-bearing and was asserted only in prose: publishing the
/// pre-projection pool instead can leave broad posture differentials green.
///
/// It stays invisible to those because they only ever use `input.string` to
/// SELECT A BRANCH. This uses one as an `CsilInstr::Order` STRING OPERAND, so
/// the chosen value exists in the pool only because the projection put it
/// there: the source pool is `["Alpha", "strategy.long", "", ...]`, and
/// `"Beta"` is appended. Under the pre-projection pool the out-of-range
/// handle resolves to `None` in `cell_str` (runtime/src/broker.rs), the exit's
/// `from_entry` becomes "every open entry", and the posture diverges.
///
/// Corollary worth stating where someone will read it: a posture
/// differential compares the posture SIGN only, so an id-resolution error
/// that changes quantities but not the net sign is structurally invisible to
/// it. This case is chosen to move the sign.
#[test]
fn the_broker_pool_is_the_projected_pool_not_the_source_pool() {
    let source = concat!(
        "strategy(\"POOL\", initial_capital = 10000, pyramiding = 2)\n",
        // Never a branch condition — only ever an order-id operand, and the
        // literal "Beta" appears NOWHERE else, so the only route to that
        // string is the handle the projection appends. Verified: the source
        // pool is ["Alpha", "strategy.long", "", "strategy.oca.none", "X"].
        "lot = input.string(\"Beta\", \"Lot\", options = [\"Zulu\", \"Beta\"])\n",
        "if close > ta.sma(close, 5)\n",
        "    strategy.entry(\"Alpha\", strategy.long)\n",
        "    strategy.entry(lot, strategy.long)\n",
        // Exits ONLY the `lot` entry, leaving `Alpha` open. A pool that
        // cannot resolve the handle passes `from_entry = na`, which the
        // broker reads as "every open entry" — flat instead of long.
        "strategy.exit(\"X\", lot, profit = 1)\n",
    );

    let wasm = compile_strategy_standalone_wasm(source);
    pinewasm_aot::audit_standalone_wasm(&wasm)
        .expect("still a zero-import standalone WebAssembly module");

    let bars = synthetic_bars();
    let out = StandaloneWasm::new(&wasm)
        .evaluate(&wire(&bars))
        .expect("evaluate");
    let signals = posture_signals(&out, &bars, bars.len());
    let expected = interpreter_posture(source, &market_data(&bars));
    let got = posture_lane(&signals, &bars);
    for (bar, (got, want)) in got.iter().zip(&expected).enumerate() {
        assert_eq!(
            got, want,
            "posture diverges at bar {bar} — the broker resolved its order \
             ids against the wrong const pool"
        );
    }
    assert!(
        got.iter().any(|p| *p != 0),
        "the fixture must actually trade, or it pins nothing"
    );
}

/// The crossover with the importless-indicator projection: a
/// `strategy()` source takes that projection too.  Neither mechanism may
/// swallow the other — the finite string option must reach the parameter
/// schema as an integer index and compare canonically, the label/table
/// chain must be erased, the broker must still own the signal, and the
/// posture must still equal the interpreter's bar for bar.
#[test]
fn a_strategy_takes_the_standalone_wasm_projection_without_losing_the_broker() {
    let source = concat!(
        "strategy(\"PROJECTED\", initial_capital = 10000)\n",
        "mode = input.string(\"Fast\", \"Mode\", options = [\"Fast\", \"Slow\"])\n",
        "basis = mode == \"Fast\" ? ta.sma(close, 5) : ta.sma(close, 20)\n",
        "if close > basis\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if close < basis\n",
        "    strategy.close(\"L\")\n",
    );

    let wasm = compile_strategy_standalone_wasm(source);
    pinewasm_aot::audit_standalone_wasm(&wasm)
        .expect("a projected strategy is still a zero-import standalone WebAssembly module");

    // Fallible chart effects are not erased around the broker either: the
    // table's cells and the `str.tostring` label text can fail in the
    // VM, so the same strategy with a table and a label keeps host
    // sites and fails closed. (The label's own constructor is certified and
    // erased, so the refusal names the string producer it still needs.)
    let charted = source.replace(
        "if close > basis\n",
        concat!(
            "var panel = table.new(position.top_right, 1, 1)\n",
            "table.cell(panel, 0, 0, str.tostring(basis) + \" basis\")\n",
            "label.new(bar_index, close, str.tostring(close))\n",
            "if close > basis\n",
        ),
    );
    let error = compile_standalone_wasm_program(
        &compile_csil(&charted),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect_err("fallible chart effects are not projected away");
    assert!(
        error
            .to_string()
            .contains("strategy feature unsupported for standalone WebAssembly: str.tostring, string concatenation (+), table, table cells"),
        "{error}"
    );

    // The schema view survives on the strategy path: the option list is
    // an int index over 0..n-1, not an omitted string.
    let schema: serde_json::Value = serde_json::from_slice(
        &custom_section(&wasm, pinewasm_aot::schema::PARAMETER_SCHEMA_SECTION)
            .expect("parameter_schema section"),
    )
    .expect("schema JSON");
    assert_eq!(
        schema,
        serde_json::json!([{
            "name": "mode",
            "label": "Mode",
            "type": "int",
            "default": 0,
            "min": 0,
            "max": 1,
            "step": 1
        }]),
        "the finite string option must reach standalone WebAssembly's schema as an index"
    );

    // Ours: the broker still owns the signal, and it still matches.
    let bars = synthetic_bars();
    let out = StandaloneWasm::new(&wasm)
        .evaluate(&wire(&bars))
        .expect("evaluate");
    let signals = posture_signals(&out, &bars, bars.len());
    assert!(
        !signals.is_empty(),
        "the projected strategy must still trade — an erased broker would be silent"
    );
    let expected = interpreter_posture(source, &market_data(&bars));
    for (bar, (got, want)) in posture_lane(&signals, &bars)
        .iter()
        .zip(&expected)
        .enumerate()
    {
        assert_eq!(
            got, want,
            "posture diverges at bar {bar} under the standalone WebAssembly projection"
        );
    }
}

#[test]
fn strategy_recompiles_are_byte_identical() {
    // The moduleSha256 story: same source, same bytes, every time.
    let first = compile_strategy_standalone_wasm(ORDER_PRIMITIVES);
    let second = compile_strategy_standalone_wasm(ORDER_PRIMITIVES);
    assert_eq!(
        first, second,
        "strategy standalone WebAssembly recompile is not reproducible"
    );
}

// ---------------------------------------------------------------------------
// A dependency-free SHA-256 (as in tests/standalone_wasm.rs), for the byte-identity pins.

fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}

#[test]
fn opt_in_full_results_match_native_vm_with_fractional_symbol_economics() {
    let source = concat!(
        "//@version=6\n",
        "strategy('Full results', initial_capital=1000.000049, default_qty_type=strategy.percent_of_equity, default_qty_value=100, commission_type=strategy.commission.percent, commission_value=1.0, process_orders_on_close=true)\n",
        "if bar_index == 0\n    strategy.entry('L', strategy.long)\n",
        "if bar_index == 1\n    strategy.close('L')\n",
        "plot(close, 'Price')\nplot(ta.sma(close, 2), 'Mean')\n"
    );
    let bars: Vec<Bar> = [3.0, 4.0, 5.0]
        .into_iter()
        .enumerate()
        .map(|(i, p)| Bar {
            time: BAR0_TIME + i as i64 * MS_PER_DAY,
            open: p,
            high: p,
            low: p,
            close: p,
            volume: 100.0,
        })
        .collect();
    let mut data = market_data(&bars);
    data.mintick = 0.00001;
    data.syminfo.mincontract = 0.01;
    let program = compile_csil(source);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty());
    let strategy = expected.strategy.unwrap();
    let mut input: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
    input["mintick"] = 0.00001.into();
    input["mincontract"] = 0.01.into();
    input["pointvalue"] = 1.0.into();
    input["include_results"] = true.into();
    let wasm = compile_strategy_standalone_wasm(source);
    let mut instance = StandaloneWasm::new(&wasm);
    let result = instance.evaluate(&input.to_string()).expect("full result");
    let actual: serde_json::Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(actual["plots"][0], serde_json::json!([3.0, 4.0, 5.0]));
    assert!(actual["plots"][1][0].is_null());
    assert_eq!(actual["strategy"]["trades"].as_array().unwrap().len(), 1);
    assert!((strategy.trades[0].qty - 330.03).abs() < 1e-10);
    assert_eq!(
        actual["strategy"]["trades"][0]["qty"],
        strategy.trades[0].qty
    );
    assert_eq!(
        actual["strategy"]["trades"][0]["commission"],
        strategy.trades[0].commission
    );
    assert_eq!(
        actual["strategy"]["trades"][0]["entryPrice"],
        strategy.trades[0].entry_price
    );
    assert_eq!(
        actual["strategy"]["trades"][0]["pnl"],
        strategy.trades[0].pnl
    );
    assert_eq!(
        actual["strategy"]["equity"],
        serde_json::to_value(strategy.equity).unwrap()
    );
    assert_eq!(
        result,
        instance.evaluate(&input.to_string()).unwrap(),
        "prepared reuse must reset native broker and output state"
    );
    let stable = instance.evaluate_reclaiming(&input.to_string());
    let stable_bytes = instance.memory_bytes();
    for _ in 0..32 {
        assert_eq!(stable, instance.evaluate_reclaiming(&input.to_string()));
        assert_eq!(
            instance.memory_bytes(),
            stable_bytes,
            "full output must not leak the broker arena"
        );
    }
    for (key, value) in [
        ("netProfit", strategy.net_profit),
        ("grossProfit", strategy.gross_profit),
        ("grossLoss", strategy.gross_loss),
        ("maxDrawdown", strategy.max_drawdown),
    ] {
        assert_eq!(actual["strategy"][key].as_f64(), Some(value), "{key}");
    }
    let legacy: serde_json::Value =
        serde_json::from_slice(&instance.evaluate(&wire(&bars)).unwrap()).unwrap();
    assert!(
        legacy.get("plots").is_none(),
        "full-result flag must not leak to another evaluation"
    );
    for field in ["mintick", "mincontract", "pointvalue"] {
        let mut invalid = input.clone();
        invalid[field] = 0.into();
        assert!(
            instance.evaluate(&invalid.to_string()).is_err(),
            "invalid {field} must fail"
        );
    }
}

// Binary transport must preserve full results, parameter specialization, input
// source lanes, error recovery and bounded memory on a resident module.
#[test]
fn binary_input_matches_json_and_recovers_from_malformed_packets() {
    let source = "strategy(\"Binary\", initial_capital=10000)\nx = input.int(5, \"Length\", minval=1, maxval=1000)\ns = input.source(close, \"Source\")\nplot(ta.sma(s, x))\nif close > open\n    strategy.entry(\"L\", strategy.long)\nif close < open\n    strategy.close(\"L\")\n";
    let mut standalone_wasm = StandaloneWasm::new(&compile_strategy_standalone_wasm(source));
    let bars = synthetic_bars();
    let mut document: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
    document["include_results"] = true.into();
    document["mintick"] = 0.01.into();
    document["mincontract"] = 0.01.into();
    document["pointvalue"] = 1.0.into();
    for len in [5, 13, 5] {
        document["params"] = serde_json::json!({"length":len});
        let expected = standalone_wasm.evaluate(&document.to_string()).unwrap();
        let mut metadata = document.clone();
        metadata.as_object_mut().unwrap().remove("bars");
        let meta = metadata.to_string().into_bytes();
        let offset = (16 + meta.len() + 7) & !7;
        let mut binary = vec![0u8; offset];
        binary[..8].copy_from_slice(b"PWSWBIN1");
        binary[8..12].copy_from_slice(&(bars.len() as u32).to_le_bytes());
        binary[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
        binary[16..16 + meta.len()].copy_from_slice(&meta);
        for lane in 0..6 {
            for b in &bars {
                let value = [b.open, b.high, b.low, b.close, b.volume, b.time as f64][lane];
                binary.extend_from_slice(&value.to_le_bytes());
            }
        }
        assert_eq!(standalone_wasm.evaluate_bytes(&binary).unwrap(), expected);
        let stable = standalone_wasm.memory_bytes();
        for _ in 0..8 {
            assert_eq!(standalone_wasm.evaluate_bytes(&binary).unwrap(), expected);
        }
        assert_eq!(standalone_wasm.memory_bytes(), stable);
        let mut trailing = binary.clone();
        trailing.push(0);
        assert!(standalone_wasm.evaluate_bytes(&trailing).is_err());
        let mut empty = binary.clone();
        empty[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(standalone_wasm.evaluate_bytes(&empty).is_err());
        for key in ["bars", "fraction"] {
            let mut conflicting = metadata.clone();
            conflicting[key] = serde_json::json!(1);
            let meta = conflicting.to_string().into_bytes();
            let start = (16 + meta.len() + 7) & !7;
            let mut packet = vec![0u8; start];
            packet[..12].copy_from_slice(&binary[..12]);
            packet[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
            packet[16..16 + meta.len()].copy_from_slice(&meta);
            packet.extend_from_slice(&binary[offset..]);
            assert!(standalone_wasm
                .evaluate_bytes(&packet)
                .unwrap_err()
                .contains("must omit"));
        }
        for size in [8, 15, binary.len() - 1] {
            assert!(standalone_wasm.evaluate_bytes(&binary[..size]).is_err());
            assert_eq!(standalone_wasm.evaluate_bytes(&binary).unwrap(), expected);
        }
        let mut invalid = binary.clone();
        invalid[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(standalone_wasm.evaluate_bytes(&invalid).is_err());
        let time_start = offset + bars.len() * 5 * 8;
        invalid = binary.clone();
        invalid[time_start..time_start + 8].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(standalone_wasm
            .evaluate_bytes(&invalid)
            .unwrap_err()
            .contains("safe integer"));
        for invalid_time in [1.5f64, f64::INFINITY, 9_007_199_254_740_992.0] {
            invalid[time_start..time_start + 8].copy_from_slice(&invalid_time.to_le_bytes());
            assert!(standalone_wasm.evaluate_bytes(&invalid).is_err());
        }
        assert_eq!(standalone_wasm.evaluate_bytes(&binary).unwrap(), expected);
        assert_eq!(
            standalone_wasm.evaluate(&document.to_string()).unwrap(),
            expected
        );
        document["binary_results"] = true.into();
        let output = standalone_wasm.evaluate(&document.to_string()).unwrap();
        assert_eq!(
            decode_binary_results(&output),
            serde_json::from_slice::<serde_json::Value>(&expected).unwrap()
        );
        let stable_binary_memory = standalone_wasm.memory_bytes();
        for _ in 0..32 {
            assert_eq!(
                standalone_wasm.evaluate(&document.to_string()).unwrap(),
                output
            );
        }
        assert_eq!(standalone_wasm.memory_bytes(), stable_binary_memory);
        document["include_results"] = false.into();
        assert!(standalone_wasm
            .evaluate(&document.to_string())
            .unwrap_err()
            .contains("requires include_results"));
        document["include_results"] = true.into();
        document.as_object_mut().unwrap().remove("binary_results");
        assert_eq!(
            standalone_wasm.evaluate(&document.to_string()).unwrap(),
            expected
        );
    }
}

fn decode_binary_results(bytes: &[u8]) -> serde_json::Value {
    assert_eq!(&bytes[..8], b"PWSWOUT1");
    let u32_at = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
    let plots = u32_at(8);
    let bars = u32_at(12);
    let metadata_len = u32_at(16);
    assert_eq!(u32_at(20), 0);
    let start = (24 + metadata_len + 7) & !7;
    assert_eq!(bytes.len(), start + (plots + 1) * bars * 8);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&bytes[24..24 + metadata_len]).unwrap();
    let lane = |i| -> serde_json::Value {
        bytes[start + i * bars * 8..start + (i + 1) * bars * 8]
            .chunks_exact(8)
            .map(|b| serde_json::Value::from(f64::from_le_bytes(b.try_into().unwrap())))
            .collect::<Vec<_>>()
            .into()
    };
    metadata["plots"] = (0..plots).map(lane).collect::<Vec<_>>().into();
    metadata["strategy"]["equity"] = lane(plots);
    metadata
}

#[test]
fn binary_results_support_strategies_without_plots() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_strategy_standalone_wasm(
        "strategy(\"No plots\")\nstrategy.entry(\"L\", strategy.long)\n",
    ));
    let mut doc: serde_json::Value = serde_json::from_str(&wire(&synthetic_bars())).unwrap();
    doc["include_results"] = true.into();
    let expected = standalone_wasm.evaluate(&doc.to_string()).unwrap();
    doc["binary_results"] = true.into();
    let actual = decode_binary_results(&standalone_wasm.evaluate(&doc.to_string()).unwrap());
    assert_eq!(
        actual,
        serde_json::from_slice::<serde_json::Value>(&expected).unwrap()
    );
}

fn resident_packet(
    bars: &[Bar],
    metadata: &serde_json::Value,
    id: u64,
    reference: bool,
) -> Vec<u8> {
    let meta = metadata.to_string().into_bytes();
    let start = (24 + meta.len() + 7) & !7;
    let mut bytes = vec![0u8; start];
    bytes[..8].copy_from_slice(if reference { b"PWSWREF1" } else { b"PWSWBIN2" });
    bytes[8..12].copy_from_slice(&(bars.len() as u32).to_le_bytes());
    bytes[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
    bytes[16..24].copy_from_slice(&id.to_le_bytes());
    bytes[24..24 + meta.len()].copy_from_slice(&meta);
    if !reference {
        for lane in 0..6 {
            for bar in bars {
                bytes.extend_from_slice(
                    &[
                        bar.open,
                        bar.high,
                        bar.low,
                        bar.close,
                        bar.volume,
                        bar.time as f64,
                    ][lane]
                        .to_le_bytes(),
                );
            }
        }
    }
    bytes
}

#[test]
fn resident_dataset_reuses_only_prices_with_fresh_parameters_and_broker() {
    let source = "strategy(\"Resident\", initial_capital=10000)\nx=input.int(5,\"Length\", minval=1, maxval=1000)\ns=input.source(close,\"Source\")\nplot(ta.sma(s,x),\"Mean\")\nplot(ta.atr(x),\"ATR\")\nif close>open\n    strategy.entry(\"L\",strategy.long)\nif close<open\n    strategy.close(\"L\")\n";
    let wasm = compile_strategy_standalone_wasm(source);
    let capabilities = wasmparser::Parser::new(0)
        .parse_all(&wasm)
        .find_map(|payload| match payload.unwrap() {
            wasmparser::Payload::CustomSection(section)
                if section.name() == "pinewasm_capabilities" =>
            {
                Some(serde_json::from_slice::<serde_json::Value>(section.data()).unwrap())
            }
            _ => None,
        })
        .expect("advertised optional protocol");
    assert_eq!(capabilities["schema_version"], 1);
    assert!(capabilities["input_protocols"]
        .as_array()
        .unwrap()
        .contains(&"PWSWREF1".into()));
    let mut resident = StandaloneWasm::new(&wasm);
    let bars = synthetic_bars();
    let mut metadata = serde_json::json!({"version":1,"params":{},"include_results":true,"binary_results":true,"mintick":0.01});
    resident.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 42, false));
    let stable = resident.memory_bytes();
    for length in [5, 17, 3, 8, 5] {
        metadata["params"] = serde_json::json!({"length":length});
        let mut fresh = StandaloneWasm::new(&wasm);
        let expected =
            fresh.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 99, false));
        let reference = resident_packet(&bars, &metadata, 42, true);
        assert_eq!(
            reference.len() + bars.len() * 48,
            resident_packet(&bars, &metadata, 42, false).len()
        );
        for _ in 0..16 {
            assert_eq!(resident.evaluate_bytes_reclaiming(&reference), expected);
            assert_eq!(
                resident.memory_bytes(),
                stable,
                "resident trials must not grow retained memory"
            );
        }
    }
    // A different dataset of the same length must replace values, not just its
    // identifier. Price lanes change while timestamps remain identical.
    let other: Vec<Bar> = bars
        .iter()
        .map(|b| Bar {
            time: b.time,
            open: b.open + 20.0,
            high: b.high + 20.0,
            low: b.low + 20.0,
            close: b.close + 20.0,
            volume: b.volume,
        })
        .collect();
    let upload = resident_packet(&other, &metadata, 43, false);
    let expected = StandaloneWasm::new(&wasm).evaluate_bytes_reclaiming(&upload);
    assert_eq!(resident.evaluate_bytes_reclaiming(&upload), expected);
    assert_eq!(
        resident.evaluate_bytes_reclaiming(&resident_packet(&other, &metadata, 43, true)),
        expected
    );
    assert!(resident
        .evaluate_bytes(&resident_packet(&bars, &metadata, 42, true))
        .unwrap_err()
        .contains("stale or missing"));
}

#[test]
fn resident_dataset_reset_errors_and_legacy_inputs_invalidate_references() {
    let wasm =
        compile_strategy_standalone_wasm("strategy(\"Resident invalidation\")\nplot(close)\n");
    let bars = synthetic_bars();
    let metadata = serde_json::json!({"version":1,"params":{},"include_results":true});
    let upload = resident_packet(&bars, &metadata, 7, false);
    let reference = resident_packet(&bars, &metadata, 7, true);
    let mut instance = StandaloneWasm::new(&wasm);
    assert!(instance
        .evaluate_bytes(&reference)
        .unwrap_err()
        .contains("stale or missing"));
    let reset = instance
        .instance
        .get_typed_func::<(), ()>(&instance.store, "reset")
        .unwrap();
    let expected = instance.evaluate_bytes_reclaiming(&upload);
    reset.call(&mut instance.store, ()).unwrap();
    assert!(instance.evaluate_bytes(&reference).is_err());
    for malformed in [
        resident_packet(&bars, &metadata, 0, false),
        resident_packet(&bars[..bars.len() - 1], &metadata, 7, true),
        resident_packet(
            &bars,
            &serde_json::json!({"version":1,"params":{},"mintick":0}),
            7,
            true,
        ),
        reference[..reference.len() - 1].to_vec(),
        [reference.clone(), vec![0]].concat(),
    ] {
        instance.evaluate_bytes_reclaiming(&upload);
        assert!(instance.evaluate_bytes(&malformed).is_err());
        assert!(instance
            .evaluate_bytes(&reference)
            .unwrap_err()
            .contains("stale or missing"));
        reset.call(&mut instance.store, ()).unwrap();
        assert_eq!(instance.evaluate_bytes_reclaiming(&upload), expected);
        assert_eq!(instance.evaluate_bytes_reclaiming(&reference), expected);
    }
    instance.evaluate_reclaiming(&wire(&bars));
    assert!(instance
        .evaluate_bytes(&reference)
        .unwrap_err()
        .contains("stale or missing"));
}

#[test]
fn resident_dataset_runtime_errors_invalidate_after_successful_fill() {
    let wasm = compile_strategy_standalone_wasm(
        "strategy(\"Resident error\")\nlength=input.int(1,\"Length\", minval=0, maxval=1000)\nplot(ta.sma(close,length))\n",
    );
    let bars = &synthetic_bars()[..4];
    let good = serde_json::json!({"version":1,"params":{"length":1},"include_results":true});
    let bad = serde_json::json!({"version":1,"params":{"length":0},"include_results":true});
    let mut instance = StandaloneWasm::new(&wasm);
    let upload = resident_packet(bars, &good, 1, false);
    let reference = resident_packet(bars, &good, 1, true);
    let expected = instance.evaluate_bytes_reclaiming(&upload);
    assert!(instance
        .evaluate_bytes(&resident_packet(bars, &bad, 1, true))
        .unwrap_err()
        .starts_with("Indicator failure: "));
    assert!(instance
        .evaluate_bytes(&reference)
        .unwrap_err()
        .contains("stale or missing"));
    assert_eq!(instance.evaluate_bytes_reclaiming(&upload), expected);
    assert_eq!(instance.evaluate_bytes_reclaiming(&reference), expected);
}

fn report_objectives(full: &serde_json::Value, initial: f64) -> serde_json::Value {
    let mut net = 0.0;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut commission = 0.0;
    let trades = full["strategy"]["trades"].as_array().unwrap();
    for trade in trades {
        let pnl = trade["pnl"].as_f64().unwrap();
        net += pnl;
        gross_profit += pnl.max(0.0);
        gross_loss += (-pnl).max(0.0);
        commission += trade["commission"].as_f64().unwrap();
    }
    let equity = full["strategy"]["equity"].as_array().unwrap();
    let mut peak = initial;
    let mut drawdown: f64 = 0.0;
    for value in equity {
        let value = value.as_f64().unwrap();
        peak = peak.max(value);
        drawdown = drawdown.max(peak - value);
    }
    serde_json::json!({"initialCapital":initial,"netProfit":net,"grossProfit":gross_profit,
        "grossLoss":gross_loss,"closedTrades":trades.len(),"totalCommission":commission,
        "finalEquity":equity.last().unwrap(),"positionSize":full["strategy"]["position"]["size"],"maxDrawdown":drawdown})
}

#[test]
fn objective_projection_matches_full_reports_and_retains_fresh_execution() {
    let bars = synthetic_bars();
    for source in [ORDER_PRIMITIVES, PROCESS_ON_CLOSE, STOP_EXIT, CROSSOVER,
        "strategy(\"Empty\", initial_capital=10000)\nplot(close)\n",
        "strategy(\"Fees\",initial_capital=10000,commission_type=strategy.commission.percent,commission_value=0.1,default_qty_type=strategy.fixed,default_qty_value=0.25,process_orders_on_close=true)\nn=input.int(5,\"Period\", minval=1, maxval=1000)\nif bar_index%n==0\n    strategy.entry(\"L\",strategy.long)\nif bar_index%n==2\n    strategy.close(\"L\")\nplot(strategy.netprofit)\n",
    ] {
        let program = compile_csil(source);
        let initial = program.metadata.strategy.as_ref().unwrap().initial_capital;
        let wasm = compile_strategy_standalone_wasm(source);
        let mut instance = StandaloneWasm::new(&wasm);
        let mut metadata = serde_json::json!({"version":1,"params":{},"include_results":true,"mincontract":0.01});
        let first = instance.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 1, false));
        let maximum_pages = instance.memory_bytes();
        for i in 0..12 {
            if source.contains("input.int") {
                metadata["params"] = serde_json::json!({"period":if i%2==0 { 5 } else { 11 }});
            }
            metadata["include_results"] = true.into();
            metadata["objective_results"] = false.into();
            let full_bytes = instance.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 1, true));
            let full: serde_json::Value = serde_json::from_slice(&full_bytes).unwrap();
            let expected = report_objectives(&full, initial);
            metadata["include_results"] = false.into();
            metadata["objective_results"] = true.into();
            let compact_bytes = instance.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 1, true));
            assert!(compact_bytes.len() < 1024);
            assert!(compact_bytes.len() < full_bytes.len());
            let compact: serde_json::Value = serde_json::from_slice(&compact_bytes).unwrap();
            assert_eq!(compact["projection"], "retained-objectives-v1");
            assert_eq!(compact["bars"].as_f64(), Some(bars.len() as f64));
            assert_eq!(compact["firstRetainedTradeIndex"], full["strategy"]["firstRetainedTradeIndex"]);
            assert_eq!(compact["metrics"].as_object().unwrap().len(), expected.as_object().unwrap().len());
            for (name, expected) in expected.as_object().unwrap() {
                assert_eq!(compact["metrics"][name].as_f64(), expected.as_f64(), "{name} in {source}");
            }
            assert!(compact.get("plots").is_none() && compact.get("signals").is_none());
            assert_eq!(instance.memory_bytes(), maximum_pages);
        }
        metadata["params"] = serde_json::json!({});
        metadata["include_results"] = true.into();
        assert!(instance.evaluate_bytes(&resident_packet(&bars, &metadata, 1, true)).unwrap_err().contains("cannot be combined"));
        metadata["objective_results"] = false.into();
        assert_eq!(instance.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 1, false)), first);
    }
}

/// The numeric `strategy.opentrades.*` / `strategy.closedtrades.*` accessors
/// lower natively to the linked broker's trade view and read exactly what the
/// interpreter's `read_trade` reads, bar for bar: every field over two
/// pyramided open lots, closed trades by index, and the na/out-of-range
/// indices. Trading decisions also depend on the accessors, so any drift
/// would compound into diverging positions.
#[test]
fn trade_accessors_match_the_interpreter_bar_for_bar() {
    let source = concat!(
        "//@version=6\n",
        "strategy('Trades', initial_capital=10000, pyramiding=2, commission_type=strategy.commission.percent, commission_value=0.1, process_orders_on_close=true)\n",
        "fast = ta.sma(close, 5)\n",
        "slow = ta.sma(close, 20)\n",
        "if ta.crossover(fast, slow) or (strategy.opentrades == 1 and close > strategy.opentrades.entry_price(0) * 1.01)\n",
        "    strategy.entry('L', strategy.long)\n",
        "if strategy.opentrades > 0 and (close < strategy.opentrades.entry_price(0) * 0.98 or bar_index > strategy.opentrades.entry_bar_index(0) + 15.0)\n",
        "    strategy.close_all()\n",
        "plot(strategy.opentrades.entry_price(0))\n",
        "plot(strategy.opentrades.size(0))\n",
        "plot(strategy.opentrades.profit(0))\n",
        "plot(strategy.opentrades.profit_percent(0))\n",
        "plot(strategy.opentrades.commission(0))\n",
        "plot(strategy.opentrades.entry_bar_index(0))\n",
        "plot(strategy.opentrades.max_runup(0))\n",
        "plot(strategy.opentrades.max_runup_percent(0))\n",
        "plot(strategy.opentrades.max_drawdown(0))\n",
        "plot(strategy.opentrades.max_drawdown_percent(0))\n",
        "plot(strategy.opentrades.entry_price(1))\n",
        "plot(strategy.opentrades.entry_price(5))\n",
        "plot(strategy.opentrades.entry_price(na))\n",
        "plot(strategy.closedtrades.entry_price(2))\n",
        "plot(strategy.closedtrades.exit_price(2))\n",
        "plot(strategy.closedtrades.size(2))\n",
        "plot(strategy.closedtrades.profit(2))\n",
        "plot(strategy.closedtrades.profit_percent(2))\n",
        "plot(strategy.closedtrades.commission(2))\n",
        "plot(strategy.closedtrades.entry_bar_index(2))\n",
        "plot(strategy.closedtrades.exit_bar_index(2))\n",
        "plot(strategy.closedtrades.max_runup(2))\n",
        "plot(strategy.closedtrades.max_drawdown_percent(2))\n",
        "plot(strategy.closedtrades.entry_price(0))\n",
        "plot(strategy.closedtrades.profit(1))\n",
        "plot(strategy.closedtrades.entry_price(-1))\n",
    );
    let bars = synthetic_bars();
    let data = market_data(&bars);
    let program = compile_csil(source);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let closed = expected.strategy.as_ref().unwrap().trades.len();
    assert!(
        closed >= 3,
        "the fixture must close several trades ({closed})"
    );

    let mut input: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
    input["include_results"] = true.into();
    let wasm = compile_strategy_standalone_wasm(source);
    pinewasm_aot::audit_standalone_wasm(&wasm).expect("standalone audit");
    let out = StandaloneWasm::new(&wasm)
        .evaluate(&input.to_string())
        .expect("evaluate");
    let actual: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let plots = actual["plots"].as_array().expect("plots");
    assert_eq!(plots.len(), expected.plots.len());
    let mut saw_second_lot = false;
    for (index, (native, reference)) in plots.iter().zip(&expected.plots).enumerate() {
        let native = native.as_array().expect("plot values");
        assert_eq!(native.len(), bars.len(), "plot {index} length");
        for (bar, (a, e)) in native.iter().zip(&reference.values).enumerate() {
            let e = match e {
                Value::Float(v) => Some(*v),
                Value::Int(v) => Some(*v as f64),
                Value::Na => None,
                other => panic!("plot {index} bar {bar}: unexpected {other:?}"),
            };
            assert_eq!(a.as_f64(), e, "plot {index} diverges at bar {bar}");
            saw_second_lot |= index == 10 && e.is_some();
        }
    }
    assert!(saw_second_lot, "the fixture must pyramid a second open lot");
}

// ---------------------------------------------------------------------------
// Erased drawing effects in strategies.

/// A strategy that keeps a take-profit line: `var line` state, `line.delete`
/// of an `na` and of a live line, `line.new(..., extend=extend.right)` and an
/// `alertcondition`. The standalone projection erases every drawing site, so
/// no `host.vm` site survives and `alertcondition` lowers to the native alert
/// lane, which a strategy's output ignores (see `docs/aot.md`).
const DRAWING_STRATEGY: &str = concat!(
    "//@version=6\n",
    "strategy('Lines', initial_capital=10000, process_orders_on_close=true)\n",
    "fast = ta.ema(close, 2)\n",
    "slow = ta.ema(close, 4)\n",
    "bull = ta.crossover(fast, slow)\n",
    "bear = ta.crossunder(fast, slow)\n",
    "if bull\n",
    "    strategy.entry('L', strategy.long)\n",
    "if bear\n",
    "    strategy.close('L')\n",
    "var line x = na\n",
    "var float prevSize = 0.0\n",
    "if strategy.position_size != prevSize\n",
    "    line.delete(x)\n",
    "    p = close * 1.02\n",
    "    x := line.new(bar_index, p, bar_index + 1, p, extend=extend.right)\n",
    "prevSize := strategy.position_size\n",
    "alertcondition(bull, title='Long', message='Long entry')\n",
    "plot(strategy.position_size)\n",
    "plot(strategy.netprofit)\n",
);

fn capabilities(wasm: &[u8]) -> serde_json::Value {
    serde_json::from_slice(&custom_section(wasm, "pinewasm_capabilities").expect("capabilities"))
        .expect("capabilities JSON")
}

fn line_field_count(program: &pinewasm_csil::CsilProgram) -> u32 {
    program
        .main()
        .code
        .iter()
        .find_map(|op| match op {
            pinewasm_csil::CsilInstr::NewObject { field_count, .. } => Some(*field_count),
            _ => None,
        })
        .expect("a line.new constructor")
}

#[test]
fn erased_drawings_match_the_interpreter_bar_for_bar() {
    let bars = synthetic_bars();
    let data = market_data(&bars);
    let program = compile_csil(DRAWING_STRATEGY);
    let expected = run(&program, &data, &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let strategy = expected.strategy.as_ref().expect("strategy outcome");
    assert!(
        strategy.trades.len() >= 3,
        "the fixture must close several trades: {}",
        strategy.trades.len()
    );
    // The VM records the fired alertconditions; the standalone strategy
    // output has no alert field, so nothing is compared or lost there.
    assert!(!expected.alerts.is_empty(), "the fixture must fire alerts");

    let wasm = compile_strategy_standalone_wasm(DRAWING_STRATEGY);
    pinewasm_aot::audit_standalone_wasm(&wasm).expect("standalone audit");
    // One line per bar at most, `field_count` elements each, against the
    // default 100,000-element budget and the 1,000,000-object cap.
    let fields = u64::from(line_field_count(&program));
    assert_eq!(
        capabilities(&wasm)["max_bars"].as_u64(),
        Some((100_000 / fields).min(1_000_000))
    );

    // Posture, from the OUT signal document.
    let out = StandaloneWasm::new(&wasm)
        .evaluate(&wire(&bars))
        .expect("evaluate");
    let signals = posture_signals(&out, &bars, bars.len());
    let aot = posture_lane(&signals, &bars);
    let reference = interpreter_posture(DRAWING_STRATEGY, &data);
    for (bar, (a, e)) in aot.iter().zip(&reference).enumerate() {
        assert_eq!(a, e, "posture diverges at bar {bar}");
    }

    // Trades, net profit and every plot, from the full results.
    let mut input: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
    input["include_results"] = true.into();
    let full: serde_json::Value = serde_json::from_slice(
        &StandaloneWasm::new(&wasm)
            .evaluate(&input.to_string())
            .expect("full results"),
    )
    .unwrap();
    let trades = full["strategy"]["trades"].as_array().expect("trades");
    assert_eq!(trades.len(), strategy.trades.len());
    for (index, (native, reference)) in trades.iter().zip(&strategy.trades).enumerate() {
        assert_eq!(native["qty"].as_f64(), Some(reference.qty), "trade {index}");
        assert_eq!(
            native["entryPrice"].as_f64(),
            Some(reference.entry_price),
            "trade {index}"
        );
        assert_eq!(native["pnl"].as_f64(), Some(reference.pnl), "trade {index}");
    }
    assert_eq!(
        full["strategy"]["netProfit"].as_f64(),
        Some(strategy.net_profit)
    );
    let plots = full["plots"].as_array().expect("plots");
    assert_eq!(plots.len(), expected.plots.len());
    for (index, (native, reference)) in plots.iter().zip(&expected.plots).enumerate() {
        for (bar, (a, e)) in native
            .as_array()
            .expect("plot values")
            .iter()
            .zip(&reference.values)
            .enumerate()
        {
            let e = match e {
                Value::Float(v) => Some(*v),
                Value::Int(v) => Some(*v as f64),
                Value::Na => None,
                other => panic!("plot {index} bar {bar}: unexpected {other:?}"),
            };
            assert_eq!(a.as_f64(), e, "plot {index} diverges at bar {bar}");
        }
    }
}

/// The `NewObject` certificate holds only while total allocations stay inside
/// the VM heap caps, so the artifact refuses a request that could execute
/// more bars than its bound, and accepts (and matches the VM on) one at the
/// bound. A small element budget makes the bound a handful of bars.
#[test]
fn a_request_past_the_drawing_allocation_bound_is_refused() {
    let program = compile_csil(DRAWING_STRATEGY);
    let fields = u64::from(line_field_count(&program));
    let limit = 5u64;
    let budget = fields * limit + fields - 1;
    let wasm = pinewasm_aot::compile_standalone_wasm_program_with_config(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
        &pinewasm_aot::StandaloneWasmBuildConfig {
            max_collection_elements: budget,
            ..pinewasm_aot::StandaloneWasmBuildConfig::default()
        },
    )
    .expect("bounded strategy emit");
    assert_eq!(capabilities(&wasm)["max_bars"].as_u64(), Some(limit));

    let bars = synthetic_bars();
    let mut instance = StandaloneWasm::new(&wasm);
    let error = instance
        .evaluate(&wire(&bars[..=limit as usize]))
        .expect_err("one bar past the bound must be refused");
    assert_eq!(
        error,
        format!(
            "request executes {} bars, but this artifact's drawing allocation bound admits at most {limit} (pinewasm_capabilities max_bars)",
            limit + 1
        )
    );
    // A fraction that trims the executed prefix back under the bound is
    // accepted: the bound counts executed bars, not supplied ones.
    let mut trimmed: serde_json::Value =
        serde_json::from_str(&wire(&bars[..2 * limit as usize])).unwrap();
    trimmed["fraction"] = 0.5.into();
    instance
        .evaluate(&trimmed.to_string())
        .expect("a fraction inside the bound");

    let at_bound = &bars[..limit as usize];
    let out = instance
        .evaluate(&wire(at_bound))
        .expect("a request at the bound");
    let options = RunOptions {
        max_collection_elements: budget as usize,
        ..RunOptions::default()
    };
    let data = market_data(at_bound);
    let reference = run(&program, &data, &options);
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    let signals = posture_signals(&out, at_bound, at_bound.len());
    assert_eq!(posture_lane(&signals, at_bound).len(), at_bound.len());
}

/// A constructor that can run more than once per bar has no per-bar
/// allocation bound, so it keeps its host site and the strategy is still
/// refused by the drawing's Pine name.
#[test]
fn a_drawing_constructor_inside_a_loop_is_still_refused() {
    let source = concat!(
        "strategy('Loop lines')\n",
        "float k = 0.0\n",
        "while k < 3.0\n",
        "    line.new(bar_index, close + k, bar_index + 1, close)\n",
        "    k += 1.0\n",
        "if close > open\n",
        "    strategy.entry('L', strategy.long)\n",
    );
    let error = compile_standalone_wasm_program(
        &compile_csil(source),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect_err("a looped constructor must be refused");
    match error {
        AotError::StrategyFeatureUnsupported { feature } => {
            assert!(feature.contains("line"), "{feature}");
        }
        other => panic!("unexpected refusal {other:?}"),
    }
}

/// Large inlined strategies must not require one compiler-local cell per
/// Pine slot. Exercise branch-local na, persistent carry, histories, all scalar
/// types and repeated instance use against the interpreter.
#[test]
fn large_current_cell_storage_preserves_strategy_and_instance_isolation() {
    let mut source = String::from("//@version=6\nstrategy('Large current cells', process_orders_on_close=true)\nvar float carry = na\nfloat total = 0.0\n");
    for i in 0..300 {
        source.push_str(&format!("float value{i} = na\nif close > open\n    value{i} := close + {i}.0\ntotal += nz(value{i}, 0.0)\n"));
    }
    source.push_str("int counter = bar_index\nbool rising = close > close[1]\nstring direction = rising ? 'up' : 'down'\ncolor tint = rising ? color.green : color.red\nvar values = array.new<float>(1, 0.0)\narray.set(values, 0, total)\ncarry := total + nz(carry[1], 0.0) * 0.01\nif total > 0 and counter > 0 and direction == 'up' and tint == color.green and array.get(values, 0) > 0 and carry > 0\n    strategy.entry('L', strategy.long)\nelse\n    strategy.close('L')\n");
    let program = compile_csil(&source);
    assert!(program.series_slots.len() > 256);
    let wasm = compile_strategy_sliced(&source);
    pinewasm_aot::audit_standalone_wasm(&wasm).expect("five-export standalone surface");
    let max_locals = wasmparser::Parser::new(0)
        .parse_all(&wasm)
        .filter_map(|payload| match payload.expect("valid module") {
            wasmparser::Payload::CodeSectionEntry(body) => Some(
                body.get_locals_reader()
                    .unwrap()
                    .into_iter()
                    .map(|entry| entry.unwrap().0 as usize)
                    .sum::<usize>(),
            ),
            _ => None,
        })
        .max()
        .unwrap();
    // Linked runtime helpers also count; the script's thousands of current
    // cells must not appear in any function's local declaration.
    assert!(
        max_locals < 256,
        "unbounded compiler-local state: {max_locals}"
    );
    let bars = synthetic_bars();
    let expected = interpreter_posture(&source, &market_data(&bars));
    let input = wire(&bars);
    let mut host = StandaloneWasm::new(&wasm);
    let first = host.evaluate(&input).unwrap();
    assert_eq!(
        posture_lane(&posture_signals(&first, &bars, bars.len()), &bars),
        expected
    );
    let different = &bars[..37];
    let different_out = host.evaluate(&wire(different)).unwrap();
    assert_eq!(
        posture_lane(
            &posture_signals(&different_out, different, different.len()),
            different
        ),
        interpreter_posture(&source, &market_data(different))
    );
    assert_eq!(
        host.evaluate(&input).unwrap(),
        first,
        "fresh-run globals after a shorter evaluation"
    );
    assert_eq!(
        StandaloneWasm::new(&wasm).evaluate(&input).unwrap(),
        first,
        "instance-owned current cells"
    );
}

/// Synthetic arithmetic regions on both sides of order placement force
/// outlining. The snapshot plots
/// distinguish the pre-script fill walk from the post-script close pass;
/// full equity and trades detect any extra or missing broker boundary.
fn split_strategy_source(process_on_close: bool) -> String {
    let mut source = format!(
        "//@version=6\nstrategy('Split broker', initial_capital=10000, commission_type=strategy.commission.percent, commission_value=0.1, process_orders_on_close={process_on_close})\n\
         var float carried = 0.0\nfloat total = 0.0\n\
         if bar_index % 7 == 0\n    strategy.entry('L', strategy.long)\n\
         for j = 0 to 2\n    total += close * j\n"
    );
    for i in 0..80 {
        source.push_str(&format!(
            "float value{i} = close * {}.0 - open\n\
             if value{i} > high\n    value{i} := value{i} - high\n\
             total += value{i} > close ? value{i} - close : close - value{i}\n",
            i + 1
        ));
        if i == 40 {
            source.push_str(
                "if bar_index % 7 == 4\n    strategy.close('L')\nplot(strategy.position_size)\nplot(strategy.equity)\n",
            );
        }
    }
    source.push_str("carried := total + nz(carried[1], 0.0) * 0.01\nplot(total)\nplot(carried)\n");
    source
}

fn assert_full_strategy_results(actual: &serde_json::Value, expected: &pinewasm_vm::RunOutcome) {
    let plots: Vec<Vec<Option<f64>>> = expected
        .plots
        .iter()
        .map(|plot| {
            plot.values
                .iter()
                .map(|value| match value {
                    Value::Float(value) => Some(*value),
                    Value::Int(value) => Some(*value as f64),
                    Value::Na => None,
                    value => panic!("unexpected numeric plot value: {value:?}"),
                })
                .collect()
        })
        .collect();
    assert_eq!(actual["plots"], serde_json::to_value(plots).unwrap());
    let strategy = expected.strategy.as_ref().expect("strategy outcome");
    assert!(!strategy.trades.is_empty(), "the fixture must close trades");
    assert_eq!(
        actual["strategy"]["equity"],
        serde_json::to_value(&strategy.equity).unwrap()
    );
    let trades = actual["strategy"]["trades"].as_array().unwrap();
    assert_eq!(trades.len(), strategy.trades.len());
    for (actual, expected) in trades.iter().zip(&strategy.trades) {
        assert_eq!(actual["entryId"], expected.entry_id);
        assert_eq!(actual["exitId"], expected.exit_id.as_deref().unwrap_or(""));
        for (field, value) in [
            ("sign", expected.sign),
            ("qty", expected.qty),
            ("entryBar", expected.entry_bar as f64),
            ("exitBar", expected.exit_bar as f64),
            ("entryPrice", expected.entry_price),
            ("exitPrice", expected.exit_price),
            ("pnl", expected.pnl),
            ("commission", expected.commission),
        ] {
            assert_eq!(actual[field].as_f64(), Some(value), "trade {field}");
        }
    }
    for (field, value) in [
        ("netProfit", strategy.net_profit),
        ("grossProfit", strategy.gross_profit),
        ("grossLoss", strategy.gross_loss),
        ("maxDrawdown", strategy.max_drawdown),
    ] {
        assert_eq!(actual["strategy"][field].as_f64(), Some(value), "{field}");
    }
}

#[test]
fn split_strategy_pieces_preserve_order_phases_snapshots_and_full_results() {
    let all_bars = synthetic_bars();
    for process_on_close in [false, true] {
        let source = split_strategy_source(process_on_close);
        let program = compile_csil(&source);
        let wasm = compile_strategy_standalone_wasm(&source);
        let mut instance = StandaloneWasm::new(&wasm);
        for count in [81, 23, 81] {
            let bars = &all_bars[..count];
            let expected = run(&program, &market_data(bars), &RunOptions::default());
            assert!(expected.errors.is_empty(), "{:?}", expected.errors);
            let mut input: serde_json::Value = serde_json::from_str(&wire(bars)).unwrap();
            input["include_results"] = true.into();
            let first = instance.evaluate_reclaiming(&input.to_string());
            let actual: serde_json::Value = serde_json::from_slice(&first).unwrap();
            assert_full_strategy_results(&actual, &expected);
            assert_eq!(first, instance.evaluate_reclaiming(&input.to_string()));
            assert_eq!(
                first,
                StandaloneWasm::new(&wasm).evaluate_reclaiming(&input.to_string()),
                "piece globals and broker state belong to the instance"
            );
        }
    }
}

/// A native heap guard inside an outlined region must abort immediately
/// with its original source PC and bar. Running again after the failure
/// must initialize the piece globals and broker as a fresh evaluation.
#[test]
fn a_split_strategy_propagates_native_stops_and_recovers_on_reuse() {
    let mut source = split_strategy_source(true);
    source.push_str(
        "var values = array.new<float>(1, 1.0)\n\
         bad = input.bool(false, 'Bad')\n\
         value = array.get(values, bad and bar_index == 17 ? 1 : 0)\nplot(value)\n",
    );
    let program = compile_csil(&source);
    let bars = &synthetic_bars()[..32];
    let mut options = RunOptions::default();
    options
        .inputs
        .insert(program.inputs[0].id, pinewasm_hir::InputValue::Bool(true));
    let expected = run(&program, &market_data(bars), &options);
    let error = expected.errors.first().expect("the VM must stop");
    assert_eq!(error.bar_index, Some(17));
    assert_eq!(
        error.kind,
        pinewasm_vm::RuntimeErrorKind::CollectionIndexOutOfBounds
    );
    let wasm = compile_strategy_standalone_wasm(&source);
    let mut instance = StandaloneWasm::new(&wasm);
    let good = wire(bars);
    let first = instance.evaluate_reclaiming(&good);
    let mut bad: serde_json::Value = serde_json::from_str(&good).unwrap();
    bad["params"]["bad"] = true.into();
    assert!(instance
        .evaluate(&bad.to_string())
        .unwrap_err()
        .contains("native heap error"));
    let header = |offset: u32| {
        let memory = instance.memory.data(&instance.store);
        u32::from_le_bytes(
            memory[offset as usize..offset as usize + 4]
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(header(pinewasm_aot::header::OFF_ERR_BAR), 17);
    assert_eq!(
        header(pinewasm_aot::header::OFF_ERR_PC),
        error.instruction_offset.unwrap()
    );
    assert_eq!(first, instance.evaluate_reclaiming(&good));
}

/// A CSIL Return normally ends the script body for one bar, not the whole run.
/// Replace an empty-stack branch exit with Return so even bars leave the
/// outlined body early; their pending orders must still get the close pass
/// and their broker must still record exactly one equity sample.
#[test]
fn a_split_strategy_body_return_still_finishes_the_broker_bar() {
    let source = split_strategy_source(true).replacen(
        "for j = 0 to 2\n",
        "if bar_index % 2 == 0\n    total += close\nelse\n    total -= open\nfor j = 0 to 2\n",
        1,
    );
    let mut program = compile_csil(&source);
    let main = &mut program.functions[0];
    let at = main
        .code
        .iter()
        .position(|op| matches!(op, pinewasm_csil::CsilInstr::Jump { .. }))
        .expect("the early conditional has an empty-stack then-arm exit");
    main.code[at] = pinewasm_csil::CsilInstr::Return;
    main.budget_reference = None;
    pinewasm_csil::validate(&program).expect("early-return fixture is valid CSIL");
    let wasm = compile_standalone_wasm_program(&program, &strategy_options())
        .expect("early-return strategy compiles");
    let bodies = wasmparser::Parser::new(0)
        .parse_all(&wasm)
        .filter_map(|payload| match payload.unwrap() {
            wasmparser::Payload::CodeSectionEntry(body) => Some(body),
            _ => None,
        })
        .collect::<Vec<_>>();
    let locals = bodies[bodies.len() - 2]
        .get_locals_reader()
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(
        locals,
        vec![(1, wasmparser::ValType::I32)],
        "the strategy must split"
    );
    let bars = &synthetic_bars()[..40];
    let expected = run(&program, &market_data(bars), &RunOptions::default());
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    assert_eq!(expected.strategy.as_ref().unwrap().equity.len(), bars.len());
    let mut input: serde_json::Value = serde_json::from_str(&wire(bars)).unwrap();
    input["include_results"] = true.into();
    let actual = StandaloneWasm::new(&wasm).evaluate_reclaiming(&input.to_string());
    assert_full_strategy_results(&serde_json::from_slice(&actual).unwrap(), &expected);
}

/// The small live-element budget cannot accommodate one three-element
/// temporary array per bar without collection. A persistent object's
/// array field retains the previous bar's live array across the next
/// collection. Read that array before allocating or assigning its
/// replacement, so losing the nested root is observable in the plots.
#[test]
fn split_strategy_pieces_collect_and_preserve_live_object_array_roots() {
    const BUDGET: usize = 20;
    const COUNT: usize = 81;
    let mut source = split_strategy_source(true);
    source.push_str(concat!(
        "type State\n    float level\n    array<float> values\n",
        "var State state = State.new(na, array.new_float(0))\n",
        "plot(bar_index > 0 ? array.avg(state.values) : na, 'Previous rooted array')\n",
        "recent = array.new_float(0)\n",
        "array.push(recent, close)\narray.push(recent, close[1])\narray.push(recent, close[2])\n",
        "state.level := array.max(recent)\nstate.values := recent\n",
        "plot(state.level)\nplot(array.avg(state.values))\n",
    ));
    let program = compile_csil(&source);
    let wasm = compile_standalone_wasm_program_with_config(
        &program,
        &strategy_options(),
        &StandaloneWasmBuildConfig {
            max_collection_elements: BUDGET as u64,
            ..StandaloneWasmBuildConfig::default()
        },
    )
    .expect("split strategy with collector compiles");
    let bodies = wasmparser::Parser::new(0)
        .parse_all(&wasm)
        .filter_map(|payload| match payload.unwrap() {
            wasmparser::Payload::CodeSectionEntry(body) => Some(body),
            _ => None,
        })
        .collect::<Vec<_>>();
    let step = &bodies[bodies.len() - 2];
    assert_eq!(
        step.get_locals_reader()
            .unwrap()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
        vec![(1, wasmparser::ValType::I32)],
        "the collector regression must use split pieces"
    );
    let ops = step
        .get_operators_reader()
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert!(
        ops.windows(8).any(|ops| matches!(
            ops,
            [
                wasmparser::Operator::GlobalGet { .. },
                wasmparser::Operator::GlobalGet { .. },
                wasmparser::Operator::I64GeS,
                wasmparser::Operator::GlobalGet { .. },
                wasmparser::Operator::GlobalGet { .. },
                wasmparser::Operator::I64LeS,
                wasmparser::Operator::I32Or,
                wasmparser::Operator::If {
                    blockty: wasmparser::BlockType::Empty
                },
            ]
        )),
        "the emitted wrapper must check its collector thresholds"
    );
    let bars = &synthetic_bars()[..COUNT];
    assert!(
        3 * bars.len() > BUDGET,
        "the fixture must require reclamation"
    );
    let expected = run(
        &program,
        &market_data(bars),
        &RunOptions {
            max_collection_elements: BUDGET,
            ..RunOptions::default()
        },
    );
    assert!(expected.errors.is_empty(), "{:?}", expected.errors);
    let previous = &expected.plots[expected.plots.len() - 3].values;
    let current = &expected.plots.last().unwrap().values;
    for bar in 3..bars.len() {
        assert_eq!(
            previous[bar],
            current[bar - 1],
            "previous rooted array at bar {bar}"
        );
    }
    assert!(previous.iter().zip(current).any(|(previous, current)| {
        matches!((previous, current), (Value::Float(p), Value::Float(c)) if p.is_finite() && c.is_finite() && p != c)
    }), "the retained previous array must differ from its replacement");
    let mut input: serde_json::Value = serde_json::from_str(&wire(bars)).unwrap();
    input["include_results"] = true.into();
    let mut instance = StandaloneWasm::new(&wasm);
    let first = instance.evaluate_reclaiming(&input.to_string());
    assert_full_strategy_results(&serde_json::from_slice(&first).unwrap(), &expected);
    for _ in 0..4 {
        assert_eq!(first, instance.evaluate_reclaiming(&input.to_string()));
    }
    assert_eq!(
        first,
        StandaloneWasm::new(&wasm).evaluate_reclaiming(&input.to_string()),
        "a fresh instance must reproduce collected heap and broker output"
    );
}

#[test]
fn explicit_currency_and_mintick_match_vm_and_fail_closed_without_matching_quote() {
    let source = "strategy(\"Ticks\", currency=currency.USD, process_orders_on_close=true)\nif close > open + syminfo.mintick * 2\n    strategy.entry(\"L\", strategy.long)\nif close < open - syminfo.mintick * 2\n    strategy.close(\"L\")\nplot(syminfo.mintick)\nplot(syminfo.mintick[1])\n";
    let bars = synthetic_bars();
    let program = compile_csil(source);
    let wasm = compile_strategy_standalone_wasm(source);
    let mut instance = StandaloneWasm::new(&wasm);
    for tick in [0.01, 0.25, 5.0] {
        let mut data = market_data(&bars);
        data.mintick = tick;
        data.syminfo.currency = "USD".into();
        let expected = run(&program, &data, &RunOptions::default());
        assert!(expected.errors.is_empty(), "{:?}", expected.errors);
        let mut input: serde_json::Value = serde_json::from_str(&wire(&bars)).unwrap();
        input["currency"] = "USD".into();
        input["mintick"] = tick.into();
        input["include_results"] = true.into();
        let bytes = instance.evaluate(&input.to_string()).unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(actual["plots"][0][0], tick);
        assert!(actual["plots"][1][0].is_null());
        assert_eq!(actual["plots"][1][1], tick);
        let strategy = expected.strategy.unwrap();
        assert_eq!(
            actual["strategy"]["trades"].as_array().unwrap().len(),
            strategy.trades.len()
        );
        for (a, e) in actual["strategy"]["trades"]
            .as_array()
            .unwrap()
            .iter()
            .zip(strategy.trades)
        {
            assert_eq!(a["entryPrice"], e.entry_price);
            assert_eq!(a["qty"], e.qty);
        }
        for currency in [None, Some("EUR"), Some("USDT")] {
            if let Some(currency) = currency {
                input["currency"] = currency.into();
            } else {
                input.as_object_mut().unwrap().remove("currency");
            }
            let error = instance.evaluate(&input.to_string()).unwrap_err();
            assert!(error.contains("matching dataset quote currency"), "{error}");
        }
    }
}

// ---------------------------------------------------------------------------
// The compact trade ledger (`trade_results: true`, `broker-trades-v1`).

/// Bars whose every price is an exact multiple of 0.25, derived with integer
/// arithmetic only. `synthetic_bars` goes through the platform's `sin` and
/// `cos`, which a differential tolerates (both runtimes read the same bars)
/// and a pinned output hash does not.
fn exact_bars(count: usize) -> Vec<Bar> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut level: i64 = 480;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let drift = match (i / 37) % 4 {
            0 => 3,
            1 => -4,
            2 => 5,
            _ => -3,
        };
        let open = level;
        level = (level + drift + (state % 13) as i64 - 6).max(80);
        let close = level;
        let high = open.max(close) + 1 + ((state >> 8) % 7) as i64;
        let low = open.min(close) - 1 - ((state >> 16) % 7) as i64;
        let quarter = |points: i64| points as f64 * 0.25;
        out.push(Bar {
            time: BAR0_TIME + (i as i64) * MS_PER_DAY,
            open: quarter(open),
            high: quarter(high),
            low: quarter(low),
            close: quarter(close),
            volume: 1_000.0 + ((state >> 24) % 500) as f64,
        });
    }
    out
}

/// The JSON request over `bars` with `fields`, a `"key":value,` sequence,
/// written verbatim ahead of the bars.
fn request(bars: &[Bar], fields: &str) -> String {
    let wire = wire(bars);
    assert!(wire.starts_with("{\"version\":1,\"params\":{},\"bars\":["));
    wire.replacen("\"params\":{},", &format!("\"params\":{{}},{fields}"), 1)
}

/// Bracket exits in ticks, a pyramided second entry, reversals, percent
/// commission, slippage and percent-of-equity sizing.
const PINNED_BRACKETS: &str = concat!(
    "//@version=6\n",
    "strategy(\"Pinned brackets\", initial_capital=25000, pyramiding=2, ",
    "default_qty_type=strategy.percent_of_equity, default_qty_value=20, ",
    "commission_type=strategy.commission.percent, commission_value=0.075, slippage=2)\n",
    "fast = ta.sma(close, 3)\n",
    "slow = ta.sma(close, 8)\n",
    "if ta.crossover(fast, slow)\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if strategy.position_size > 0 and close > fast and close > close[1]\n",
    "    strategy.entry(\"L2\", strategy.long)\n",
    "if ta.crossunder(fast, slow)\n",
    "    strategy.entry(\"S\", strategy.short)\n",
    "strategy.exit(\"XL\", \"L\", loss=16, profit=36)\n",
    "strategy.exit(\"XS\", \"S\", loss=16, profit=36)\n",
    "plot(fast)\n",
    "plot(strategy.equity)\n",
);

/// The close pass, partial closes, `close_all`, per-order commission and
/// fixed sizing.
const PINNED_CLOSE_PASS: &str = concat!(
    "//@version=6\n",
    "strategy(\"Pinned close pass\", initial_capital=10000, process_orders_on_close=true, ",
    "default_qty_type=strategy.fixed, default_qty_value=3, ",
    "commission_type=strategy.commission.cash_per_order, commission_value=1.25)\n",
    "if close > open and strategy.position_size == 0\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if close < open and strategy.position_size > 0\n",
    "    strategy.close(\"L\", qty_percent=50)\n",
    "if bar_index % 17 == 0\n",
    "    strategy.close_all()\n",
    "plot(strategy.netprofit)\n",
);

/// Every output a caller can request without the `trade_results` flag, as
/// SHA-256 of the returned bytes. The projection is opt-in, so a request
/// without the flag still returns exactly these bytes. The full and binary
/// results of the two trading fixtures are pinned at their current values: the
/// account drawdown sums every open lot's adverse move, and a `strategy.close`
/// or `close_all` exit carries its order name (`exitOrderName`).
const PINNED_OUTPUTS: [(&str, &str); 12] = [
    (
        "brackets/signals",
        "e036469126503b5f219a368a79ee2371fd37f86059d18a8562fcde27bef3b460",
    ),
    (
        "brackets/full",
        "2758cf56cc4bef787d124d7639139c884608f6aa3bfa195ef054299c30a76fd7",
    ),
    (
        "brackets/binary",
        "02183765ce5bdf5880cbe2208cd0b676637dc37e6376ef40702238bad87a7a4b",
    ),
    (
        "brackets/objectives",
        "5160357a593d8f85f3b462caeb59f5cabd61fdb283493af565d5d3ba1637edb1",
    ),
    (
        "close-pass/signals",
        "11794c0b5da6ea229d1302f2ff3bc1f75ff467b5a97b49c4e8a27831d106914a",
    ),
    (
        "close-pass/full",
        "a84e9dbbfe8dab81d82269f5dc0680ed52eab413bf45f9a2167a83c161a68ed5",
    ),
    (
        "close-pass/binary",
        "8b302ba4082e9f857c918fc04f359432599e7c93d79d9102a433732236d92f32",
    ),
    (
        "close-pass/objectives",
        "b2a339befa98d0037e72c780a8af374da77c2989c19fede95735ab5cbab5ce10",
    ),
    (
        "idle/signals",
        "9ce8f5b7fdf09324dff55e40f06aea99122fb36200e3aad0878cc9614397b35b",
    ),
    (
        "idle/full",
        "26058a78a6d43853a0316fe2e396fd531809693ea275b1bb776bd9dd01dc61fe",
    ),
    (
        "idle/binary",
        "fb57e491fbc88524ce6d491ce53b86b1a2df08fcfc5cdf727462040a3ac1f91d",
    ),
    (
        "idle/objectives",
        "223cca0c44930eb94cc1b9a579a62305b3d8f6f50edb7560b1e8a7350f4770dd",
    ),
];

#[test]
fn outputs_without_the_trade_results_flag_keep_their_updated_bytes() {
    let bars = exact_bars(360);
    let economics = "\"mintick\":0.25,\"mincontract\":0.001,\"pointvalue\":2.0,";
    let mut actual = Vec::new();
    for (name, source) in [
        ("brackets", PINNED_BRACKETS),
        ("close-pass", PINNED_CLOSE_PASS),
        ("idle", "strategy(\"Idle\")\nplot(close)\n"),
    ] {
        let mut instance = StandaloneWasm::new(&compile_strategy_standalone_wasm(source));
        for (mode, flags) in [
            ("signals", ""),
            ("full", "\"include_results\":true,"),
            (
                "binary",
                "\"include_results\":true,\"binary_results\":true,",
            ),
            ("objectives", "\"objective_results\":true,"),
        ] {
            let output = instance
                .evaluate(&request(&bars, &format!("{economics}{flags}")))
                .unwrap_or_else(|error| panic!("{name}/{mode}: {error}"));
            if mode == "full" && name != "idle" {
                // The pin means something only while the fixture trades.
                let full: serde_json::Value = serde_json::from_slice(&output).unwrap();
                let trades = full["strategy"]["trades"].as_array().unwrap().len();
                assert!(trades >= 10, "{name}: only {trades} closed trades");
                assert!(!full["signals"].as_array().unwrap().is_empty(), "{name}");
            }
            // A disabled flag is the absent flag.
            let disabled = request(
                &bars,
                &format!("{economics}{flags}\"trade_results\":false,"),
            );
            assert_eq!(
                instance.evaluate(&disabled).unwrap(),
                output,
                "{name}/{mode}: trade_results=false changed the output"
            );
            actual.push((format!("{name}/{mode}"), sha256_hex(&output)));
        }
    }
    let expected: Vec<(String, String)> = PINNED_OUTPUTS
        .iter()
        .map(|(name, hash)| ((*name).to_owned(), (*hash).to_owned()))
        .collect();
    assert_eq!(actual, expected, "the reviewed output bytes moved");
}

/// The symbol economics one differential hands to both runtimes.
#[derive(Clone, Copy)]
struct Economics {
    mintick: f64,
    mincontract: f64,
    pointvalue: f64,
    currency: &'static str,
}

/// Both runtimes' defaults, spelled out.
const DEFAULT_ECONOMICS: Economics = Economics {
    mintick: 0.01,
    mincontract: 1.0,
    pointvalue: 1.0,
    currency: "",
};

impl Economics {
    /// The request fields, for [`request`].
    fn fields(self) -> String {
        let mut fields = format!(
            "\"mintick\":{:?},\"mincontract\":{:?},\"pointvalue\":{:?},",
            self.mintick, self.mincontract, self.pointvalue
        );
        if !self.currency.is_empty() {
            fields.push_str(&format!("\"currency\":\"{}\",", self.currency));
        }
        fields
    }

    fn market_data(self, bars: &[Bar]) -> MarketData {
        let mut data = market_data(bars);
        data.mintick = self.mintick;
        data.syminfo.mincontract = self.mincontract;
        data.syminfo.pointvalue = self.pointvalue;
        self.currency.clone_into(&mut data.syminfo.currency);
        data
    }
}

/// The `strategy` object's fields, in document order. The three first-entry
/// fields exist once an opening leg has filled.
const LEDGER_KEYS: [&str; 24] = [
    "initialCapital",
    "mintick",
    "pointValue",
    "slippageTicks",
    "accountCurrency",
    "symbolCurrency",
    "currencyConversionComplete",
    "firstRetainedTradeIndex",
    "closedTrades",
    "firstEntryBar",
    "firstEntryPrice",
    "firstEntryAccountNotionalPerUnit",
    "netProfit",
    "grossProfit",
    "grossLoss",
    "openProfit",
    "maxDrawdown",
    "maxDrawdownPercent",
    "openEntryCommission",
    "commissionPaid",
    "commissionPaidLong",
    "commissionPaidShort",
    "position",
    "trades",
];

/// One ledger row's fields, in document order.
const LEDGER_ROW_KEYS: [&str; 17] = [
    "index",
    "entryId",
    "exitId",
    "sign",
    "qty",
    "entryBar",
    "exitBar",
    "entryPrice",
    "exitPrice",
    "pnl",
    "entryNotional",
    "entryCommission",
    "exitCommission",
    "commission",
    "cumulativePnl",
    "maxRunup",
    "maxDrawdown",
];

fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object, got {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort_unstable();
    keys
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    names.sort_unstable();
    names
}

/// A ledger number: a finite JSON number, and never negative zero.
fn num(object: &serde_json::Value, key: &str) -> f64 {
    let field = &object[key];
    let number = field
        .as_f64()
        .filter(|_| field.is_number())
        .unwrap_or_else(|| panic!("{key} must be a JSON number, got {field}"));
    assert!(
        number.is_finite() && (number != 0.0 || number.is_sign_positive()),
        "{key} must be finite and unsigned at zero, got {number:?}"
    );
    number
}

/// A ledger count: a JSON integer token, readable as an integer or a float.
fn int(object: &serde_json::Value, key: &str) -> u64 {
    object[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} must be a JSON integer, got {}", object[key]))
}

fn rows(document: &serde_json::Value) -> &[serde_json::Value] {
    document["strategy"]["trades"]
        .as_array()
        .expect("trades array")
}

/// Bit equality, except that a zero carries no sign on this wire.
fn assert_recomputed(context: &str, recomputed: f64, reported: f64) {
    let canonical = if recomputed == 0.0 { 0.0 } else { recomputed };
    assert_eq!(
        canonical.to_bits(),
        reported.to_bits(),
        "{context}: recomputed {recomputed:?}, reported {reported:?}"
    );
}

/// Whether the retained rows are the whole run. A zero first index is not
/// enough: an evicted row can sit between retained ones.
fn ledger_is_complete(document: &serde_json::Value) -> bool {
    let strategy = &document["strategy"];
    let rows = rows(document);
    int(strategy, "firstRetainedTradeIndex") == 0
        && rows.len() as u64 == int(strategy, "closedTrades")
        && rows
            .iter()
            .enumerate()
            .all(|(position, row)| int(row, "index") == position as u64)
}

/// What a verifier that never runs the module derives from the document
/// alone: each row's P&L, commission and notional in the broker's operation
/// order, each fill inside its bar, and, for a complete ledger, the full-run
/// totals as sequential close-order sums. The commission totals follow from
/// the rows only as far as the rows go: a lot still open carries commission
/// that no row holds.
fn assert_ledger_recomputes(name: &str, document: &serde_json::Value, bars: &[Bar]) {
    let strategy = &document["strategy"];
    assert_eq!(int(document, "bars"), bars.len() as u64, "{name}: bars");
    let point_value = num(strategy, "pointValue");
    // One tick of rounding beyond the declared slippage: fills are quantized.
    let reach = (int(strategy, "slippageTicks") as f64 + 1.0) * num(strategy, "mintick");
    // The standalone broker admits identity conversion only.
    assert_eq!(strategy["currencyConversionComplete"], true, "{name}");
    assert_eq!(
        strategy["accountCurrency"], strategy["symbolCurrency"],
        "{name}"
    );
    let exit_currency_rate = 1.0;
    let complete = ledger_is_complete(document);
    let (mut realized, mut gross_profit, mut gross_loss) = (0.0f64, 0.0f64, 0.0f64);
    // The closed trades' commission: all, long, short.
    let mut closed_commission = [0.0f64; 3];
    let mut previous: Option<(u64, u64)> = None;
    for row in rows(document) {
        let index = int(row, "index");
        let context = |field: &str| format!("{name}: trade {index} {field}");
        let sign = row["sign"].as_i64().expect("integer sign") as f64;
        assert!(sign == 1.0 || sign == -1.0, "{}", context("sign"));
        let (qty, entry, exit) = (
            num(row, "qty"),
            num(row, "entryPrice"),
            num(row, "exitPrice"),
        );
        let (entry_commission, exit_commission) =
            (num(row, "entryCommission"), num(row, "exitCommission"));
        assert!(qty > 0.0, "{}", context("qty"));
        assert!(
            entry_commission >= 0.0 && exit_commission >= 0.0,
            "{}",
            context("commission split")
        );
        assert_recomputed(
            &context("pnl"),
            (exit - entry) * qty * sign * point_value * exit_currency_rate
                - exit_commission
                - entry_commission,
            num(row, "pnl"),
        );
        assert_recomputed(
            &context("commission"),
            entry_commission + exit_commission,
            num(row, "commission"),
        );
        assert_recomputed(
            &context("entryNotional"),
            entry.abs() * point_value * 1.0 * qty,
            num(row, "entryNotional"),
        );
        assert!(num(row, "maxRunup") >= 0.0 && num(row, "maxDrawdown") >= 0.0);

        let (entry_bar, exit_bar) = (int(row, "entryBar"), int(row, "exitBar"));
        assert!(
            entry_bar <= exit_bar && exit_bar < bars.len() as u64,
            "{}",
            context("bars")
        );
        for (bar, price) in [(entry_bar, entry), (exit_bar, exit)] {
            let bar = &bars[bar as usize];
            assert!(
                bar.low - reach <= price && price <= bar.high + reach,
                "{}: {price} outside [{}, {}] by more than {reach}",
                context("fill"),
                bar.low,
                bar.high
            );
        }
        // Close order: indices strictly increase and exits never go back.
        if let Some((last_index, last_exit)) = previous {
            assert!(
                index > last_index && exit_bar >= last_exit,
                "{}",
                context("order")
            );
        }
        previous = Some((index, exit_bar));

        if complete {
            let pnl = num(row, "pnl");
            realized += pnl;
            assert_recomputed(
                &context("cumulativePnl"),
                realized,
                num(row, "cumulativePnl"),
            );
            if pnl > 0.0 {
                gross_profit += pnl;
            } else if pnl < 0.0 {
                gross_loss += -pnl;
            }
            let commission = num(row, "commission");
            closed_commission[0] += commission;
            closed_commission[if sign > 0.0 { 1 } else { 2 }] += commission;
        }
    }
    let paid = [
        num(strategy, "commissionPaid"),
        num(strategy, "commissionPaidLong"),
        num(strategy, "commissionPaidShort"),
    ];
    assert!(
        paid.iter().all(|total| *total >= 0.0),
        "{name}: commission paid {paid:?}"
    );
    if complete {
        for (field, recomputed) in [
            ("netProfit", realized - num(strategy, "openEntryCommission")),
            ("grossProfit", gross_profit),
            (
                "grossLoss",
                gross_loss + num(strategy, "openEntryCommission"),
            ),
        ] {
            assert_recomputed(
                &format!("{name}: {field}"),
                recomputed,
                num(strategy, field),
            );
        }
        // A scope with no lot open paid exactly what its rows hold. The open
        // position's own direction, and the run as a whole, paid that and
        // the opening commission the open lots still carry.
        let size = num(&strategy["position"], "size");
        for (scope, field, holds_open_lots) in [
            (0, "commissionPaid", size != 0.0),
            (1, "commissionPaidLong", size > 0.0),
            (2, "commissionPaidShort", size < 0.0),
        ] {
            if holds_open_lots {
                assert!(
                    paid[scope] >= closed_commission[scope],
                    "{name}: {field} {} is under its closed trades' {}",
                    paid[scope],
                    closed_commission[scope]
                );
            } else {
                assert_recomputed(
                    &format!("{name}: {field}"),
                    closed_commission[scope],
                    paid[scope],
                );
            }
        }
    }
}

/// Evaluate `source` with `trade_results` and hold every field of the
/// document to the interpreter's strategy outcome on the same bars and
/// economics, with no tolerance. Returns the document for the caller's own
/// scenario checks.
fn assert_trade_results(
    name: &str,
    source: &str,
    bars: &[Bar],
    economics: Economics,
) -> serde_json::Value {
    let program = compile_csil(source);
    let declared = program.metadata.strategy.as_ref().expect("strategy source");
    let reference = run(
        &program,
        &economics.market_data(bars),
        &RunOptions::default(),
    );
    assert!(
        reference.errors.is_empty(),
        "{name}: {:?}",
        reference.errors
    );
    let expected = reference.strategy.as_ref().expect("strategy outcome");

    let wasm = compile_strategy_standalone_wasm(source);
    pinewasm_aot::audit_standalone_wasm(&wasm)
        .unwrap_or_else(|error| panic!("{name}: standalone audit: {error}"));
    let fields = economics.fields();
    let output = StandaloneWasm::new(&wasm)
        .evaluate(&request(bars, &format!("{fields}\"trade_results\":true,")))
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    let text = std::str::from_utf8(&output).expect("UTF-8 document");

    // The envelope. The default document's posture events are not in it:
    // there can be one for every bar, and this document must stay bounded.
    let head = format!(
        "{{\"version\":1,\"projection\":\"broker-trades-v1\",\"bars\":{},\"strategy\":{{\"initialCapital\":",
        bars.len()
    );
    assert!(
        text.starts_with(&head),
        "{name}: the document must open with {head}"
    );
    let document: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(
        sorted_keys(&document),
        sorted(&["version", "projection", "bars", "strategy"]),
        "{name}"
    );

    let strategy = &document["strategy"];
    let traded = expected.first_entry_bar.is_some();
    let ledger_keys: Vec<&str> = LEDGER_KEYS
        .into_iter()
        .filter(|key| traded || !key.starts_with("firstEntry"))
        .collect();
    assert_eq!(sorted_keys(strategy), sorted(&ledger_keys), "{name}");
    let same = |field: &str, module: f64, interpreter: f64| {
        assert!(
            module == interpreter,
            "{name}: {field} drifted: module {module:?}, interpreter {interpreter:?}"
        );
    };
    same(
        "initialCapital",
        num(strategy, "initialCapital"),
        expected.initial_capital,
    );
    same("mintick", num(strategy, "mintick"), economics.mintick);
    same(
        "pointValue",
        num(strategy, "pointValue"),
        economics.pointvalue,
    );
    assert_eq!(
        int(strategy, "slippageTicks"),
        u64::from(declared.slippage),
        "{name}"
    );
    assert_eq!(
        strategy["accountCurrency"].as_str(),
        Some(expected.account_currency.as_str()),
        "{name}"
    );
    assert_eq!(
        strategy["symbolCurrency"].as_str(),
        Some(expected.symbol_currency.as_str()),
        "{name}"
    );
    assert_eq!(
        strategy["currencyConversionComplete"].as_bool(),
        Some(expected.currency_conversion_complete),
        "{name}"
    );
    assert_eq!(
        int(strategy, "firstRetainedTradeIndex"),
        u64::from(
            expected
                .first_retained_trade_index
                .expect("retention index")
        ),
        "{name}"
    );
    assert_eq!(
        int(strategy, "closedTrades"),
        u64::from(expected.closed_trades),
        "{name}"
    );
    if traded {
        assert_eq!(
            Some(int(strategy, "firstEntryBar")),
            expected.first_entry_bar.map(u64::from),
            "{name}"
        );
        same(
            "firstEntryPrice",
            num(strategy, "firstEntryPrice"),
            expected.first_entry_price.expect("first entry price"),
        );
        same(
            "firstEntryAccountNotionalPerUnit",
            num(strategy, "firstEntryAccountNotionalPerUnit"),
            expected
                .first_entry_account_notional_per_unit
                .expect("first entry notional"),
        );
    }
    for (field, interpreter) in [
        ("netProfit", expected.net_profit),
        ("grossProfit", expected.gross_profit),
        ("grossLoss", expected.gross_loss),
        ("openProfit", expected.open_profit),
        ("maxDrawdown", expected.max_drawdown),
        (
            "maxDrawdownPercent",
            expected.max_drawdown_percent.expect("drawdown percent"),
        ),
        (
            "openEntryCommission",
            expected
                .open_entry_commission
                .expect("open entry commission"),
        ),
    ] {
        same(field, num(strategy, field), interpreter);
    }
    // Commission paid is the aggregate's, scope by scope: the closed trades'
    // and what the lots still open carry. The module sums it in place; the
    // interpreter builds the whole aggregate set.
    let report = expected
        .report_aggregates
        .as_ref()
        .expect("report aggregates");
    for (field, interpreter) in [
        ("commissionPaid", report.all.commission),
        ("commissionPaidLong", report.long.commission),
        ("commissionPaidShort", report.short.commission),
    ] {
        same(field, num(strategy, field), interpreter);
    }
    let position = &strategy["position"];
    assert_eq!(
        sorted_keys(position),
        sorted(&["size", "avgPrice"]),
        "{name}"
    );
    same(
        "position.size",
        num(position, "size"),
        expected.position.size,
    );
    same(
        "position.avgPrice",
        num(position, "avgPrice"),
        expected.position.avg_price,
    );

    let rows = rows(&document);
    assert_eq!(rows.len(), expected.trades.len(), "{name}: retained rows");
    for (row, trade) in rows.iter().zip(&expected.trades) {
        let mut keys = LEDGER_ROW_KEYS.to_vec();
        if trade.comment.is_some() {
            keys.push("comment");
        }
        if trade.exit_order_name.is_some() {
            keys.push("exitOrderName");
        }
        assert_eq!(sorted_keys(row), sorted(&keys), "{name}");
        assert_eq!(
            row.get("comment").and_then(|v| v.as_str()),
            trade.comment.as_deref(),
            "{name}: comment"
        );
        assert_eq!(
            row.get("exitOrderName").and_then(|v| v.as_str()),
            trade.exit_order_name.as_deref(),
            "{name}: exit name"
        );
        let index = u64::from(trade.index.expect("stamped index"));
        assert_eq!(int(row, "index"), index, "{name}");
        assert_eq!(
            row["entryId"].as_str(),
            Some(trade.entry_id.as_str()),
            "{name}: trade {index}"
        );
        assert_eq!(
            row["exitId"].as_str(),
            Some(trade.exit_id.as_deref().unwrap_or("")),
            "{name}: trade {index}"
        );
        assert_eq!(
            row["sign"].as_i64().map(|sign| sign as f64),
            Some(trade.sign),
            "{name}: trade {index} sign"
        );
        assert_eq!(
            (int(row, "entryBar"), int(row, "exitBar")),
            (u64::from(trade.entry_bar), u64::from(trade.exit_bar)),
            "{name}: trade {index} bars"
        );
        for (field, interpreter) in [
            ("qty", trade.qty),
            ("entryPrice", trade.entry_price),
            ("exitPrice", trade.exit_price),
            ("pnl", trade.pnl),
            (
                "entryNotional",
                trade.entry_notional.expect("entry notional"),
            ),
            ("commission", trade.commission),
            (
                "cumulativePnl",
                trade.cumulative_pnl.expect("cumulative P&L"),
            ),
            ("maxRunup", trade.max_runup),
            ("maxDrawdown", trade.max_drawdown),
        ] {
            same(
                &format!("trade {index} {field}"),
                num(row, field),
                interpreter,
            );
        }
    }
    // The headline counts a consumer derives from row signs are the
    // interpreter's own whenever the ledger is whole.
    if ledger_is_complete(&document) {
        let count =
            |keep: fn(f64) -> bool| rows.iter().filter(|row| keep(num(row, "pnl"))).count() as u32;
        assert_eq!(
            (
                count(|pnl| pnl > 0.0),
                count(|pnl| pnl < 0.0),
                count(|pnl| pnl == 0.0)
            ),
            (
                expected.win_trades,
                expected.loss_trades,
                expected.even_trades
            ),
            "{name}: winners, losers and even trades"
        );
    }
    assert_ledger_recomputes(name, &document, bars);
    document
}

/// The module's ledger with nothing else: the document for `fields`.
fn trade_document(source: &str, bars: &[Bar], fields: &str) -> Vec<u8> {
    StandaloneWasm::new(&compile_strategy_standalone_wasm(source))
        .evaluate(&request(bars, &format!("{fields}\"trade_results\":true,")))
        .expect("trade results")
}

fn round_trip_source(sizing: &str, commission: &str, process_on_close: bool) -> String {
    format!(
        concat!(
            "//@version=6\n",
            "strategy(\"Round trips\", initial_capital=20000, {}, {}, slippage=1, ",
            "process_orders_on_close={})\n",
            "fast = ta.sma(close, 4)\n",
            "slow = ta.sma(close, 9)\n",
            "if ta.crossover(fast, slow)\n",
            "    strategy.entry(\"L\", strategy.long)\n",
            "if ta.crossunder(fast, slow)\n",
            "    strategy.close(\"L\")\n",
            "plot(fast)\n",
        ),
        sizing, commission, process_on_close
    )
}

#[test]
fn trade_results_match_the_interpreter_across_sizing_commission_and_fill_timing() {
    // Market entries and exits under every default sizing, every commission
    // type, and both fill timings, on a symbol with a fractional quantity
    // step and a contract multiplier.
    let bars = synthetic_bars();
    let economics = Economics {
        mintick: 0.05,
        mincontract: 0.001,
        pointvalue: 2.5,
        currency: "",
    };
    for sizing in [
        "default_qty_type=strategy.fixed, default_qty_value=3",
        "default_qty_type=strategy.cash, default_qty_value=2500",
        "default_qty_type=strategy.percent_of_equity, default_qty_value=40",
    ] {
        for (commission, kind) in [
            (
                "commission_type=strategy.commission.percent, commission_value=0.1",
                "percent",
            ),
            (
                "commission_type=strategy.commission.cash_per_contract, commission_value=0.02",
                "contract",
            ),
            (
                "commission_type=strategy.commission.cash_per_order, commission_value=1.5",
                "order",
            ),
        ] {
            for process_on_close in [false, true] {
                let name = format!("{sizing} / {kind} / close pass {process_on_close}");
                let source = round_trip_source(sizing, commission, process_on_close);
                let document = assert_trade_results(&name, &source, &bars, economics);
                let rows = rows(&document);
                assert!(rows.len() >= 8, "{name}: only {} closed trades", rows.len());
                assert!(ledger_is_complete(&document), "{name}");
                for row in rows {
                    // An order placed on a bar fills on its close pass, or
                    // on the next bar's open.
                    assert!(int(row, "entryBar") < int(row, "exitBar"), "{name}");
                    assert_eq!(row["entryId"], "L", "{name}");
                    let (qty, entry, exit) = (
                        num(row, "qty"),
                        num(row, "entryPrice"),
                        num(row, "exitPrice"),
                    );
                    let (entry_commission, exit_commission) =
                        (num(row, "entryCommission"), num(row, "exitCommission"));
                    // Each charge belongs to its own fill. The broker rounds
                    // an order's commission to ten significant digits.
                    let near = |charge: f64, expected: f64| (charge / expected - 1.0).abs() < 1e-9;
                    match kind {
                        "percent" => {
                            assert!(near(entry_commission, qty * entry * 2.5 * 0.001), "{name}");
                            assert!(near(exit_commission, qty * exit * 2.5 * 0.001), "{name}");
                        }
                        "contract" => {
                            assert!(near(entry_commission, qty * 0.02), "{name}");
                            assert!(near(exit_commission, qty * 0.02), "{name}");
                        }
                        _ => {
                            assert_eq!(entry_commission, 1.5 / qty * qty, "{name}");
                            assert_eq!(exit_commission, 1.5, "{name}");
                        }
                    }
                }
                if sizing.contains("fixed") {
                    assert!(rows.iter().all(|row| num(row, "qty") == 3.0), "{name}");
                } else {
                    // Cash and equity sizing land on the fractional step.
                    assert!(
                        rows.iter().any(|row| num(row, "qty").fract() != 0.0),
                        "{name}: sizing never used the quantity step"
                    );
                }
            }
        }
    }
}

#[test]
fn sliced_trade_results_preserve_observable_order_comments_and_names() {
    let bars = synthetic_bars();
    for (name, exit, comment) in [
        ("close", "if bar_index == 3\n    strategy.close(\"L\", comment = \"close\")\n", Some("close")),
        ("conditional slot", "reason = bar_index % 2 == 0 ? \"even\" : \"odd\"\nif bar_index == 3\n    strategy.close(\"L\", comment = reason)\n", Some("odd")),
        ("empty comment", "if bar_index == 3\n    strategy.close(\"L\", comment = \"\")\n", None),
        ("escaped", r#"if bar_index == 3
    strategy.close("L", comment = "雪 \"quoted\" \\ tail")
"#, Some("雪 \"quoted\" \\ tail")),
        ("close all", "if bar_index == 3\n    strategy.close_all(comment = \"all\")\n", Some("all")),
        ("close name", "if bar_index == 3\n    strategy.close(\"L\")\n", None),
        ("close all name", "if bar_index == 3\n    strategy.close_all()\n", None),
        ("entry reversal", "if bar_index == 3\n    strategy.entry(\"S\", strategy.short, comment = \"reverse\")\n", Some("reverse")),
        ("order", "if bar_index == 3\n    strategy.order(\"O\", strategy.short, qty = 1, comment = \"order\")\n", Some("order")),
        ("exit", "strategy.exit(\"X\", \"L\", profit = 1, comment = \"exit\")\n", Some("exit")),
        ("profit", "strategy.exit(\"X\", \"L\", profit = 1, comment = \"fallback\", comment_profit = \"profit\")\n", Some("profit")),
        ("loss", "strategy.exit(\"X\", \"L\", loss = 1, comment = \"fallback\", comment_loss = \"loss\")\n", Some("loss")),
        ("trailing", "strategy.exit(\"X\", \"L\", trail_points = 1, trail_offset = 1, comment = \"fallback\", comment_trailing = \"trailing\")\n", Some("trailing")),
    ] {
        let source = format!("strategy(\"Comments\", default_qty_type = strategy.fixed, default_qty_value = 1)\nif bar_index == 0\n    strategy.entry(\"L\", strategy.long, comment = \"entry\")\n{exit}");
        let program = compile_csil(&source);
        let reference = run(&program, &market_data(&bars), &RunOptions::default());
        assert!(reference.errors.is_empty(), "{name}: {:?}", reference.errors);
        let expected = reference.strategy.as_ref().expect("strategy results");
        let wasm = compile_strategy_sliced(&source);
        for fields in ["\"trade_results\":true,", "\"include_results\":true,", "\"include_results\":true,\"binary_results\":true,"] {
            let output = StandaloneWasm::new(&wasm).evaluate(&request(&bars, fields)).expect("full results");
            let document: serde_json::Value = if fields.contains("binary_results") { decode_binary_results(&output) } else { serde_json::from_slice(&output).expect("valid JSON") };
            let trades = document["strategy"]["trades"].as_array().expect("trades");
            assert_eq!(trades.len(), expected.trades.len(), "{name}");
            if fields.contains("trade_results") {
                for (row, trade) in trades.iter().zip(&expected.trades) {
                    let mut keys = LEDGER_ROW_KEYS.to_vec();
                    if trade.comment.is_some() { keys.push("comment"); }
                    if trade.exit_order_name.is_some() { keys.push("exitOrderName"); }
                    assert_eq!(sorted_keys(row), sorted(&keys), "{name}: compact keys");
                }
            }
            for (row, trade) in trades.iter().zip(&expected.trades) {
                assert_eq!(row.get("comment").and_then(|v| v.as_str()), trade.comment.as_deref(), "{name}: comment");
                assert_eq!(row.get("exitOrderName").and_then(|v| v.as_str()), trade.exit_order_name.as_deref(), "{name}: exit name");
                assert_eq!(row["entryId"], trade.entry_id, "{name}");
                assert_eq!(row["exitId"].as_str(), Some(trade.exit_id.as_deref().unwrap_or("")), "{name}");
            }
        assert!(!trades.is_empty(), "{name}: scenario must close a trade");
        assert_eq!(trades[0].get("comment").and_then(|value| value.as_str()), comment, "{name}");
        if comment.is_none() {
            let expected = if name != "close all name" { "Close entry(s) order L" } else { "Close position order" };
            assert_eq!(trades[0]["exitOrderName"], expected, "{name}");
        }
        }
    }
}

#[test]
fn trade_results_carry_limit_and_stop_entries_and_bracket_exits() {
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Orders\", initial_capital=50000, default_qty_type=strategy.fixed, ",
        "default_qty_value=2, commission_type=strategy.commission.cash_per_contract, ",
        "commission_value=0.03, slippage=2)\n",
        "flat = strategy.position_size == 0\n",
        "if flat and bar_index % 9 == 0\n",
        "    strategy.entry(\"Stop\", strategy.long, stop = high + 0.4)\n",
        "if flat and bar_index % 9 == 4\n",
        "    strategy.cancel_all()\n",
        "    strategy.entry(\"Limit\", strategy.long, limit = low - 0.4)\n",
        "if flat and bar_index % 9 == 7\n",
        "    strategy.cancel_all()\n",
        "    strategy.entry(\"Market\", strategy.short)\n",
        "strategy.exit(\"XS\", \"Stop\", loss = 150, profit = 250)\n",
        "strategy.exit(\"XL\", \"Limit\", stop = strategy.position_avg_price - 2.5, ",
        "limit = strategy.position_avg_price + 3.5)\n",
        "strategy.exit(\"XM\", \"Market\", loss = 200, profit = 200)\n",
        "plot(close)\n",
    );
    let bars = synthetic_bars();
    let document = assert_trade_results("orders", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    assert!(ledger_is_complete(&document));
    for (entry, exit) in [("Stop", "XS"), ("Limit", "XL"), ("Market", "XM")] {
        let of_entry: Vec<_> = rows.iter().filter(|row| row["entryId"] == entry).collect();
        assert!(of_entry.len() >= 3, "{entry}: {} trades", of_entry.len());
        assert!(
            of_entry.iter().all(|row| row["exitId"] == exit),
            "{entry} trades close through {exit}"
        );
        // Both legs of the bracket fire: a target and a stop.
        assert!(of_entry.iter().any(|row| num(row, "pnl") > 0.0), "{entry}");
        assert!(of_entry.iter().any(|row| num(row, "pnl") < 0.0), "{entry}");
    }
    // Each entry price follows from the bars alone. An order is placed on
    // the latest bar of its residue before the fill (the next residue's
    // `cancel_all` would have removed it). Under PineWASM's broker rules, a limit fills at its price or the
    // open, whichever is better, with no slippage; a stop at its price or
    // the open, whichever is worse, and a market order at the open, each two
    // ticks worse; every fill is then rounded to the tick. The declared
    // price is first moved onto the tick grid, toward the harder-to-reach
    // side: a buy limit rounds down and a buy stop rounds up.
    let tick = |price: f64| (price / 0.01).round() * 0.01;
    let floor_tick = |price: f64| (price / 0.01 + 1e-9).floor() * 0.01;
    let ceil_tick = |price: f64| (price / 0.01 - 1e-9).ceil() * 0.01;
    let placed = |row: &serde_json::Value, residue: usize| {
        let fill = int(row, "entryBar") as usize;
        (0..fill).rev().find(|bar| bar % 9 == residue).unwrap()
    };
    for row in rows {
        let open = bars[int(row, "entryBar") as usize].open;
        let expected = match row["entryId"].as_str().unwrap() {
            "Limit" => tick(open.min(floor_tick(bars[placed(row, 4)].low - 0.4))),
            "Stop" => tick(open.max(ceil_tick(bars[placed(row, 0)].high + 0.4)) + 2.0 * 0.01),
            "Market" => {
                assert_eq!(placed(row, 7) + 1, int(row, "entryBar") as usize);
                tick(open - 2.0 * 0.01)
            }
            other => panic!("unexpected entry {other}"),
        };
        assert_eq!(num(row, "entryPrice"), expected, "{row}");
    }
    assert!(rows
        .iter()
        .any(|row| row["sign"] == -1 && row["entryId"] == "Market"));
}

/// A bar whose prices are all exact, for fills a test can compute by hand.
fn bar(index: usize, open: f64, high: f64, low: f64, close: f64) -> Bar {
    Bar {
        time: BAR0_TIME + (index as i64) * MS_PER_DAY,
        open,
        high,
        low,
        close,
        volume: 1_000.0,
    }
}

#[test]
fn a_position_opened_and_stopped_inside_one_bar_is_a_closed_trade() {
    // Every third bar places a market entry with a stop one point under its
    // close. The next bar opens above that stop, trades up, then through it:
    // the position opens and closes inside the bar, so the bar-close posture
    // never leaves flat and the default document reports nothing at all.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Same bar stop\", initial_capital=10000, ",
        "commission_type=strategy.commission.percent, commission_value=0.1)\n",
        "if bar_index % 3 == 0\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "    strategy.exit(\"X\", \"L\", stop = close - 1.0)\n",
        "plot(close)\n",
    );
    let bars: Vec<Bar> = (0..30)
        .map(|index| match index % 3 {
            0 => bar(index, 100.0, 101.0, 99.5, 100.0),
            1 => bar(index, 100.5, 101.0, 98.0, 99.5),
            _ => bar(index, 99.5, 100.5, 99.25, 100.0),
        })
        .collect();
    assert_eq!(
        StandaloneWasm::new(&compile_strategy_standalone_wasm(source))
            .evaluate(&request(&bars, &DEFAULT_ECONOMICS.fields()))
            .unwrap(),
        b"{\"version\":1,\"signals\":[]}",
        "the posture basis sees no trade"
    );
    let document = assert_trade_results("same-bar stop", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    assert_eq!(rows.len(), 10);
    assert!(ledger_is_complete(&document));
    for (position, row) in rows.iter().enumerate() {
        let fill_bar = 3 * position as u64 + 1;
        assert_eq!(
            (int(row, "entryBar"), int(row, "exitBar")),
            (fill_bar, fill_bar)
        );
        assert_eq!(
            (num(row, "entryPrice"), num(row, "exitPrice")),
            (100.5, 99.0)
        );
        assert_eq!(
            (row["entryId"].as_str(), row["exitId"].as_str()),
            (Some("L"), Some("X"))
        );
        // A point and a half lost, and both fills' commission on top.
        assert!(num(row, "entryCommission") > 0.0 && num(row, "exitCommission") > 0.0);
        assert!(num(row, "pnl") < -1.5);
    }
    let strategy = &document["strategy"];
    assert_eq!(num(strategy, "grossProfit"), 0.0);
    assert_eq!(num(strategy, "grossLoss"), -num(strategy, "netProfit"));
    assert_eq!(num(&strategy["position"], "size"), 0.0);
    assert_eq!(num(strategy, "openProfit"), 0.0);
}

#[test]
fn a_stop_out_and_same_direction_reentry_inside_one_bar_are_two_trades() {
    // The stop fills during the bar and the script, seeing a flat position
    // at the close, re-enters on that bar's close pass. The bar-close posture
    // is long before and after, so the default document merges every trade
    // into one position that never closes.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Reentry\", initial_capital=10000, process_orders_on_close=true, ",
        "commission_type=strategy.commission.cash_per_contract, commission_value=0.05)\n",
        "if strategy.position_size == 0\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "strategy.exit(\"X\", \"L\", loss = 200)\n",
        "plot(close)\n",
    );
    let bars = [
        bar(0, 100.0, 101.0, 99.5, 100.0),
        bar(1, 100.0, 100.5, 97.0, 99.0),
        bar(2, 99.0, 103.0, 98.5, 102.0),
        bar(3, 102.0, 102.5, 96.0, 97.0),
        bar(4, 97.0, 98.0, 94.0, 96.0),
        bar(5, 96.0, 99.0, 95.5, 98.5),
    ];
    let legacy = StandaloneWasm::new(&compile_strategy_standalone_wasm(source))
        .evaluate(&request(&bars, &DEFAULT_ECONOMICS.fields()))
        .unwrap();
    let signals = posture_signals(&legacy, &bars, bars.len());
    assert_eq!(signals.len(), 1, "one posture event: long from bar 0");
    assert_eq!(signals[0].1, 1);
    let document = assert_trade_results("re-entry", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    let fills: Vec<(u64, f64, u64, f64)> = rows
        .iter()
        .map(|row| {
            (
                int(row, "entryBar"),
                num(row, "entryPrice"),
                int(row, "exitBar"),
                num(row, "exitPrice"),
            )
        })
        .collect();
    assert_eq!(
        fills,
        [(0, 100.0, 1, 98.0), (1, 99.0, 3, 97.0), (3, 97.0, 4, 95.0)]
    );
    for pair in rows.windows(2) {
        assert_eq!(int(&pair[0], "exitBar"), int(&pair[1], "entryBar"));
        assert_eq!(
            (pair[0]["sign"].as_i64(), pair[1]["sign"].as_i64()),
            (Some(1), Some(1))
        );
    }
    assert!(rows.iter().all(|row| num(row, "pnl") == -2.0 - 0.05 - 0.05));
    // The fourth position, opened on bar 4's close, is still open.
    let strategy = &document["strategy"];
    assert_eq!(int(strategy, "closedTrades"), 3);
    assert_eq!(num(&strategy["position"], "size"), 1.0);
    assert_eq!(num(&strategy["position"], "avgPrice"), 96.0);
    assert_eq!(num(strategy, "openProfit"), 98.5 - 96.0);
}

#[test]
fn a_reversal_closes_one_trade_and_opens_the_next_on_one_fill() {
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Reversal\", initial_capital=10000, default_qty_type=strategy.fixed, ",
        "default_qty_value=2, commission_type=strategy.commission.percent, ",
        "commission_value=0.05, slippage=3)\n",
        "fast = ta.sma(close, 3)\n",
        "slow = ta.sma(close, 7)\n",
        "if ta.crossover(fast, slow)\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if ta.crossunder(fast, slow)\n",
        "    strategy.entry(\"S\", strategy.short)\n",
        "plot(fast)\n",
    );
    let bars = synthetic_bars();
    let document = assert_trade_results("reversal", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    assert!(rows.len() >= 10, "only {} closed trades", rows.len());
    for pair in rows.windows(2) {
        // One order: the fill that closes a trade opens the opposite one.
        assert_eq!(int(&pair[0], "exitBar"), int(&pair[1], "entryBar"));
        assert_eq!(num(&pair[0], "exitPrice"), num(&pair[1], "entryPrice"));
        assert_eq!(
            pair[0]["sign"].as_i64(),
            pair[1]["sign"].as_i64().map(|sign| -sign)
        );
        assert_eq!(pair[0]["exitId"], pair[1]["entryId"]);
        // That order's commission covers four contracts; the closing leg and
        // the lot it opens carry two each.
        let order = 4.0 * num(&pair[0], "exitPrice") * 0.0005;
        let (closing, opening) = (
            num(&pair[0], "exitCommission"),
            num(&pair[1], "entryCommission"),
        );
        assert!((closing / (order / 2.0) - 1.0).abs() < 1e-9);
        assert!((opening / (order / 2.0) - 1.0).abs() < 1e-9);
    }
    assert!(rows.iter().any(|row| row["sign"] == 1) && rows.iter().any(|row| row["sign"] == -1));
}

#[test]
fn pyramided_lots_closed_by_one_order_share_its_commission() {
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Pyramid\", initial_capital=100000, pyramiding=3, ",
        "default_qty_type=strategy.fixed, default_qty_value=1, ",
        "commission_type=strategy.commission.cash_per_order, commission_value=1.25)\n",
        "if close > close[1] and bar_index % 2 == 0\n",
        "    strategy.entry(\"P\", strategy.long)\n",
        "if ta.crossunder(ta.sma(close, 3), ta.sma(close, 8))\n",
        "    strategy.close_all()\n",
        "plot(close)\n",
    );
    let bars = synthetic_bars();
    let document = assert_trade_results("pyramid", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    let mut by_exit: std::collections::BTreeMap<u64, Vec<&serde_json::Value>> =
        std::collections::BTreeMap::new();
    for row in rows {
        by_exit.entry(int(row, "exitBar")).or_default().push(row);
    }
    let mut saw_three_lots = false;
    for lots in by_exit.values() {
        // Each entry was its own one-contract order; the closing order is
        // charged once and split across the lots it closes.
        let closing: f64 = lots.iter().map(|row| num(row, "exitCommission")).sum();
        assert!((closing - 1.25).abs() < 1e-12, "closing charge {closing}");
        for row in lots {
            assert_eq!(num(row, "entryCommission"), 1.25);
            assert_eq!(num(row, "qty"), 1.0);
        }
        let mut entries: Vec<u64> = lots.iter().map(|row| int(row, "entryBar")).collect();
        entries.dedup();
        assert_eq!(entries.len(), lots.len(), "one row per pyramided lot");
        saw_three_lots |= lots.len() == 3;
    }
    assert!(saw_three_lots, "the fixture must fill its pyramid");
}

#[test]
fn partial_closes_split_one_entry_across_rows() {
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Partial\", initial_capital=100000, default_qty_type=strategy.fixed, ",
        "default_qty_value=4, commission_type=strategy.commission.percent, ",
        "commission_value=0.1, process_orders_on_close=true)\n",
        "fast = ta.sma(close, 3)\n",
        "slow = ta.sma(close, 8)\n",
        "if ta.crossover(fast, slow)\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if strategy.position_size == 4 and close > strategy.position_avg_price + 1.0\n",
        "    strategy.close(\"L\", qty_percent = 50)\n",
        "if ta.crossunder(fast, slow)\n",
        "    strategy.close(\"L\")\n",
        "plot(fast)\n",
    );
    let bars = synthetic_bars();
    let document = assert_trade_results("partial", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    let mut split_entries = 0;
    for pair in rows.windows(2) {
        if int(&pair[0], "entryBar") != int(&pair[1], "entryBar") {
            continue;
        }
        split_entries += 1;
        // Half the lot, then the other half later, at the one entry fill.
        assert_eq!((num(&pair[0], "qty"), num(&pair[1], "qty")), (2.0, 2.0));
        assert!(int(&pair[0], "exitBar") < int(&pair[1], "exitBar"));
        assert_eq!(num(&pair[0], "entryPrice"), num(&pair[1], "entryPrice"));
        // Each row carries its own half of the entry order's commission.
        assert_eq!(
            num(&pair[0], "entryCommission"),
            num(&pair[1], "entryCommission")
        );
        let entry_order = 4.0 * num(&pair[0], "entryPrice") * 0.001;
        let carried = num(&pair[0], "entryCommission") + num(&pair[1], "entryCommission");
        assert!((carried / entry_order - 1.0).abs() < 1e-9);
    }
    assert!(split_entries >= 3, "only {split_entries} partial closes");
    assert!(rows.iter().any(|row| num(row, "qty") == 4.0));
}

#[test]
fn a_bracket_that_scales_out_reserves_each_exit_its_share_of_the_lot() {
    // Two `strategy.exit` orders on one entry: half the lot at a near
    // target, and the rest between a stop and a far target. Each reserves
    // its own contracts of the lot, so the entry closes in two rows.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Scale out\", initial_capital=100000, default_qty_type=strategy.fixed, ",
        "default_qty_value=4, commission_type=strategy.commission.cash_per_contract, ",
        "commission_value=0.05, slippage=1)\n",
        "fast = ta.sma(close, 3)\n",
        "slow = ta.sma(close, 8)\n",
        "if ta.crossover(fast, slow) and strategy.position_size == 0\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "strategy.exit(\"Half\", \"L\", qty_percent = 50, profit = 150)\n",
        "strategy.exit(\"Rest\", \"L\", loss = 250, profit = 400)\n",
        "plot(fast)\n",
    );
    let bars = synthetic_bars();
    let document = assert_trade_results("scale out", source, &bars, DEFAULT_ECONOMICS);
    let rows = rows(&document);
    let halves: Vec<_> = rows.iter().filter(|row| row["exitId"] == "Half").collect();
    let rests: Vec<_> = rows.iter().filter(|row| row["exitId"] == "Rest").collect();
    assert!(halves.len() >= 3, "only {} scale-outs", halves.len());
    assert_eq!(halves.len() + rests.len(), rows.len());
    let mut paired = 0;
    for half in &halves {
        // The near target is a limit: two contracts, 150 ticks up, no slippage.
        assert_eq!(num(half, "qty"), 2.0);
        assert!(num(half, "pnl") > 0.0);
        assert!(num(half, "exitPrice") - num(half, "entryPrice") >= 1.5 - 0.005);
        // Its other half is a later `Rest` row of the same entry, unless the
        // run ended first with those two contracts still open.
        let entry = int(half, "entryBar");
        let Some(rest) = rests.iter().find(|row| int(row, "entryBar") == entry) else {
            assert_eq!(int(half, "index") as usize, rows.len() - 1);
            assert_eq!(num(&document["strategy"]["position"], "size"), 2.0);
            continue;
        };
        assert_eq!(num(rest, "qty"), 2.0);
        assert!(int(rest, "index") > int(half, "index"));
        assert_eq!(num(half, "entryCommission"), num(rest, "entryCommission"));
        paired += 1;
    }
    assert!(paired >= 3, "only {paired} entries closed in two rows");
    // `Half` reserved its two contracts first, so `Rest` only ever holds the
    // other two: its stop takes two contracts, never the whole lot.
    assert!(rests.iter().all(|row| num(row, "qty") == 2.0));
    assert!(rests.iter().any(|row| num(row, "pnl") < 0.0));
    assert!(rests.iter().any(|row| num(row, "pnl") > 0.0));
}

#[test]
fn a_strategy_that_never_trades_returns_an_empty_ledger() {
    let source = "strategy(\"Idle\")\nplot(close)\n";
    let bars = exact_bars(5);
    // The whole document, in field order, at both runtimes' defaults.
    assert_eq!(
        String::from_utf8(trade_document(source, &bars, "")).unwrap(),
        concat!(
            r#"{"version":1,"projection":"broker-trades-v1","bars":5,"#,
            r#""strategy":{"initialCapital":1000000.0,"mintick":0.01,"pointValue":1.0,"#,
            r#""slippageTicks":0,"accountCurrency":"","symbolCurrency":"","#,
            r#""currencyConversionComplete":true,"firstRetainedTradeIndex":0,"#,
            r#""closedTrades":0,"netProfit":0.0,"grossProfit":0.0,"grossLoss":0.0,"#,
            r#""openProfit":0.0,"maxDrawdown":0.0,"maxDrawdownPercent":0.0,"openEntryCommission":0.0,"commissionPaid":0.0,"#,
            r#""commissionPaidLong":0.0,"commissionPaidShort":0.0,"#,
            r#""position":{"size":0.0,"avgPrice":0.0},"trades":[]}}"#
        )
    );
    let document = assert_trade_results("idle", source, &bars, DEFAULT_ECONOMICS);
    assert!(ledger_is_complete(&document));
    assert!(document["strategy"].get("firstEntryBar").is_none());
}

#[test]
fn a_position_that_never_closes_has_an_entry_context_and_no_rows() {
    // Bought once and held: no closed trade, so no row, but the first-entry
    // anchor, the open position and its unrealized profit are all there for
    // a consumer that must not mistake "nothing closed" for "nothing happened".
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Hold\", initial_capital=10000, default_qty_value=5, ",
        "commission_type=strategy.commission.cash_per_contract, commission_value=0.1)\n",
        "if bar_index == 2\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "plot(close)\n",
    );
    let bars = exact_bars(40);
    let document = assert_trade_results("hold", source, &bars, DEFAULT_ECONOMICS);
    let strategy = &document["strategy"];
    assert!(rows(&document).is_empty());
    assert_eq!(int(strategy, "closedTrades"), 0);
    assert_eq!(int(strategy, "firstEntryBar"), 3);
    assert_eq!(num(strategy, "firstEntryPrice"), bars[3].open);
    assert_eq!(
        num(strategy, "firstEntryAccountNotionalPerUnit"),
        bars[3].open
    );
    assert_eq!(num(&strategy["position"], "size"), 5.0);
    assert_eq!(num(&strategy["position"], "avgPrice"), bars[3].open);
    assert_eq!(
        num(strategy, "openProfit"),
        (bars[39].close - bars[3].open) * 5.0
    );
    assert_eq!(num(strategy, "netProfit"), -0.5);
}

#[test]
fn a_ledger_with_a_number_that_is_not_finite_is_refused() {
    // With margin checks off, a 1e308-contract position overflows its
    // notional, and the profit and commission computed from it are not
    // numbers. Full results spell those as `null`; the ledger a score would
    // be computed from is refused instead, as this evaluation's own failure
    // (the `Indicator failure: ` class) and not as a host fault.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Overflow\", margin_long=0, margin_short=0, process_orders_on_close=true)\n",
        "if bar_index == 0\n",
        "    strategy.entry(\"L\", strategy.long, qty = 1e308)\n",
        "if bar_index == 1\n",
        "    strategy.close(\"L\")\n",
        "plot(close)\n",
    );
    let bars = [
        bar(0, 100.0, 100.0, 100.0, 100.0),
        bar(1, 102.0, 102.0, 102.0, 102.0),
        bar(2, 102.0, 102.0, 102.0, 102.0),
    ];
    let reference = run(
        &compile_csil(source),
        &market_data(&bars),
        &RunOptions::default(),
    );
    let strategy = reference.strategy.as_ref().expect("strategy outcome");
    assert_eq!(strategy.trades.len(), 1);
    let trade = &strategy.trades[0];
    assert!(trade.entry_notional.is_some_and(f64::is_infinite));
    assert!(trade.pnl.is_nan() && strategy.net_profit.is_nan());

    let mut instance = StandaloneWasm::new(&compile_strategy_standalone_wasm(source));
    assert_eq!(
        instance
            .evaluate(&request(&bars, "\"trade_results\":true,"))
            .unwrap_err(),
        "Indicator failure: trade ledger contains a value that is not finite"
    );
    // Only this projection refuses: the same run still has its signals.
    assert!(instance
        .evaluate(&request(&bars, ""))
        .unwrap()
        .starts_with(b"{\"version\":1,\"signals\":[{"));
    // One bar earlier the position is only open, and its unrealized profit
    // is already not a number: the header is held to the same rule as rows.
    assert_eq!(
        instance
            .evaluate(&request(&bars[..1], "\"trade_results\":true,"))
            .unwrap_err(),
        "Indicator failure: trade ledger contains a value that is not finite"
    );
}

#[test]
fn a_short_closed_in_two_parts_serializes_exactly() {
    // Short two contracts on bar 0's close at 10 for one 0.5 order charge.
    // Cover one on bar 2's close at 9 and the other on bar 3's close at 9.5,
    // each closing order charged 0.5 as well. Every number is exact, so the
    // whole document is pinned: field order, number forms, and which charge
    // is which. Each row carries half the entry order's charge (0.25) and
    // all of its own closing order's (0.5):
    //   row 0: (9 - 10) * 1 * -1 - 0.5 - 0.25 = 0.25
    //   row 1: (9.5 - 10) * 1 * -1 - 0.5 - 0.25 = -0.25
    // Bar 1 trades up to 11.5 and bar 2 down to 8.5 while both contracts are
    // open: three points against and three in favor, halved onto each row,
    // and a trade-anchored account drawdown of 3. Three orders were charged
    // 0.5 each, all of it on short trades, and the run ends flat, so the 1.5
    // paid is the two rows' 0.75 each.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Two parts\", initial_capital=1000, process_orders_on_close=true, ",
        "commission_type=strategy.commission.cash_per_order, commission_value=0.5)\n",
        "if bar_index == 0\n",
        "    strategy.entry(\"Short\", strategy.short, qty = 2)\n",
        "if bar_index == 2\n",
        "    strategy.close(\"Short\", qty = 1)\n",
        "if bar_index == 3\n",
        "    strategy.close(\"Short\")\n",
        "plot(close)\n",
    );
    let bars = [
        bar(0, 10.0, 10.5, 9.5, 10.0),
        bar(1, 10.0, 11.5, 10.0, 11.0),
        bar(2, 11.0, 11.0, 8.5, 9.0),
        bar(3, 9.0, 10.0, 9.0, 9.5),
    ];
    assert_trade_results("two parts", source, &bars, DEFAULT_ECONOMICS);
    assert_eq!(
        String::from_utf8(trade_document(source, &bars, "")).unwrap(),
        concat!(
            r#"{"version":1,"projection":"broker-trades-v1","bars":4,"#,
            r#""strategy":{"initialCapital":1000.0,"mintick":0.01,"pointValue":1.0,"#,
            r#""slippageTicks":0,"accountCurrency":"","symbolCurrency":"","#,
            r#""currencyConversionComplete":true,"firstRetainedTradeIndex":0,"#,
            r#""closedTrades":2,"firstEntryBar":0,"firstEntryPrice":10.0,"#,
            r#""firstEntryAccountNotionalPerUnit":10.0,"netProfit":0.0,"grossProfit":0.25,"#,
            r#""grossLoss":0.25,"openProfit":0.0,"maxDrawdown":3.5,"maxDrawdownPercent":0.35000000000000003,"openEntryCommission":0.0,"commissionPaid":1.5,"#,
            r#""commissionPaidLong":0.0,"commissionPaidShort":1.5,"#,
            r#""position":{"size":0.0,"avgPrice":0.0},"trades":["#,
            r#"{"index":0,"entryId":"Short","exitId":"Short","exitOrderName":"Close entry(s) order Short","sign":-1,"qty":1.0,"#,
            r#""entryBar":0,"exitBar":2,"entryPrice":10.0,"exitPrice":9.0,"pnl":0.25,"#,
            r#""entryNotional":10.0,"entryCommission":0.25,"exitCommission":0.5,"#,
            r#""commission":0.75,"cumulativePnl":0.25,"maxRunup":1.25,"maxDrawdown":1.75},"#,
            r#"{"index":1,"entryId":"Short","exitId":"Short","exitOrderName":"Close entry(s) order Short","sign":-1,"qty":1.0,"#,
            r#""entryBar":0,"exitBar":3,"entryPrice":10.0,"exitPrice":9.5,"pnl":-0.25,"#,
            r#""entryNotional":10.0,"entryCommission":0.25,"exitCommission":0.5,"#,
            r#""commission":0.75,"cumulativePnl":0.0,"maxRunup":1.25,"maxDrawdown":1.75}]}}"#
        )
    );
}

#[test]
fn an_open_lot_carries_commission_that_no_row_holds() {
    // A quarter a contract, on every fill, at each bar's close:
    //   bar 0  buy 4 at 10                              charged 1.00
    //   bar 1  sell 1 of them at 11                     charged 0.25
    //   bar 2  sell 5 at 12: the other 3, and 2 short   charged 1.25
    //   bar 3  buy 1 of the short back at 11            charged 0.25
    // Three rows close 1 long, 3 long and 1 short, each carrying a quarter a
    // contract for its entry and a quarter for its exit: 0.5, 1.5 and 0.5.
    // One short contract is still open at the end, and the quarter charged
    // to open it is in no row. Commission paid is the 2.75 the four orders
    // were charged: 2.0 on the long trades, and on the short side the closed
    // trade's 0.5 and the open contract's 0.25. Open profit already has that
    // quarter in it: (12 - 11.25) * 1 - 0.25 = 0.5.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Carried\", initial_capital=1000, process_orders_on_close=true, ",
        "commission_type=strategy.commission.cash_per_contract, commission_value=0.25)\n",
        "if bar_index == 0\n",
        "    strategy.entry(\"L\", strategy.long, qty = 4)\n",
        "if bar_index == 1\n",
        "    strategy.close(\"L\", qty = 1)\n",
        "if bar_index == 2\n",
        "    strategy.entry(\"S\", strategy.short, qty = 2)\n",
        "if bar_index == 3\n",
        "    strategy.close(\"S\", qty = 1)\n",
        "plot(close)\n",
    );
    let bars = [
        bar(0, 10.0, 10.5, 9.5, 10.0),
        bar(1, 10.0, 11.5, 10.0, 11.0),
        bar(2, 11.0, 12.5, 11.0, 12.0),
        bar(3, 12.0, 12.5, 10.5, 11.0),
        bar(4, 11.0, 11.5, 10.5, 11.25),
    ];
    let document = assert_trade_results("carried", source, &bars, DEFAULT_ECONOMICS);
    let strategy = &document["strategy"];
    let rows = rows(&document);
    let commissions: Vec<(i64, f64, f64)> = rows
        .iter()
        .map(|row| {
            (
                row["sign"].as_i64().unwrap(),
                num(row, "qty"),
                num(row, "commission"),
            )
        })
        .collect();
    assert_eq!(commissions, [(1, 1.0, 0.5), (1, 3.0, 1.5), (-1, 1.0, 0.5)]);
    assert_eq!(num(&strategy["position"], "size"), -1.0);
    assert_eq!(num(strategy, "openProfit"), 0.75);
    assert_eq!(num(strategy, "commissionPaid"), 0.5 + 1.5 + 0.5 + 0.25);
    assert_eq!(num(strategy, "commissionPaidLong"), 0.5 + 1.5);
    assert_eq!(num(strategy, "commissionPaidShort"), 0.5 + 0.25);
    // Stopped after bar 2, the run has not bought the contract back: two
    // short contracts are open, carrying the half charged to open them.
    let short = assert_trade_results("carried, to bar 2", source, &bars[..3], DEFAULT_ECONOMICS);
    let strategy = &short["strategy"];
    assert_eq!(num(&strategy["position"], "size"), -2.0);
    assert_eq!(num(strategy, "commissionPaid"), 0.5 + 1.5 + 0.5);
    assert_eq!(num(strategy, "commissionPaidLong"), 0.5 + 1.5);
    assert_eq!(num(strategy, "commissionPaidShort"), 0.5);
    // Stopped after bar 1, three long contracts are open instead, carrying
    // three quarters, beside the one row's 0.5.
    let long = assert_trade_results("carried, to bar 1", source, &bars[..2], DEFAULT_ECONOMICS);
    let strategy = &long["strategy"];
    assert_eq!(num(&strategy["position"], "size"), 3.0);
    assert_eq!(num(strategy, "commissionPaid"), 0.5 + 0.75);
    assert_eq!(num(strategy, "commissionPaidLong"), 0.5 + 0.75);
    assert_eq!(num(strategy, "commissionPaidShort"), 0.0);
}

/// A reversing strategy that adds a second lot to each position and later
/// closes one contract of the first, under percent commission.
const OPEN_LOTS: &str = concat!(
    "//@version=6\n",
    "strategy(\"Open lots\", initial_capital=100000, pyramiding=2, ",
    "default_qty_type=strategy.fixed, default_qty_value=4, ",
    "commission_type=strategy.commission.percent, commission_value=0.1, slippage=1)\n",
    "fast = ta.sma(close, 3)\n",
    "slow = ta.sma(close, 8)\n",
    "if ta.crossover(fast, slow)\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "if ta.crossunder(fast, slow)\n",
    "    strategy.entry(\"S\", strategy.short)\n",
    "if strategy.position_size == 4 and bar_index % 3 == 0\n",
    "    strategy.entry(\"L2\", strategy.long, qty = 2)\n",
    "if strategy.position_size == -4 and bar_index % 3 == 0\n",
    "    strategy.entry(\"S2\", strategy.short, qty = 2)\n",
    "if strategy.position_size == 6 and bar_index % 3 == 1\n",
    "    strategy.close(\"L\", qty = 1)\n",
    "if strategy.position_size == -6 and bar_index % 3 == 1\n",
    "    strategy.close(\"S\", qty = 1)\n",
    "plot(fast)\n",
);

#[test]
fn commission_paid_matches_the_interpreter_with_lots_open_and_partly_closed() {
    // Stopped on any bar after its first fill, this run has a position open:
    // one lot, two lots, or two lots with a contract of the earlier already
    // closed, long or short. Percent commission on slipped fills makes every
    // charge an inexact product, so the totals agree with the interpreter's
    // report to the bit only if each scope adds the same charges in the same
    // order. The run is stopped in each of those states: early, when no
    // trade or one has closed, and late, after as many as thirty.
    let bars = exact_bars(400);
    let economics = Economics {
        mintick: 0.05,
        mincontract: 0.001,
        pointvalue: 2.5,
        currency: "",
    };
    let mut ends = std::collections::BTreeSet::new();
    for end in [
        43, 44, 60, 79, 80, 100, 140, 190, 191, 220, 229, 302, 338, 360, 376, 377, 400,
    ] {
        let name = format!("open lots, {end} bars");
        let document = assert_trade_results(&name, OPEN_LOTS, &bars[..end], economics);
        assert!(ledger_is_complete(&document), "{name}");
        let strategy = &document["strategy"];
        let rows = rows(&document);
        let closed = |keep: fn(i64) -> bool| {
            rows.iter()
                .filter(|row| keep(row["sign"].as_i64().expect("integer sign")))
                .fold(0.0, |sum, row| sum + num(row, "commission"))
        };
        // What the open lots carry, by scope: all, long, short.
        let carried = [
            num(strategy, "commissionPaid") - closed(|_| true),
            num(strategy, "commissionPaidLong") - closed(|sign| sign > 0),
            num(strategy, "commissionPaidShort") - closed(|sign| sign < 0),
        ];
        let size = num(&strategy["position"], "size");
        let (own, other) = if size > 0.0 { (1, 2) } else { (2, 1) };
        assert!(size != 0.0, "{name}: the run must end with a lot open");
        assert!(carried[own] > 0.0, "{name}: {carried:?}");
        assert_eq!(carried[other], 0.0, "{name}: {carried:?}");
        assert!(
            (carried[0] - carried[own]).abs() < 1e-9,
            "{name}: {carried:?}"
        );
        if size.abs() == 5.0 {
            // Five contracts are six less the one the latest row closed, out
            // of the earlier lot. That row holds one contract's share of the
            // lot's opening commission; the contracts the lot still has open
            // carry at least as much again, and the other lot its own.
            let partial = rows.last().expect("the partial close");
            assert_eq!(num(partial, "qty"), 1.0, "{name}");
            assert_eq!(
                partial["sign"].as_i64(),
                Some(if size > 0.0 { 1 } else { -1 }),
                "{name}"
            );
            assert!(
                carried[own] > num(partial, "entryCommission"),
                "{name}: {carried:?}"
            );
        }
        ends.insert(size as i64);
    }
    // Two contracts are what one bar's pair of orders leaves: a reversal
    // into four, and then a two-contract entry the other way.
    assert_eq!(
        ends.into_iter().collect::<Vec<_>>(),
        [-6, -5, -4, 2, 4, 5, 6],
        "the prefixes must end in every open state, long and short"
    );
}

#[test]
fn a_break_even_short_reports_zero_without_a_sign() {
    // `(10 - 10) * 1 * -1` is negative zero, and with no commission so is the
    // P&L the broker stores. The document writes `0.0`: a JSON reader need
    // not keep a zero's sign, so none is put on the wire.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Break even\", initial_capital=1000, process_orders_on_close=true)\n",
        "if bar_index == 0\n",
        "    strategy.entry(\"S\", strategy.short)\n",
        "if bar_index == 1\n",
        "    strategy.close(\"S\")\n",
        "plot(close)\n",
    );
    let bars = [
        bar(0, 10.0, 10.5, 9.5, 10.0),
        bar(1, 10.0, 10.5, 9.5, 10.0),
        bar(2, 10.0, 10.5, 9.5, 10.0),
    ];
    let program = compile_csil(source);
    let reference = run(&program, &market_data(&bars), &RunOptions::default());
    let trade = &reference.strategy.as_ref().unwrap().trades[0];
    assert!(trade.pnl == 0.0 && trade.pnl.is_sign_negative());

    let text = String::from_utf8(trade_document(source, &bars, "")).unwrap();
    assert!(!text.contains("-0"), "{text}");
    let document = assert_trade_results("break even", source, &bars, DEFAULT_ECONOMICS);
    let row = &rows(&document)[0];
    assert_eq!(num(row, "pnl").to_bits(), 0.0f64.to_bits());
    assert_eq!(int(&document["strategy"], "closedTrades"), 1);
}

/// `A` opens with the run's first fill and stays open while `B` round-trips
/// beside it under `close_entries_rule = "ANY"`, so the first closed trade
/// belongs to `B`. After bar 10 every bar fills one order.
const RETENTION_GAP: &str = concat!(
    "//@version=6\n",
    "strategy(\"Gap\", pyramiding=2, close_entries_rule=\"ANY\", ",
    "process_orders_on_close=true, commission_type=strategy.commission.cash_per_order, ",
    "commission_value=0.125)\n",
    "if bar_index == 0\n",
    "    strategy.entry(\"A\", strategy.long)\n",
    "if bar_index > 0 and bar_index % 2 == 1\n",
    "    strategy.entry(\"B\", strategy.long)\n",
    "if bar_index > 0 and bar_index % 2 == 0\n",
    "    strategy.close(\"B\")\n",
    "if bar_index == 10\n",
    "    strategy.close(\"A\")\n",
    "plot(close)\n",
);

fn alternating_bars(count: usize) -> Vec<Bar> {
    (0..count)
        .map(|index| {
            let price = if index % 2 == 0 { 100.0 } else { 101.0 };
            bar(index, price, price, price, price)
        })
        .collect()
}

#[test]
fn a_retention_gap_in_the_middle_of_the_ledger_is_visible() {
    // 9,000 bars fill exactly 9,001 orders. `A`'s opening fill is then the
    // only one outside the 9,000-order window, so the one row it belongs to,
    // closed trade 5, is evicted while trade 0 is still retained.
    let bars = alternating_bars(9_000);
    let document = assert_trade_results("retention gap", RETENTION_GAP, &bars, DEFAULT_ECONOMICS);
    let strategy = &document["strategy"];
    let rows = rows(&document);
    assert_eq!(int(strategy, "firstRetainedTradeIndex"), 0);
    assert_eq!(int(strategy, "closedTrades"), 4_500);
    assert_eq!(rows.len(), 4_499);
    let indices: Vec<u64> = rows.iter().map(|row| int(row, "index")).collect();
    assert_eq!(indices[..6], [0, 1, 2, 3, 4, 6]);
    assert!(indices[5..].windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert!(rows.iter().all(|row| row["entryId"] == "B"));
    // A zero first index alone would have passed this ledger as whole.
    assert!(!ledger_is_complete(&document));

    // The totals are the full run's: they include the evicted trade's loss,
    // which the retained rows alone cannot reproduce.
    let last = rows.last().expect("retained rows");
    assert_eq!(
        num(strategy, "netProfit"),
        num(last, "cumulativePnl") - num(strategy, "openEntryCommission")
    );
    let retained: f64 = rows.iter().map(|row| num(row, "pnl")).sum();
    assert_eq!(num(strategy, "netProfit") - retained, -0.375);
    assert_eq!(num(strategy, "grossProfit"), 0.0);
    assert_eq!(num(strategy, "grossLoss"), -num(strategy, "netProfit"));
    // The ninth-thousand bar's entry is still open.
    assert_eq!(num(&strategy["position"], "size"), 1.0);
    assert_eq!(num(strategy, "openProfit"), 0.0);
}

#[test]
fn a_ledger_truncated_from_the_front_reports_its_first_retained_index() {
    // 9,400 bars fill 9,401 orders, so the window starts at order 401. `B`'s
    // entry on bar 401 is order 402, the first inside it, and its close on
    // bar 402 is closed trade 201: bars 2 to 10 close trades 0 to 4, `A` is
    // trade 5, and every later even bar 2j closes trade j.
    let bars = alternating_bars(9_400);
    let document = assert_trade_results("truncated", RETENTION_GAP, &bars, DEFAULT_ECONOMICS);
    let strategy = &document["strategy"];
    let rows = rows(&document);
    assert_eq!(int(strategy, "firstRetainedTradeIndex"), 201);
    assert_eq!(int(&rows[0], "index"), 201);
    assert_eq!(
        (int(&rows[0], "entryBar"), int(&rows[0], "exitBar")),
        (401, 402)
    );
    // 4,699 closes of `B` and one of `A`; the last 4,499 are retained, in
    // one contiguous run that does not start at zero.
    assert_eq!(int(strategy, "closedTrades"), 4_700);
    assert_eq!(rows.len(), 4_499);
    assert_eq!(int(rows.last().unwrap(), "index"), 4_699);
    assert!(!ledger_is_complete(&document));
    assert_eq!(
        num(strategy, "netProfit"),
        num(rows.last().unwrap(), "cumulativePnl") - num(strategy, "openEntryCommission")
    );
}

/// One order fills on every bar's close and reverses the position, so every
/// bar changes the posture and, from the second, closes a trade.
const CHURN: &str = concat!(
    "//@version=6\n",
    "strategy(\"Churn\", process_orders_on_close=true, ",
    "commission_type=strategy.commission.cash_per_order, commission_value=0.75)\n",
    "if bar_index % 2 == 0\n",
    "    strategy.entry(\"L\", strategy.long)\n",
    "else\n",
    "    strategy.entry(\"S\", strategy.short)\n",
    "plot(close)\n",
);

/// The ledger of [`CHURN`] over an even number of `alternating_bars` past
/// the 9,000-order window, written out from the run length alone. `bars`
/// bars fill `bars` orders and close `bars - 1` trades. Order `k` opens trade
/// `k` and order `k + 1` closes it, so the window keeps trades `bars - 9,000`
/// to `bars - 2`: 8,999 rows however long the run is. Every trade gains one
/// point. A reversal's 0.75 charge is split between the trade it closes and
/// the lot it opens, so each trade nets 0.25, except trade 0, whose entry
/// order carried a whole charge: -0.125, the run's only loss. The orders
/// paid `0.75 * bars` between them. The long trades hold half of that and
/// the half charge trade 0's entry did not share. The short side holds the
/// rest: its trades, and the 0.375 the last bar's short lot still carries.
/// The open entry fee is already realized as a loss; each retained row's excursion also includes its 0.375 entry fee.
fn churn_ledger(bars: u64) -> String {
    use std::fmt::Write as _;
    assert!(bars > 9_000 && bars % 2 == 0);
    let first = bars - 9_000;
    let mut out = format!(
        concat!(
            r#"{{"version":1,"projection":"broker-trades-v1","bars":{},"#,
            r#""strategy":{{"initialCapital":1000000.0,"mintick":0.01,"pointValue":1.0,"#,
            r#""slippageTicks":0,"accountCurrency":"","symbolCurrency":"","#,
            r#""currencyConversionComplete":true,"firstRetainedTradeIndex":{},"#,
            r#""closedTrades":{},"firstEntryBar":0,"firstEntryPrice":100.0,"#,
            r#""firstEntryAccountNotionalPerUnit":100.0,"netProfit":{:?},"grossProfit":{:?},"#,
            r#""grossLoss":0.5,"openProfit":0.0,"maxDrawdown":0.75,"maxDrawdownPercent":0.00007500000000000001,"openEntryCommission":0.375,"#,
            r#""commissionPaid":{:?},"commissionPaidLong":{:?},"commissionPaidShort":{:?},"#,
            r#""position":{{"size":-1.0,"avgPrice":101.0}},"trades":["#,
        ),
        bars,
        first,
        bars - 1,
        -0.5 + 0.25 * (bars - 2) as f64,
        0.25 * (bars - 2) as f64,
        0.75 * bars as f64,
        0.375 * (bars + 1) as f64,
        0.375 * (bars - 1) as f64,
    );
    for trade in first..bars - 1 {
        let (entry_id, exit_id, sign, entry, exit) = if trade % 2 == 0 {
            ("L", "S", 1, "100.0", "101.0")
        } else {
            ("S", "L", -1, "101.0", "100.0")
        };
        if trade > first {
            out.push(',');
        }
        write!(
            out,
            concat!(
                r#"{{"index":{},"entryId":"{}","exitId":"{}","sign":{},"qty":1.0,"#,
                r#""entryBar":{},"exitBar":{},"entryPrice":{},"exitPrice":{},"pnl":0.25,"#,
                r#""entryNotional":{},"entryCommission":0.375,"exitCommission":0.375,"#,
                r#""commission":0.75,"cumulativePnl":{:?},"maxRunup":0.625,"maxDrawdown":0.375}}"#,
            ),
            trade,
            entry_id,
            exit_id,
            sign,
            trade,
            trade + 1,
            entry,
            exit,
            entry,
            -0.125 + 0.25 * trade as f64,
        )
        .unwrap();
    }
    out.push_str("]}}");
    out
}

/// Hold the ledger of [`CHURN`] over `bars` bars to [`churn_ledger`] byte for
/// byte and to the interpreter field for field. Returns the byte lengths of
/// the default document and of the ledger.
fn assert_churn_ledger(bars: u64) -> (usize, usize) {
    let name = format!("{bars} bars");
    let data = alternating_bars(bars as usize);
    let ledger = String::from_utf8(trade_document(CHURN, &data, "")).unwrap();
    let expected = churn_ledger(bars);
    if ledger != expected {
        // Megabytes of JSON: report where the two part, not all of both.
        let at = ledger
            .bytes()
            .zip(expected.bytes())
            .take_while(|(module, closed_form)| module == closed_form)
            .count();
        let near = |text: &str| text[at.saturating_sub(80)..(at + 80).min(text.len())].to_owned();
        panic!(
            "{name}: the ledger leaves its closed form at byte {at}:\n{}\n{}",
            near(&ledger),
            near(&expected)
        );
    }
    // No posture event is in it: the bytes above are the header and the rows.
    assert!(!ledger.contains("signals") && !ledger.contains("direction"));

    let document = assert_trade_results(&name, CHURN, &data, DEFAULT_ECONOMICS);
    let strategy = &document["strategy"];
    let rows = rows(&document);
    // The counters say exactly what the list no longer holds.
    let evicted = bars - 9_000;
    assert_eq!(int(strategy, "closedTrades"), bars - 1, "{name}");
    assert_eq!(int(strategy, "firstRetainedTradeIndex"), evicted, "{name}");
    assert_eq!(rows.len(), 8_999, "{name}");
    assert_eq!(int(&rows[0], "index"), evicted, "{name}");
    assert_eq!(int(rows.last().unwrap(), "index"), bars - 2, "{name}");
    assert!(!ledger_is_complete(&document), "{name}");
    // The totals are the whole run's. The retained rows fall short of the net
    // profit by the evicted trades' profit and the paid opening fee on the
    // still-open lot. Trade 0 is the only closed loss, and was evicted.
    let retained: f64 = rows.iter().map(|row| num(row, "pnl")).sum();
    assert_eq!(
        num(strategy, "netProfit") - retained,
        -0.5 + 0.25 * (evicted - 1) as f64,
        "{name}"
    );
    assert_eq!(num(strategy, "grossLoss"), 0.5, "{name}");
    assert!(rows.iter().all(|row| num(row, "pnl") > 0.0), "{name}");
    // So is the commission paid: every order's charge, of which the 8,999
    // retained rows hold 0.75 each.
    let retained: f64 = rows.iter().map(|row| num(row, "commission")).sum();
    assert_eq!(retained, 0.75 * 8_999.0, "{name}");
    assert_eq!(
        num(strategy, "commissionPaid") - retained,
        0.75 * (bars - 8_999) as f64,
        "{name}"
    );

    // The default document has a posture event for every bar.
    let legacy = StandaloneWasm::new(&compile_strategy_standalone_wasm(CHURN))
        .evaluate(&wire(&data))
        .unwrap();
    let events: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
    assert_eq!(
        events["signals"].as_array().unwrap().len() as u64,
        bars,
        "{name}"
    );
    (legacy.len(), ledger.len())
}

#[test]
fn a_high_turnover_ledger_holds_its_window_while_the_signal_stream_grows() {
    // Both runs are past the window. 600 more bars put 600 more events, of
    // fifty bytes or more each, in the default document. The ledger still
    // has its 8,999 rows, and its bytes are the closed form's, which takes
    // nothing from the signal stream. All it gained is digits, under five
    // bytes a bar: the later window's indices, bar numbers and cumulative
    // profits are longer numbers.
    let (short_signals, short_ledger) = assert_churn_ledger(9_200);
    let (long_signals, long_ledger) = assert_churn_ledger(9_800);
    assert!(long_signals - short_signals >= 600 * 50);
    assert!(long_ledger - short_ledger < 600 * 5);
}

#[test]
#[ignore = "minutes under wasmi: every fill past the order window shifts the retained rows"]
fn a_high_turnover_ledger_gains_two_digits_when_the_run_doubles() {
    // Twice the bars put 19,000 more events in the default document and two
    // bytes in the ledger: the long and short commission totals, 7,125.375
    // and 7,124.625, double into five figures. The two lengths can be
    // compared exactly because both windows sit in the same decade of every
    // other number that moves with the run: five-digit indices, bar numbers
    // and commission paid, and four-digit cumulative profit.
    let (short_signals, short_ledger) = assert_churn_ledger(19_000);
    let (long_signals, long_ledger) = assert_churn_ledger(38_000);
    assert!(long_signals - short_signals >= 19_000 * 50);
    assert_eq!(long_ledger, short_ledger + 2);
}

#[test]
fn trade_results_refuse_every_other_result_format() {
    let bars = exact_bars(40);
    let mut instance = StandaloneWasm::new(&compile_strategy_standalone_wasm(PINNED_CLOSE_PASS));
    let ledger = instance
        .evaluate(&request(&bars, "\"trade_results\":true,"))
        .unwrap();
    let legacy = instance.evaluate(&request(&bars, "")).unwrap();
    let full = instance
        .evaluate(&request(&bars, "\"include_results\":true,"))
        .unwrap();
    let objectives = instance
        .evaluate(&request(&bars, "\"objective_results\":true,"))
        .unwrap();
    assert!(ledger.starts_with(b"{\"version\":1,\"projection\":\"broker-trades-v1\","));

    for flags in [
        "\"trade_results\":true,\"include_results\":true,",
        "\"include_results\":true,\"trade_results\":true,",
        "\"trade_results\":true,\"objective_results\":true,",
        "\"objective_results\":true,\"trade_results\":true,",
        "\"trade_results\":true,\"binary_results\":true,",
        "\"trade_results\":true,\"include_results\":true,\"binary_results\":true,",
        "\"trade_results\":true,\"include_results\":true,\"objective_results\":true,",
    ] {
        let error = instance.evaluate(&request(&bars, flags)).unwrap_err();
        assert_eq!(
            error, "Invalid JSON: trade_results cannot be combined with another result format",
            "{flags}"
        );
        // The refusal leaves nothing behind: the next request is served.
        assert_eq!(
            instance
                .evaluate(&request(&bars, "\"trade_results\":true,"))
                .unwrap(),
            ledger,
            "{flags}"
        );
    }
    assert!(instance
        .evaluate(&request(
            &bars,
            "\"trade_results\":true,\"trade_results\":false,"
        ))
        .unwrap_err()
        .contains("duplicate result format field"));
    for value in ["1", "null", "\"true\"", "[]"] {
        assert!(
            instance
                .evaluate(&request(&bars, &format!("\"trade_results\":{value},")))
                .is_err(),
            "trade_results must be a boolean, not {value}"
        );
    }

    // A disabled flag leaves the other formats as they are, and the flag
    // never outlives its own request.
    for (flags, expected) in [
        ("", &legacy),
        ("\"trade_results\":false,", &legacy),
        ("\"trade_results\":false,\"include_results\":true,", &full),
        (
            "\"objective_results\":true,\"trade_results\":false,",
            &objectives,
        ),
    ] {
        assert_eq!(
            instance
                .evaluate(&request(&bars, "\"trade_results\":true,"))
                .unwrap(),
            ledger
        );
        assert_eq!(
            &instance.evaluate(&request(&bars, flags)).unwrap(),
            expected,
            "{flags}"
        );
    }
}

fn binary_packet(bars: &[Bar], metadata: &str) -> Vec<u8> {
    let meta = metadata.as_bytes();
    let start = (16 + meta.len() + 7) & !7;
    let mut packet = vec![0u8; start];
    packet[..8].copy_from_slice(b"PWSWBIN1");
    packet[8..12].copy_from_slice(&(bars.len() as u32).to_le_bytes());
    packet[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
    packet[16..16 + meta.len()].copy_from_slice(meta);
    for lane in 0..6 {
        for bar in bars {
            let cell = [
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume,
                bar.time as f64,
            ][lane];
            packet.extend_from_slice(&cell.to_le_bytes());
        }
    }
    packet
}

#[test]
fn trade_results_are_the_same_document_over_every_input_transport() {
    let bars = exact_bars(240);
    let economics = "\"mintick\":0.25,\"mincontract\":0.001,\"pointvalue\":2.0,";
    let wasm = compile_strategy_standalone_wasm(PINNED_BRACKETS);
    let json = StandaloneWasm::new(&wasm)
        .evaluate(&request(
            &bars,
            &format!("{economics}\"trade_results\":true,"),
        ))
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert!(rows(&document).len() >= 10);

    let metadata = format!("{{\"version\":1,\"params\":{{}},{economics}\"trade_results\":true}}");
    assert_eq!(
        StandaloneWasm::new(&wasm)
            .evaluate_bytes(&binary_packet(&bars, &metadata))
            .unwrap(),
        json,
        "binary input"
    );
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    let mut resident = StandaloneWasm::new(&wasm);
    assert_eq!(
        resident.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 9, false)),
        json,
        "resident upload"
    );
    assert_eq!(
        resident.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata, 9, true)),
        json,
        "resident repeat"
    );

    // `fraction` evaluates a prefix, and the ledger is that prefix's own:
    // the document for half the bars is the document of the first half.
    let half = StandaloneWasm::new(&wasm)
        .evaluate(&request(
            &bars,
            &format!("{economics}\"fraction\":0.5,\"trade_results\":true,"),
        ))
        .unwrap();
    assert_eq!(
        half,
        StandaloneWasm::new(&wasm)
            .evaluate(&request(
                &bars[..120],
                &format!("{economics}\"trade_results\":true,")
            ))
            .unwrap()
    );
    assert_eq!(int(&serde_json::from_slice(&half).unwrap(), "bars"), 120);
}

#[test]
fn trade_results_name_the_request_and_account_currencies() {
    let bars = synthetic_bars();
    // An explicit account currency is admitted against the same quote
    // currency only, so both names are that currency.
    let explicit = concat!(
        "//@version=6\n",
        "strategy(\"Dollars\", currency=currency.USD, process_orders_on_close=true)\n",
        "if close > open\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if close < open\n",
        "    strategy.close(\"L\")\n",
        "plot(close)\n",
    );
    let dollars = Economics {
        currency: "USD",
        ..DEFAULT_ECONOMICS
    };
    let document = assert_trade_results("explicit account currency", explicit, &bars, dollars);
    assert_eq!(document["strategy"]["accountCurrency"], "USD");
    assert_eq!(document["strategy"]["symbolCurrency"], "USD");
    assert!(rows(&document).len() >= 10);
    let mut instance = StandaloneWasm::new(&compile_strategy_standalone_wasm(explicit));
    for fields in ["", "\"currency\":\"EUR\","] {
        let error = instance
            .evaluate(&request(&bars, &format!("{fields}\"trade_results\":true,")))
            .unwrap_err();
        assert!(error.contains("matching dataset quote currency"), "{error}");
    }

    // An unset account currency follows the symbol's, whatever its length;
    // a later request without one reports none.
    let unset = explicit.replace("currency=currency.USD, ", "");
    let tether = Economics {
        currency: "USDT",
        ..DEFAULT_ECONOMICS
    };
    let document = assert_trade_results("unset account currency", &unset, &bars, tether);
    assert_eq!(document["strategy"]["accountCurrency"], "USDT");
    assert_eq!(document["strategy"]["symbolCurrency"], "USDT");
    let mut instance = StandaloneWasm::new(&compile_strategy_standalone_wasm(&unset));
    for (fields, currency) in [
        ("\"currency\":\"USDT\",", "USDT"),
        ("", ""),
        ("\"currency\":\"EUR\",", "EUR"),
        ("", ""),
    ] {
        let output = instance
            .evaluate(&request(&bars, &format!("{fields}\"trade_results\":true,")))
            .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(document["strategy"]["symbolCurrency"], currency);
        assert_eq!(document["strategy"]["accountCurrency"], currency);
    }
}

#[test]
fn strategy_capabilities_declare_the_trade_ledger_and_the_broker_settings() {
    let section = |wasm: &[u8]| {
        String::from_utf8(custom_section(wasm, "pinewasm_capabilities").expect("capabilities"))
            .expect("UTF-8 capabilities")
    };
    let wasm = compile_strategy_standalone_wasm(PINNED_BRACKETS);
    assert_eq!(
        section(&wasm),
        concat!(
            r#"{"schema_version":1,"input_protocols":["json-v1","PWSWBIN1","PWSWBIN2","PWSWREF1"],"#,
            r#""output_projections":["retained-objectives-v1","broker-trades-v1"],"#,
            r#""strategy":{"initial_capital":25000.0,"slippage_ticks":2,"#,
            r#""commission_type":"percent","commission_value":0.075}}"#
        )
    );
    // What the module declares is what its ledger reports.
    let document: serde_json::Value =
        serde_json::from_slice(&trade_document(PINNED_BRACKETS, &exact_bars(60), "")).unwrap();
    let declared = &capabilities(&wasm)["strategy"];
    assert_eq!(
        declared["initial_capital"].as_f64(),
        Some(num(&document["strategy"], "initialCapital"))
    );
    assert_eq!(
        declared["slippage_ticks"].as_u64(),
        Some(int(&document["strategy"], "slippageTicks"))
    );

    // The declaration's defaults, an explicit account currency, and the
    // drawing bound all keep their places.
    assert_eq!(
        section(&compile_strategy_standalone_wasm(
            "strategy(\"Defaults\", currency=currency.EUR)\nplot(close)\n"
        )),
        concat!(
            r#"{"schema_version":1,"input_protocols":["json-v1","PWSWBIN1","PWSWBIN2","PWSWREF1"],"#,
            r#""output_projections":["retained-objectives-v1","broker-trades-v1"],"#,
            r#""strategy":{"initial_capital":1000000.0,"slippage_ticks":0,"#,
            r#""commission_type":"percent","commission_value":0.0,"account_currency":"EUR"}}"#
        )
    );
    let drawing = capabilities(&compile_strategy_standalone_wasm(DRAWING_STRATEGY));
    assert!(drawing["max_bars"].as_u64().is_some());
    assert_eq!(drawing["strategy"]["initial_capital"], 10000.0);
    for commission in ["cash_per_contract", "cash_per_order"] {
        let source = format!(
            "strategy(\"Fees\", commission_type=strategy.commission.{commission}, commission_value=1.5)\nplot(close)\n"
        );
        let declared = capabilities(&compile_strategy_standalone_wasm(&source));
        assert_eq!(declared["strategy"]["commission_type"], commission);
        assert_eq!(declared["strategy"]["commission_value"], 1.5);
    }
}

#[test]
fn indicator_sources_neither_advertise_nor_serve_trade_results() {
    // An indicator-sourced module links the runtime without a broker, whose
    // bytes this projection does not touch: it advertises exactly what it
    // did, and the flag is an unknown key it skips.
    let source = "indicator(\"X\")\nplot(close > open ? 1 : -1)\n";
    let bars = synthetic_bars();
    for module_type in [
        StandaloneWasmModuleType::Strategy,
        StandaloneWasmModuleType::Indicator,
    ] {
        let wasm = compile_standalone_wasm_program(
            &compile_csil(source),
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type,
            },
        )
        .expect("indicator-sourced standalone WebAssembly emit");
        assert_eq!(
            custom_section(&wasm, "pinewasm_capabilities").expect("capabilities"),
            br#"{"schema_version":1,"input_protocols":["json-v1","PWSWBIN1","PWSWBIN2","PWSWREF1"]}"#
        );
        let mut instance = StandaloneWasm::new(&wasm);
        let plain = instance.evaluate(&wire(&bars)).unwrap();
        assert!(plain.starts_with(b"{\"version\":1,\"signals\":["));
        assert_eq!(
            instance
                .evaluate(&request(&bars, "\"trade_results\":true,"))
                .unwrap(),
            plain
        );
    }
}

#[test]
fn a_resident_instance_stays_flat_across_five_thousand_trade_ledgers() {
    // The repeated-evaluation shape: one resident dataset, a fresh broker and fresh
    // parameters per trial, and the ledger read out each time. The ledger is
    // written from the broker in place, so nothing is left behind for the
    // next trial to stack on. A short window and a script without history
    // keep five thousand evaluations quick; it still closes a trade every
    // six bars.
    let source = concat!(
        "//@version=6\n",
        "strategy(\"Resident ledger\", initial_capital=10000, process_orders_on_close=true, ",
        "commission_type=strategy.commission.percent, commission_value=0.1)\n",
        "hold = input.int(2, \"Hold\", minval=1, maxval=5)\n",
        "if bar_index % 6 == 0\n",
        "    strategy.entry(\"L\", strategy.long)\n",
        "if bar_index % 6 == hold\n",
        "    strategy.close(\"L\")\n",
        "plot(close)\n",
    );
    let wasm = compile_strategy_standalone_wasm(source);
    let bars: Vec<Bar> = synthetic_bars().into_iter().take(48).collect();
    let metadata = |hold: u32| serde_json::json!({"version":1,"params":{"hold":hold},"mintick":0.01,"trade_results":true});
    let holds = [1u32, 2, 3, 4];
    let expected: Vec<Vec<u8>> =
        holds
            .iter()
            .map(|hold| {
                let output = StandaloneWasm::new(&wasm)
                    .evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata(*hold), 1, false));
                let document: serde_json::Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(rows(&document).len(), 8, "hold {hold}");
                assert!(rows(&document)
                    .iter()
                    .all(|row| int(row, "exitBar") - int(row, "entryBar") == u64::from(*hold)));
                output
            })
            .collect();
    assert!(expected.windows(2).all(|pair| pair[0] != pair[1]));

    let mut resident = StandaloneWasm::new(&wasm);
    assert_eq!(
        resident.evaluate_bytes_reclaiming(&resident_packet(&bars, &metadata(1), 7, false)),
        expected[0]
    );
    let references: Vec<Vec<u8>> = holds
        .iter()
        .map(|hold| resident_packet(&bars, &metadata(*hold), 7, true))
        .collect();
    // One settling pass over every parameter, as in the reclaim pins above.
    for (reference, expected) in references.iter().zip(&expected) {
        assert_eq!(&resident.evaluate_bytes_reclaiming(reference), expected);
    }
    let settled = resident.memory_bytes();
    for trial in 0..5_000 {
        let choice = trial % holds.len();
        assert_eq!(
            resident.evaluate_bytes_reclaiming(&references[choice]),
            expected[choice],
            "resident ledger drifted at trial {trial}"
        );
        assert_eq!(
            resident.memory_bytes(),
            settled,
            "linear memory grew at trial {trial}"
        );
    }
}

#[test]
fn mixing_result_formats_on_one_instance_leaks_nothing() {
    // Full results consume the broker; the two compact projections read it
    // in place. Whatever order a host asks in, each answer is the one a
    // fresh instance gives, and memory settles.
    let bars = exact_bars(90);
    let wasm = compile_strategy_standalone_wasm(PINNED_CLOSE_PASS);
    let requests: Vec<String> = [
        "\"trade_results\":true,",
        "",
        "\"include_results\":true,",
        "\"trade_results\":true,",
        "\"objective_results\":true,",
        "\"include_results\":true,\"binary_results\":true,",
    ]
    .iter()
    .map(|flags| request(&bars, flags))
    .collect();
    let expected: Vec<Vec<u8>> = requests
        .iter()
        .map(|input| StandaloneWasm::new(&wasm).evaluate_reclaiming(input))
        .collect();
    let ledger: serde_json::Value = serde_json::from_slice(&expected[0]).unwrap();
    assert!(rows(&ledger).len() >= 5, "the window must trade");
    let mut host = StandaloneWasm::new(&wasm);
    for (input, expected) in requests.iter().zip(&expected) {
        assert_eq!(&host.evaluate_reclaiming(input), expected);
    }
    let settled = host.memory_bytes();
    for round in 0..25 {
        for (input, expected) in requests.iter().zip(&expected) {
            assert_eq!(&host.evaluate_reclaiming(input), expected, "round {round}");
            assert_eq!(host.memory_bytes(), settled, "round {round}");
        }
    }
}
