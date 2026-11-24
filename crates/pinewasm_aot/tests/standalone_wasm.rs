//! The standalone WebAssembly export-mode contract test suite: the producer-side half of the public
//! Standalone WebAssembly v1 host contract.
//!
//! Four kinds of pinning cover the complete documented contract:
//!
//! 1. **Surface** — the emitted bytes carry exactly the five documented function
//!    exports plus `memory`, zero imports, and `audit_standalone_wasm` refuses every
//!    broken shape whether or not any emitter path produces it (the same
//!    bytes-not-flags posture as `tests/standalone.rs`).
//! 2. **Behavior** — the full host protocol (`alloc` → write IN →
//!    `evaluate_indicator_cabi` → unpack → `dealloc`) against a wasmi
//!    instance, cross-checked against the *same program's* standalone
//!    artifact run with host-written lanes: two independently-filled lane
//!    sets must edge-detect to the same signal series.
//! 3. **Error classes** — the exact `"Indicator failure: "` (expected,
//!    exit-1) and `"Invalid JSON: "` (infrastructure, exit-2) prefixes used
//!    by compatible hosts for exit-code mapping.
//! 4. **Statelessness and determinism** — byte-identical outputs a thousand
//!    times on a resident instance, and byte-identical artifacts
//!    from repeated compilation (the `moduleSha256` story depends on it).

use pinewasm_aot::{
    audit_standalone_wasm, compile_program, compile_standalone_program,
    compile_standalone_wasm_program, compile_standalone_wasm_program_with_config, header, schema,
    standalone_eligibility, status, AotError, StandaloneWasmBuildConfig, StandaloneWasmModuleType,
    StandaloneWasmOptions, StandaloneWasmSignalSemantics,
};
use pinewasm_compiler::{compile, CompileOptions};
use std::collections::BTreeSet;

/// The expected-failure class prefix, exactly as compatible hosts match
/// it: colon and trailing space included.
const INDICATOR_FAILURE: &str = "Indicator failure: ";
/// The tokenizer-failure prefix required for malformed-input handling.
const INVALID_JSON: &str = "Invalid JSON: ";

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

/// Compile a strategy-type standalone WebAssembly artifact with the default (single-plot)
/// signal designation.
fn compile_standalone_wasm(source: &str) -> Vec<u8> {
    compile_standalone_wasm_program(
        &compile_csil(source),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("standalone WebAssembly emit")
}

// ---------------------------------------------------------------------------
// Wire-format builders. `{:?}` on f64 is Rust's shortest-roundtrip Debug
// formatting, which is always a valid JSON number, so IN documents carry
// every awkward mantissa without loss.

#[derive(Clone, Copy)]
struct Bar {
    time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: Option<f64>,
}

/// A bar whose posture-relevant fields are `open`/`close`; high/low bracket
/// them and volume is present.
fn bar(time: i64, open: f64, close: f64) -> Bar {
    Bar {
        time,
        open,
        high: open.max(close) + 1.0,
        low: open.min(close) - 1.0,
        close,
        volume: Some(100.0),
    }
}

/// `n` bars whose `close > open` truth value alternates starting true, so a
/// `close > open ? 1 : -1` posture transitions on every bar.
fn alternating(n: usize) -> Vec<Bar> {
    (0..n)
        .map(|i| {
            let up = i % 2 == 0;
            bar(
                1_700_000_000_000 + i as i64 * 60_000,
                10.0,
                if up { 11.0 } else { 9.0 },
            )
        })
        .collect()
}

/// Build an IN document. `fields` lands verbatim before `"bars"` (with its
/// own trailing comma, or empty).
///
/// `version` and `params` are required alongside `bars` (only
/// `fraction`/`override_kernel`/`emit_debug` are optional),
/// so the helper supplies them — but only when `fields` does not already
/// carry that key, because a repeated recognized top-level key is itself an
/// error. A test that wants a malformed `version`/`params` therefore still
/// gets exactly the one defect it wrote.
fn wire(fields: &str, bars: &[Bar]) -> String {
    let mut prefix = String::from(fields);
    if !fields.contains("\"version\"") {
        prefix.push_str("\"version\":1,");
    }
    if !fields.contains("\"params\"") {
        prefix.push_str("\"params\":{},");
    }
    // The interpreter's default market data: UTC, no session declared, and a
    // 1D default timeframe. A module with lanes requires all three fields.
    if !fields.contains("\"timeframe\"") {
        prefix.push_str("\"timeframe\":\"1D\",");
    }
    if !fields.contains("\"timezone\"") {
        prefix.push_str("\"timezone\":\"UTC\",");
    }
    if !fields.contains("\"session_hours\"") {
        prefix.push_str("\"session_hours\":\"\",");
    }
    wire_raw(&prefix, bars)
}

/// [`wire`] without the required-key backfill: `fields` is the whole
/// top-level prefix, so a test can omit a required key deliberately.
fn wire_raw(fields: &str, bars: &[Bar]) -> String {
    let mut out = String::from("{");
    out.push_str(fields);
    out.push_str("\"bars\":[");
    for (i, b) in bars.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&bar_json(b));
    }
    out.push_str("]}");
    out
}

/// One OUT signal: `(time, direction, price)`.
type Signal = (i64, i64, f64);
/// A whole OUT `signals` array, in bar order.
type SignalSeries = Vec<Signal>;

/// One bar object, exactly as [`wire`] writes it.
fn bar_json(b: &Bar) -> String {
    let mut out = format!(
        "{{\"time\":{},\"open\":{:?},\"high\":{:?},\"low\":{:?},\"close\":{:?}",
        b.time, b.open, b.high, b.low, b.close
    );
    if let Some(v) = b.volume {
        out.push_str(&format!(",\"volume\":{v:?}"));
    }
    out.push('}');
    out
}

fn wire_v1(bars: &[Bar]) -> String {
    wire("", bars)
}

fn custom_json(wasm: &[u8], name: &str) -> serde_json::Value {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(section) = payload.expect("valid wasm") {
            if section.name() == name {
                return serde_json::from_slice(section.data()).expect("custom section JSON");
            }
        }
    }
    panic!("missing custom section {name}");
}

// ---------------------------------------------------------------------------
// The host side of the documented standalone WebAssembly protocol, including handle validation.

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
        // An empty linker: zero imports is the standalone WebAssembly contract.
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

    fn alloc(&mut self, len: usize) -> u32 {
        self.instance
            .get_typed_func::<i32, i32>(&self.store, "alloc")
            .expect("alloc export")
            .call(&mut self.store, len as i32)
            .expect("alloc call") as u32
    }

    fn dealloc(&mut self, ptr: u32, len: usize) {
        self.instance
            .get_typed_func::<(i32, i32), ()>(&self.store, "dealloc")
            .expect("dealloc export")
            .call(&mut self.store, (ptr as i32, len as i32))
            .expect("dealloc call");
    }

    fn reset(&mut self) {
        self.instance
            .get_typed_func::<(), ()>(&self.store, "reset")
            .expect("reset export")
            .call(&mut self.store, ())
            .expect("reset call");
    }

    /// `last_error()`, with its packed handle validated before reading.
    fn last_error(&mut self) -> Option<String> {
        let packed = self
            .instance
            .get_typed_func::<(), i64>(&self.store, "last_error")
            .expect("last_error export")
            .call(&mut self.store, ())
            .expect("last_error call") as u64;
        if packed == 0 {
            return None;
        }
        let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        let end = ptr.checked_add(len).expect("error handle overflows");
        assert!(end <= mem.len(), "error handle out of range");
        Some(String::from_utf8(mem[ptr..end].to_vec()).expect("error is UTF-8"))
    }

    /// One full protocol round: alloc, write, evaluate, unpack, read,
    /// dealloc output then input. A `0` return must come with a non-empty
    /// diagnostic — an empty error slot on a failure path is itself a bug.
    fn evaluate(&mut self, input: &str) -> Result<Vec<u8>, String> {
        let ptr = self.alloc(input.len());
        assert_ne!(ptr, 0, "alloc must not fail for {} bytes", input.len());
        self.memory.data_mut(&mut self.store)[ptr as usize..ptr as usize + input.len()]
            .copy_from_slice(input.as_bytes());
        let packed = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, "evaluate_indicator_cabi")
            .expect("evaluate export")
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("evaluate call") as u64;
        if packed == 0 {
            let message = self.last_error().expect("a 0 return must set last_error");
            assert!(!message.is_empty(), "the error slot must not be empty");
            self.dealloc(ptr, input.len());
            return Err(message);
        }
        let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        let end = out_ptr
            .checked_add(out_len)
            .expect("output handle overflows");
        assert!(end <= mem.len(), "output handle out of range");
        let out = mem[out_ptr..end].to_vec();
        self.dealloc(out_ptr as u32, out_len);
        self.dealloc(ptr, input.len());
        Ok(out)
    }
}

/// Parse an OUT document into `(time, direction, price)` triples, asserting
/// the output shape on the way: exactly three members per element, integer
/// direction in {1, −1}.
fn signals(out: &[u8]) -> SignalSeries {
    signals_with_domain(out, false)
}

/// Parse a flat-aware posture-event OUT document. Unlike [`signals`], this
/// accepts `0` exits as well as +1/-1 entries and reversals.
fn posture_signals(out: &[u8]) -> SignalSeries {
    signals_with_domain(out, true)
}

fn signals_with_domain(out: &[u8], allow_flat: bool) -> SignalSeries {
    let doc: serde_json::Value = serde_json::from_slice(out).expect("OUT is valid JSON");
    assert_eq!(doc["version"], 1, "OUT version");
    let object = doc.as_object().expect("OUT is an object");
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["signals", "version"],
        "OUT carries exactly version + signals"
    );
    doc["signals"]
        .as_array()
        .expect("signals array")
        .iter()
        .map(|s| {
            let element = s.as_object().expect("signal object");
            assert_eq!(element.len(), 3, "exactly {{time, direction, price}}: {s}");
            let direction = s["direction"].as_i64().expect("integer direction");
            assert!(
                direction == 1 || direction == -1 || (allow_flat && direction == 0),
                "direction must be 1 or -1{}, got {direction}",
                if allow_flat { " or 0" } else { "" }
            );
            (
                s["time"].as_i64().expect("integer time"),
                direction,
                s["price"].as_f64().expect("price"),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The standalone cross-check harness: the same program compiled through
// `compile_standalone_program`, its lanes written by this test the way the
// app loader writes them, its plot lane read back raw.

fn read_u32(mem: &[u8], off: u32) -> u32 {
    u32::from_le_bytes(mem[off as usize..off as usize + 4].try_into().unwrap())
}

fn run_standalone_plot0(wasm: &[u8], bars: &[Bar]) -> Vec<f64> {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, wasm).expect("standalone module");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = <wasmi::Linker<()>>::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("standalone instantiates with no imports");
    let n = bars.len();
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(prepare.call(&mut store, n as i32).unwrap(), status::OK);
    let memory = instance.get_memory(&store, "memory").expect("memory");
    {
        let mem = memory.data_mut(&mut store);
        let builtin_base = read_u32(mem, header::OFF_BUILTIN_BASE) as usize;
        for (i, b) in bars.iter().enumerate() {
            let lanes = [
                b.open,
                b.high,
                b.low,
                b.close,
                b.volume.unwrap_or(f64::NAN),
                b.time as f64,
            ];
            for (lane, v) in lanes.iter().enumerate() {
                let at = builtin_base + lane * 8 * n + i * 8;
                mem[at..at + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    let run = instance
        .get_typed_func::<(), i32>(&store, "run")
        .expect("run export");
    assert_eq!(run.call(&mut store, ()).unwrap(), status::OK);
    let mem = memory.data(&store);
    let plots_base = read_u32(mem, header::OFF_PLOTS_BASE) as usize;
    (0..n)
        .map(|i| {
            f64::from_le_bytes(
                mem[plots_base + i * 8..plots_base + i * 8 + 8]
                    .try_into()
                    .unwrap(),
            )
        })
        .collect()
}

/// The expected edge detector: NaN is flat, transitions *into* ±1 emit,
/// price is the bar's close. Any non-posture value is a test bug here (the
/// module reports it as an indicator failure, tested separately).
fn edge_detect(plot: &[f64], bars: &[Bar]) -> SignalSeries {
    let mut prev = 0i64;
    let mut out = Vec::new();
    for (i, &v) in plot.iter().enumerate() {
        let cur = if v.is_nan() {
            0
        } else if v == 1.0 {
            1
        } else if v == -1.0 {
            -1
        } else if v == 0.0 {
            0
        } else {
            panic!("non-posture plot value {v} at bar {i}");
        };
        if cur != 0 && cur != prev {
            out.push((bars[i].time, cur, bars[i].close));
        }
        prev = cur;
    }
    out
}

/// Read a custom section's payload from module bytes (first occurrence).
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

// ---------------------------------------------------------------------------
// A dependency-free SHA-256, here only to state the artifact-identity claim
// in the same terms the bindings' `moduleSha256` states it.

fn sha256(bytes: &[u8]) -> [u8; 32] {
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
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
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
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
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
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, v) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&v.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// 1. Surface.

const POSTURE: &str = "indicator(\"P\")\nplot(close > open ? 1 : -1)\n";

#[test]
fn the_surface_is_exactly_the_five_exports_and_memory() {
    let wasm = compile_standalone_wasm(POSTURE);
    audit_standalone_wasm(&wasm).expect("the emitted module passes its own audit");

    // Independently of `audit_standalone_wasm` (which is itself under test below), walk
    // the bytes: zero imports, the exact export set, nothing leaked.
    let mut import_count = 0usize;
    let mut func_exports = BTreeSet::new();
    let mut memory_exported = false;
    for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
        match payload.expect("module parses") {
            wasmparser::Payload::ImportSection(reader) => {
                for group in reader {
                    import_count += group.expect("import group").into_iter().count();
                }
            }
            wasmparser::Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.expect("export entry");
                    match export.kind {
                        wasmparser::ExternalKind::Func => {
                            func_exports.insert(export.name.to_string());
                        }
                        wasmparser::ExternalKind::Memory => {
                            assert_eq!(export.name, "memory");
                            memory_exported = true;
                        }
                        other => panic!("unexpected export kind {other:?}"),
                    }
                }
            }
            _ => {}
        }
    }
    assert_eq!(import_count, 0, "the import section must be empty");
    assert!(memory_exported, "memory must be exported");
    let expected: BTreeSet<String> = [
        "evaluate_indicator_cabi",
        "alloc",
        "dealloc",
        "last_error",
        "reset",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        func_exports, expected,
        "exactly the five spec'd function exports, nothing else"
    );
}

#[test]
fn a_ta_program_is_a_conformant_standalone_wasm_artifact() {
    // The TA engine ships inside the module, so an engine-backed program can
    // satisfy the import-free standalone WebAssembly surface just like a scalar-only program.
    let program = compile_csil("indicator(\"T\")\nplot(ta.ema(close, 9) > close ? 1 : -1)\n");
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: Some(0),
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("a ta program lowers to a valid standalone WebAssembly artifact");
    audit_standalone_wasm(&wasm)
        .expect("the emitted bytes pass the standalone WebAssembly surface audit");
}

#[test]
fn a_source_language_strategy_compiles_to_standalone_wasm_with_named_feature_gates() {
    // Native strategy() lowering: the blanket preflight refusal is
    // gone from the standalone WebAssembly path — a plain strategy compiles, with the broker
    // spliced in — and what remains are per-feature gates, each named so
    // the UI can say exactly which declaration argument keeps a script out.
    // (The chart tier still refuses strategies: the CSAW v6 result ABI has
    // no StrategyOutcome section.)
    let program = compile_csil(
        "strategy(\"S\")\nif close > open\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("a strategy source lowers natively to standalone WebAssembly");
    audit_standalone_wasm(&wasm)
        .expect("the emitted strategy bytes pass the standalone WebAssembly audit");

    // The feature gates refuse by name (the full pin set, incl. the
    // accepted-and-inert calc_on_every_tick, lives in tests/standalone_wasm_strategy.rs).
    let gated = compile_csil(
        "strategy(\"S\", calc_on_order_fills = true)\nstrategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
    let error = compile_standalone_wasm_program(
        &gated,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect_err("calc_on_order_fills needs intra-bar re-execution");
    assert!(
        matches!(
            &error,
            AotError::StrategyFeatureUnsupported { feature } if feature == "calc_on_order_fills"
        ),
        "expected the named feature gate, got {error:?}"
    );
}

#[test]
fn signal_designation_follows_the_g1_rules() {
    let two_plots = compile_csil("indicator(\"2\")\nplot(close > open ? 1 : -1)\nplot(close)\n");
    let strategy = |signal_plot| StandaloneWasmOptions {
        signal_plot,
        module_type: StandaloneWasmModuleType::Strategy,
    };

    // Multi-plot with no designation: refused, not guessed.
    let error = compile_standalone_wasm_program(&two_plots, &strategy(None))
        .expect_err("two plots need an explicit designation");
    assert!(
        matches!(error, AotError::StandaloneWasmNoDesignatedPlot(_)),
        "{error:?}"
    );

    // Explicit designation works, out-of-range is refused.
    compile_standalone_wasm_program(&two_plots, &strategy(Some(0))).expect("explicit plot 0");
    let error = compile_standalone_wasm_program(&two_plots, &strategy(Some(2)))
        .expect_err("plot 2 of 2 is out of range");
    assert!(
        matches!(error, AotError::StandaloneWasmNoDesignatedPlot(_)),
        "{error:?}"
    );

    // An indicator-type module designates nothing, and saying otherwise is
    // an error rather than a silently-dropped field.
    let one_plot = compile_csil(POSTURE);
    let error = compile_standalone_wasm_program(
        &one_plot,
        &StandaloneWasmOptions {
            signal_plot: Some(0),
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect_err("indicator-type with a designation is contradictory");
    assert!(
        matches!(error, AotError::StandaloneWasmNoDesignatedPlot(_)),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Behavior.

#[test]
fn signals_match_the_edge_detected_standalone_plot_lane() {
    // Two programs: a three-way posture, and one whose bar-0 posture is na
    // (na condition ⇒ na result), so the NaN-is-flat rule is exercised.
    for source in [
        "indicator(\"X\")\nplot(close > open ? 1 : close < open ? -1 : 0)\n",
        "indicator(\"Y\")\nplot(close > close[1] ? 1 : -1)\n",
    ] {
        // Bars engineered to hold each posture for a stretch: up, up, flat,
        // down, down, up, flat, down…
        let pattern = [
            1.0, 1.0, 0.0, -1.0, -1.0, 1.0, 0.0, -1.0, 1.0, -1.0, 0.0, 1.0,
        ];
        let bars: Vec<Bar> = pattern
            .iter()
            .cycle()
            .take(48)
            .enumerate()
            .map(|(i, &dir)| {
                bar(
                    1_700_000_000_000 + i as i64 * 60_000,
                    10.0 + (i % 7) as f64,
                    10.0 + (i % 7) as f64 + dir,
                )
            })
            .collect();

        let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(source));
        let got = signals(
            &standalone_wasm
                .evaluate(&wire_v1(&bars))
                .expect("evaluate succeeds"),
        );

        // The same program through the standalone tier, lanes written by the
        // test: an independent fill path for the same semantics.
        let standalone = compile_standalone_program(&compile_csil(source)).expect("standalone");
        let plot = run_standalone_plot0(&standalone.wasm, &bars);
        let expected = edge_detect(&plot, &bars);

        assert_eq!(got.len(), expected.len(), "{source}: signal count");
        for (got, expected) in got.iter().zip(&expected) {
            assert_eq!(got.0, expected.0, "{source}: time");
            assert_eq!(got.1, expected.1, "{source}: direction");
            assert_eq!(
                got.2.to_bits(),
                expected.2.to_bits(),
                "{source}: price must be the close, bit-exact"
            );
        }
    }
}

#[test]
fn numeric_math_overloads_match_expected_values_in_emitted_wasm() {
    // Each case is a math overload PineWASM implements, with its expected
    // value on each of the two bars.
    let bars = [bar(1, 10.0, 10.0), bar(2, 100.0, 100.0)];
    let cases = [
        ("math.abs(-3.25)", [3.25, 3.25]),
        ("math.sign(-3)", [-1.0, -1.0]),
        ("math.round(2.567, 2)", [2.57, 2.57]),
        ("math.round_to_mintick(close)", [10.0, 100.0]),
        ("math.todegrees(math.pi)", [180.0, 180.0]),
        (
            "math.toradians(180.0)",
            [std::f64::consts::PI, std::f64::consts::PI],
        ),
        ("math.sum(close, 1)", [10.0, 100.0]),
        ("math.sqrt(9.0)", [3.0, 3.0]),
        ("math.log(1.0)", [0.0, 0.0]),
        ("math.sin(0.0)", [0.0, 0.0]),
        ("math.pow(2.0, 3.0)", [8.0, 8.0]),
        ("math.min(3, 1, 2)", [1.0, 1.0]),
        ("math.max(3.5, 1.5, 2.5)", [3.5, 3.5]),
        ("math.avg(3, 1, 2)", [2.0, 2.0]),
        ("math.log10(close)", [1.0, 2.0]),
        ("math.exp(0.0)", [1.0, 1.0]),
        ("math.cos(0.0)", [1.0, 1.0]),
        ("math.tan(0.0)", [0.0, 0.0]),
        ("math.asin(0.0)", [0.0, 0.0]),
        ("math.acos(1.0)", [0.0, 0.0]),
        ("math.atan(0.0)", [0.0, 0.0]),
    ];
    for (expression, expected) in cases {
        let source = format!("//@version=6\nindicator(\"math wasm check\")\nplot({expression})");
        let standalone = compile_standalone_program(&compile_csil(&source))
            .unwrap_or_else(|error| panic!("compile {expression} to standalone wasm: {error:?}"));
        let actual = run_standalone_plot0(&standalone.wasm, &bars);
        assert_eq!(actual.len(), expected.len(), "{expression}");
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected).abs() <= 1e-12,
                "{expression}: expected {expected}, got {actual}"
            );
        }
    }
}

#[test]
fn designated_plot_posture_semantics_are_metadata_driven_and_preserve_flat_exits() {
    let program = compile_csil(concat!(
        "indicator(\"Flat-aware\")\n",
        "plot(close > open ? 1 : close < open ? -1 : 0, title = \"Backtest Signal\")\n",
    ));
    let options = StandaloneWasmOptions {
        signal_plot: Some(0),
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let bars = [
        bar(1, 10.0, 11.0),
        bar(2, 10.0, 12.0),
        bar(3, 10.0, 10.0),
        bar(4, 10.0, 10.0),
        bar(5, 10.0, 9.0),
        bar(6, 10.0, 10.0),
        bar(7, 10.0, 11.0),
    ];

    // The compatibility default remains reversal-only.
    let default_wasm = compile_standalone_wasm_program(&program, &options)
        .expect("default standalone WebAssembly emit");
    let default_meta = custom_json(&default_wasm, schema::MODULE_METADATA_SECTION);
    assert_eq!(default_meta["signal"]["semantics"], "position-events");
    let mut default_standalone_wasm = StandaloneWasm::new(&default_wasm);
    let default_directions = signals(
        &default_standalone_wasm
            .evaluate(&wire_v1(&bars))
            .expect("default evaluation"),
    )
    .into_iter()
    .map(|signal| signal.1)
    .collect::<Vec<_>>();
    assert_eq!(default_directions, [1, -1, 1]);

    // An explicit posture designation changes both the declared contract and
    // the actual serializer. A host can therefore dispatch from metadata;
    // no test- or host-side semantic override is needed.
    let posture_wasm = compile_standalone_wasm_program_with_config(
        &program,
        &options,
        &StandaloneWasmBuildConfig {
            signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
            ..StandaloneWasmBuildConfig::default()
        },
    )
    .expect("posture standalone WebAssembly emit");
    audit_standalone_wasm(&posture_wasm).expect("posture artifact passes producer audit");
    let posture_meta = custom_json(&posture_wasm, schema::MODULE_METADATA_SECTION);
    assert_eq!(posture_meta["signal"]["semantics"], "posture-events");
    let semantics = posture_meta["signal"]["semantics"]
        .as_str()
        .expect("semantic enum");
    let mut posture_standalone_wasm = StandaloneWasm::new(&posture_wasm);
    let out = posture_standalone_wasm
        .evaluate(&wire_v1(&bars))
        .expect("posture evaluation");
    let posture = match semantics {
        "posture-events" => posture_signals(&out),
        "position-events" => signals(&out),
        other => panic!("unknown signal semantics {other}"),
    };
    assert_eq!(
        posture
            .into_iter()
            .map(|signal| signal.1)
            .collect::<Vec<_>>(),
        [1, 0, -1, 0, 1]
    );
}

#[test]
fn scalar_cast_chain_that_can_saturate_to_min_is_standalone_behind_a_guard() {
    let source = concat!(
        "indicator(\"CAST-standalone WebAssembly\")\n",
        "converted = float(int(close - open))\n",
        "plot(bool(converted) ? 1 : -1)\n",
    );
    let program = compile_csil(source);
    // A finite float can truncate to a present `i64::MIN`. The cast is no
    // longer refused: the module carries a guard on it.
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("the guarded cast lowers to a standalone module");
    audit_standalone_wasm(&wasm).expect("the guarded module passes the public audit");
    let native = pinewasm_aot::compile_program_to_wasm(&program).expect("universal entry point");
    assert_eq!(
        native.layout.execution_mode,
        pinewasm_aot::ExecutionMode::Native
    );

    let mut module = StandaloneWasm::new(&wasm);
    module
        .evaluate(&wire_v1(&alternating(8)))
        .expect("ordinary bars evaluate");
    // `close - open` is exactly -2^63 on the second bar. The interpreter
    // keeps the minimum as an integer; the module has no cell for it and no
    // interpreter to hand the request to, so the evaluation fails with the
    // mismatch status instead of treating the value as missing.
    let mut bars = alternating(3);
    bars[1] = bar(bars[1].time, 0.0, i64::MIN as f64);
    let error = module
        .evaluate(&wire_v1(&bars))
        .expect_err("a present i64::MIN must not evaluate");
    assert_eq!(error, "unexpected status 6");
    module
        .evaluate(&wire_v1(&alternating(8)))
        .expect("the instance stays usable after the refusal");
}

#[test]
fn params_overlay_the_declared_defaults_by_derived_key() {
    let source = concat!(
        "indicator(\"P\")\n",
        "thr = input.float(0.5, \"Threshold\")\n",
        "inv = input.bool(false, \"Invert\")\n",
        "m = input.int(2, \"Mode\", minval=0, maxval=3)\n",
        "src = input.source(close, \"Basis\")\n",
        "p = src - open > thr ? 1 : -1\n",
        "q = m == 2 ? p : -p\n",
        "plot(inv ? -q : q)\n",
    );
    let wasm = compile_standalone_wasm(source);
    // One bar with close − open = 1.0, so the default threshold 0.5 yields
    // posture +1 and each overlay below flips it in its own way.
    let bars = [bar(1_700_000_000_000, 10.0, 11.0)];
    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let direction = |standalone_wasm: &mut StandaloneWasm, params: &str| {
        let input = wire(&format!("\"version\":1,\"params\":{params},"), &bars);
        let got = signals(&standalone_wasm.evaluate(&input).expect("evaluate succeeds"));
        assert_eq!(got.len(), 1);
        got[0].1
    };

    assert_eq!(
        direction(&mut standalone_wasm, "{}"),
        1,
        "declared defaults"
    );
    assert_eq!(
        direction(&mut standalone_wasm, "{\"threshold\":2.0}"),
        -1,
        "float overlay"
    );
    assert_eq!(
        direction(&mut standalone_wasm, "{\"invert\":true}"),
        -1,
        "bool overlay"
    );
    assert_eq!(
        direction(&mut standalone_wasm, "{\"mode\":3}"),
        -1,
        "exact-i64 overlay"
    );
    assert_eq!(
        direction(&mut standalone_wasm, "{\"mode\":3.0}"),
        -1,
        "float-form integral value for an int param"
    );
    assert_eq!(
        direction(
            &mut standalone_wasm,
            "{\"unknown_key\":123,\"threshold\":2.0}"
        ),
        -1,
        "unknown params keys are ignored, known ones still apply"
    );
    // A fractional value for an int param is an input error, not a rounding.
    let input = wire("\"version\":1,\"params\":{\"mode\":2.5},", &bars);
    let error = standalone_wasm
        .evaluate(&input)
        .expect_err("non-integral int param");
    assert!(
        error.contains("integer") && !error.starts_with(INDICATOR_FAILURE),
        "infrastructure-class integer error, got {error:?}"
    );
}

#[test]
fn finite_string_options_use_integer_indexes_and_canonical_comparisons() {
    let source = concat!(
        "indicator(\"STRING-OPTIONS\")\n",
        "feature = input.string(\"RSI\", \"Feature\", options=[\"RSI\", \"WT\", \"CCI\"])\n",
        "posture = feature == \"RSI\" ? 1 : feature == \"WT\" ? -1 : close > open ? 1 : -1\n",
        "plot(posture)\n",
    );
    let wasm = compile_standalone_wasm(source);
    audit_standalone_wasm(&wasm).expect("finite string options stay importless");
    let schema = custom_json(&wasm, schema::PARAMETER_SCHEMA_SECTION);
    assert_eq!(
        schema,
        serde_json::json!([{
            "name": "feature",
            "label": "Feature",
            "type": "int",
            "default": 0,
            "min": 0,
            "max": 2,
            "step": 1
        }]),
        "the standalone WebAssembly-only schema exposes declared option indexes"
    );

    let bars = [
        bar(1_700_000_000_000, 10.0, 11.0),
        bar(1_700_000_060_000, 10.0, 9.0),
    ];
    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let directions = |standalone_wasm: &mut StandaloneWasm, index: i64| {
        let input = wire(
            &format!("\"version\":1,\"params\":{{\"feature\":{index}}},"),
            &bars,
        );
        signals(
            &standalone_wasm
                .evaluate(&input)
                .expect("evaluate string option"),
        )
        .into_iter()
        .map(|signal| signal.1)
        .collect::<Vec<_>>()
    };

    assert_eq!(directions(&mut standalone_wasm, 0), [1], "RSI option");
    assert_eq!(directions(&mut standalone_wasm, 1), [-1], "WT option");
    assert_eq!(directions(&mut standalone_wasm, 2), [1, -1], "CCI option");
    assert_eq!(
        directions(&mut standalone_wasm, 99),
        [1],
        "an out-of-range wire index falls back to the declared default"
    );
}

#[test]
fn duplicate_constant_pool_strings_are_canonicalized_for_standalone_wasm_equality() {
    // Compiler-authored CSIL interns equal strings, but the public serialized
    // CsilProgram surface permits distinct handles with equal contents.  The
    // The interpreter compares content, so standalone WebAssembly must not compare those raw handles.
    let source = concat!(
        "indicator(\"DUPLICATE-POOL\")\n",
        "left = close > open ? \"same\" : \"left\"\n",
        "right = close > open ? \"same\" : \"right\"\n",
        "plot(left == right ? 1 : -1)\n",
    );
    let mut program = compile_csil(source);
    let same = program
        .constants
        .iter()
        .position(|value| value == "same")
        .expect("same constant") as u32;
    let duplicate = program.constants.len() as u32;
    program.constants.push("same".to_string());
    let mut seen = 0;
    for instr in &mut program.functions[0].code {
        if matches!(instr, pinewasm_csil::CsilInstr::PushStr(handle) if *handle == same) {
            seen += 1;
            if seen == 2 {
                *instr = pinewasm_csil::CsilInstr::PushStr(duplicate);
            }
        }
    }
    assert!(seen >= 2, "fixture must push the interned value twice");
    pinewasm_csil::validate(&program).expect("duplicate-content pool is valid CSIL");

    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect("standalone WebAssembly canonicalizes duplicate string handles");
    audit_standalone_wasm(&wasm).expect("canonicalized module stays importless");

    let bars = [
        bar(1_700_000_000_000, 10.0, 11.0),
        bar(1_700_000_060_000, 10.0, 9.0),
    ];
    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let got = signals(
        &standalone_wasm
            .evaluate(&wire_v1(&bars))
            .expect("evaluate equality"),
    );
    assert_eq!(
        got.iter().map(|signal| signal.1).collect::<Vec<_>>(),
        [1, -1],
        "equal contents compare equal even when input CSIL used distinct handles"
    );
}

#[test]
fn dynamic_strings_and_signal_relevant_drawing_values_fail_closed() {
    for (label, source) in [
        (
            "unbounded input.string",
            "indicator(\"DYNAMIC\")\nfeature = input.string(\"RSI\", \"Feature\")\nplot(feature == \"RSI\" ? 1 : -1)\n",
        ),
        (
            "runtime string",
            "indicator(\"DYNAMIC\")\ntext = str.tostring(close)\nplot(text == \"1\" ? 1 : -1)\n",
        ),
        (
            "drawing handle reaches signal",
            "indicator(\"DRAWING-SIGNAL\")\nitem = label.new(bar_index, close, \"x\")\nplot(label.get_x(item) >= 0 ? 1 : -1)\n",
        ),
    ] {
        let program = compile_csil(source);
        let error = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Strategy,
            },
        )
        .expect_err(label);
        assert!(
            matches!(error, AotError::StandaloneWasmNotStandalone(_)),
            "{label} must retain host.vm, got {error:?}"
        );
    }
}

#[test]
fn label_and_table_text_chains_fail_closed_in_standalone_wasm() {
    // `label.new`, `str.tostring` and `table.cell` can each fail in the
    // interpreter (object and string limits, out-of-range cells), so the
    // projection keeps them as host sites instead of erasing them: a
    // standalone run could otherwise complete where the interpreter reports an error.
    let source = concat!(
        "indicator(\"CHART-ONLY\")\n",
        "label.new(bar_index, close, str.tostring(close))\n",
        "var panel = table.new(position.top_right, 1, 1)\n",
        "table.cell(panel, 0, 0, str.tostring(close) + \" px\")\n",
        "plot(close > open ? 1 : -1)\n",
    );
    let program = compile_csil(source);
    let ordinary = compile_program(&program).expect("ordinary AOT keeps chart host semantics");
    assert!(
        ordinary.layout.imports_vm,
        "ordinary AOT still uses host.vm"
    );

    let error = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .expect_err("fallible chart effects are not erased");
    assert!(
        error.to_string().contains("program would import host.vm"),
        "{error}"
    );
}

#[test]
fn fraction_selects_the_max_one_floor_prefix() {
    // Alternating posture: every evaluated bar emits a signal, so the
    // signal count *is* the evaluated-prefix length.
    let bars = alternating(10);
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let count = |standalone_wasm: &mut StandaloneWasm, fields: &str| {
        signals(
            &standalone_wasm
                .evaluate(&wire(fields, &bars))
                .expect("evaluate succeeds"),
        )
        .len()
    };

    assert_eq!(
        count(&mut standalone_wasm, "\"version\":1,\"params\":{},"),
        10,
        "absent = full"
    );
    assert_eq!(
        count(&mut standalone_wasm, "\"fraction\":1.0,"),
        10,
        "1.0 = full"
    );
    // 10 × 0.35 floors to 3: signals only from bars 0..3.
    assert_eq!(count(&mut standalone_wasm, "\"fraction\":0.35,"), 3);
    let got = signals(
        &standalone_wasm
            .evaluate(&wire("\"fraction\":0.35,", &bars))
            .expect("evaluate succeeds"),
    );
    assert_eq!(
        got.last().unwrap().0,
        bars[2].time,
        "no signal may reference a bar outside the evaluated prefix"
    );
    // 10 × 0.05 floors to 0; the max(1, ·) floor keeps one bar.
    assert_eq!(count(&mut standalone_wasm, "\"fraction\":0.05,"), 1);

    // Out-of-range fractions are infrastructure-class input errors.
    for fields in [
        "\"fraction\":0.0,",
        "\"fraction\":-0.5,",
        "\"fraction\":1.5,",
    ] {
        let error = standalone_wasm
            .evaluate(&wire(fields, &bars))
            .expect_err(fields);
        assert!(
            error.contains("fraction") && !error.starts_with(INDICATOR_FAILURE),
            "{fields}: got {error:?}"
        );
    }
}

#[test]
fn wire_tolerance_covers_aliases_nulls_and_unknown_fields() {
    let bars = alternating(6);
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let baseline = standalone_wasm.evaluate(&wire_v1(&bars)).expect("baseline");

    // `timestamp` is accepted wherever `time` is.
    let aliased = wire_v1(&bars).replace("\"time\":", "\"timestamp\":");
    assert_eq!(
        standalone_wasm.evaluate(&aliased).expect("timestamp alias"),
        baseline,
        "the timestamp alias must not change the output"
    );

    // Inapplicable optional fields, nulls, and unknown keys at every level
    // accepted and ignored.
    let tolerant = wire(
        concat!(
            "\"version\":1,",
            "\"override_kernel\":null,",
            "\"emit_debug\":false,",
            "\"future_field\":{\"nested\":[1,2,{\"deep\":true}]},",
            "\"params\":{\"no_such_param\":42},",
        ),
        &bars,
    );
    assert_eq!(
        standalone_wasm.evaluate(&tolerant).expect("tolerant input"),
        baseline,
        "ignorable fields must not change the output"
    );

    // Unknown per-bar keys are ignored too.
    let with_bar_extras = wire_v1(&bars).replace("\"open\":", "\"oi\":7,\"open\":");
    assert_eq!(
        standalone_wasm
            .evaluate(&with_bar_extras)
            .expect("bar extras"),
        baseline
    );
}

#[test]
fn a_missing_volume_reads_as_na() {
    // The posture *is* the na-ness of volume, so the two wires produce
    // opposite series.
    let wasm = compile_standalone_wasm("indicator(\"V\")\nplot(na(volume) ? 1 : -1)\n");
    let mut with_volume = alternating(3);
    let without_volume: Vec<Bar> = with_volume
        .iter()
        .map(|b| Bar { volume: None, ..*b })
        .collect();
    for b in &mut with_volume {
        b.volume = Some(5.0);
    }

    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let got = signals(
        &standalone_wasm
            .evaluate(&wire_v1(&with_volume))
            .expect("with volume"),
    );
    assert_eq!(got.iter().map(|s| s.1).collect::<Vec<_>>(), [-1]);
    let got = signals(
        &standalone_wasm
            .evaluate(&wire_v1(&without_volume))
            .expect("without volume"),
    );
    assert_eq!(
        got.iter().map(|s| s.1).collect::<Vec<_>>(),
        [1],
        "a missing volume must fill the lane with NaN"
    );
}

#[test]
fn f64_prices_round_trip_bit_exactly() {
    // The posture keys off `open` alone, so `close` is free to carry every
    // awkward mantissa; alternation makes every bar a transition, so every
    // value comes back as a signal price.
    let wasm = compile_standalone_wasm("indicator(\"F\")\nplot(open < 0 ? 1 : -1)\n");
    let awkward: [f64; 16] = [
        0.1,
        0.2,
        0.30000000000000004,
        1.0000000001,
        1e-10,
        9007199254740991.0,                    // 2^53 − 1
        f64::from_bits(0x3FF0_0000_0000_0001), // 1 + one ulp
        5e-324,                                // smallest subnormal
        // The famous parser-killer subnormal, bit-identical to the classic
        // literal 2.2250738585072011e-308 (largest subnormal, one ulp below
        // the smallest normal on the next line).
        2.225_073_858_507_201e-308,
        2.2250738585072014e-308, // smallest normal
        1.7976931348623157e308,  // f64::MAX
        1234567.8901234567,
        std::f64::consts::PI,
        -0.1,
        -2.5e-15,
        123_456_789.000_000_12, // bit-identical to 123456789.000000123
    ];
    let bars: Vec<Bar> = awkward
        .iter()
        .enumerate()
        .map(|(i, &price)| Bar {
            time: 1_700_000_000_000 + i as i64 * 60_000,
            open: if i % 2 == 0 { -1.0 } else { 1.0 },
            high: 1.0,
            low: -1.0,
            close: price,
            volume: None,
        })
        .collect();

    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let got = signals(
        &standalone_wasm
            .evaluate(&wire_v1(&bars))
            .expect("evaluate succeeds"),
    );
    assert_eq!(got.len(), awkward.len(), "every bar transitions");
    for (i, ((time, _, price), expected)) in got.iter().zip(&awkward).enumerate() {
        assert_eq!(*time, bars[i].time, "times survive exactly");
        assert_eq!(
            price.to_bits(),
            expected.to_bits(),
            "price {i} must survive IN-JSON → lane → OUT-JSON bit-exactly: {expected:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Error classes.

#[test]
fn tokenizer_failures_carry_the_invalid_json_prefix() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    for input in [
        "{\"bars\": [",
        "not json at all",
        "{\"bars\":[{\"time\":1,\"open\":1,\"high\":1,\"low\":1,\"close\":1}]} trailing",
        "",
    ] {
        let error = standalone_wasm.evaluate(input).expect_err(input);
        assert!(
            error.starts_with(INVALID_JSON),
            "{input:?}: expected the exact {INVALID_JSON:?} prefix, got {error:?}"
        );
    }
}

#[test]
fn shape_and_version_failures_are_infrastructure_class() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let bars = alternating(2);
    // Every document below is well-formed apart from the one defect its
    // label names, and each pins its own diagnostic — a shape test that
    // passes for the wrong reason is how a missing required-field check
    // stayed invisible in the first place.
    for (label, input, needle) in [
        ("wrong version", wire("\"version\":2,", &bars), "version"),
        (
            "empty bars",
            String::from("{\"version\":1,\"params\":{},\"bars\":[]}"),
            "empty",
        ),
        (
            "missing bars",
            String::from("{\"version\":1,\"params\":{}}"),
            "missing required field: bars",
        ),
        (
            "non-object bar",
            String::from("{\"version\":1,\"params\":{},\"bars\":[1,2]}"),
            "objects",
        ),
        (
            "missing required bar field",
            String::from("{\"version\":1,\"params\":{},\"bars\":[{\"time\":1,\"open\":1.0}]}"),
            "missing a required field",
        ),
        (
            "params not an object",
            wire("\"params\":[],", &bars),
            "params",
        ),
    ] {
        let error = standalone_wasm.evaluate(&input).expect_err(label);
        assert!(
            !error.starts_with(INDICATOR_FAILURE),
            "{label}: must be infrastructure class, got {error:?}"
        );
        assert!(
            error.contains(needle),
            "{label}: expected a diagnostic naming {needle:?}, got {error:?}"
        );
    }
}

#[test]
fn the_required_in_fields_are_required() {
    // The standalone WebAssembly input contract makes `fraction`/`override_kernel`/`emit_debug`
    // optional and requires `version`, `params`, and `bars`. Omitting one is
    // an infrastructure-class, named error—never a silently defaulted field.
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let bars = alternating(2);
    for (omitted, input) in [
        ("version", wire_raw("\"params\":{},", &bars)),
        ("params", wire_raw("\"version\":1,", &bars)),
    ] {
        let error = standalone_wasm.evaluate(&input).expect_err(omitted);
        assert_eq!(
            error,
            format!("missing required field: {omitted}"),
            "the omission must be named exactly"
        );
        assert!(
            !error.starts_with(INDICATOR_FAILURE),
            "infrastructure class"
        );
    }
    // The complete document still succeeds on the same instance.
    assert_eq!(
        signals(
            &standalone_wasm
                .evaluate(&wire_v1(&bars))
                .expect("complete input")
        )
        .len(),
        2
    );
}

#[test]
fn duplicate_recognized_top_level_keys_are_refused() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let bars = alternating(2);
    let one = wire_v1(&bars[..1]);
    let body = one.trim_start_matches('{').trim_end_matches('}');

    // The regression this test exists for: a second `"bars"` used to add its
    // elements to the scan pass's count while the fill pass restarted at bar
    // 0, so `slice_end` ran past the bars actually written and the
    // serializer read stale lane bytes — a SUCCESS document carrying a
    // phantom signal whose time matched no input bar, and whose bytes
    // differed between a fresh and a resident instance.
    let duplicate_bars = format!("{{{body},\"bars\":[{}]}}", bar_json(&bars[1]));
    let error = standalone_wasm
        .evaluate(&duplicate_bars)
        .expect_err("a duplicate `bars` must be refused, not partially filled");
    assert_eq!(error, "duplicate top-level key: bars");

    for (key, input) in [
        ("version", format!("{{{body},\"version\":1}}")),
        ("params", format!("{{{body},\"params\":{{}}}}")),
        (
            "fraction",
            format!("{{\"fraction\":1.0,{body},\"fraction\":0.5}}"),
        ),
    ] {
        let error = standalone_wasm.evaluate(&input).expect_err(key);
        assert_eq!(error, format!("duplicate top-level key: {key}"));
        assert!(
            !error.starts_with(INDICATOR_FAILURE),
            "infrastructure class"
        );
    }

    // Unknown keys are accept-and-ignore, so a *duplicated*
    // unknown key changes nothing in either pass and stays tolerated.
    let baseline = standalone_wasm.evaluate(&wire_v1(&bars)).expect("baseline");
    let duplicated_unknown = wire("\"future_field\":1,\"future_field\":[2],", &bars);
    assert_eq!(
        standalone_wasm
            .evaluate(&duplicated_unknown)
            .expect("duplicated unknown key"),
        baseline,
        "a repeated unknown key must remain ignorable"
    );
}

#[test]
fn standalone_wasm_admits_integer_loops_behind_the_minimum_guard() {
    let program = compile_csil(concat!(
        "indicator(\"B\")\n",
        "sum = 0.0\n",
        "for i = 0 to 20000000\n",
        "    sum := sum + 1\n",
        "plot(sum >= 0.0 ? 1 : -1)\n",
    ));
    // The loop counter's `IntAdd` crosses the loop join, so its range is
    // unknown. It carries a minimum guard instead of refusing the program.
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("the guarded loop lowers to a standalone module");
    audit_standalone_wasm(&wasm).expect("the guarded module passes the public audit");
    let native = pinewasm_aot::compile_program_to_wasm(&program).expect("universal entry point");
    assert_eq!(
        native.layout.execution_mode,
        pinewasm_aot::ExecutionMode::Native
    );
    // Twenty million iterations exceed the per-bar budget in the module
    // exactly as they do in the interpreter.
    let error = StandaloneWasm::new(&wasm)
        .evaluate(&wire_v1(&alternating(2)))
        .expect_err("the loop exceeds the instruction budget");
    assert_eq!(
        error,
        "Indicator failure: per-bar instruction budget exceeded"
    );
}

#[test]
fn configured_standalone_wasm_heap_budget_is_baked_into_prepare_without_changing_the_default() {
    let program = compile_csil(concat!(
        "indicator(\"Heap budget\")\n",
        "var xs = array.new_float(0)\n",
        "if bar_index == 0\n",
        "    xs := array.new_float(100001, close)\n",
        "plot(close > open ? 1 : -1)\n",
    ));
    let options = StandaloneWasmOptions {
        signal_plot: Some(0),
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let input = wire_v1(&alternating(1));

    let default_wasm =
        compile_standalone_wasm_program(&program, &options).expect("default heap artifact");
    let mut default_standalone_wasm = StandaloneWasm::new(&default_wasm);
    assert_eq!(
        default_standalone_wasm
            .evaluate(&input)
            .expect_err("100k default must remain bounded"),
        "Indicator failure: native heap error"
    );

    let expanded_wasm = compile_standalone_wasm_program_with_config(
        &program,
        &options,
        &StandaloneWasmBuildConfig {
            max_collection_elements: 4_000_000,
            ..StandaloneWasmBuildConfig::default()
        },
    )
    .expect("expanded heap artifact");
    let mut expanded_standalone_wasm = StandaloneWasm::new(&expanded_wasm);
    assert_eq!(
        signals(
            &expanded_standalone_wasm
                .evaluate(&input)
                .expect("expanded budget evaluates")
        )
        .len(),
        1
    );
}

#[test]
fn a_non_posture_signal_lane_is_an_indicator_failure() {
    // `plot(close)` with close = 11.0: a designated lane that is not a
    // posture is the producer's contract violation, named per bar.
    let mut standalone_wasm =
        StandaloneWasm::new(&compile_standalone_wasm("indicator(\"NP\")\nplot(close)\n"));
    let error = standalone_wasm
        .evaluate(&wire_v1(&alternating(2)))
        .expect_err("a non-posture lane must fail serialization");
    assert!(
        error.starts_with(
            "Indicator failure: designated signal lane produced non-posture value at bar "
        ),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Statelessness, reset, and allocator discipline.

#[test]
fn a_resident_instance_is_byte_stable_across_a_thousand_evaluations() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    let input = wire_v1(&alternating(16));
    let first = standalone_wasm.evaluate(&input).expect("first evaluation");
    for i in 1..1000 {
        let again = standalone_wasm.evaluate(&input).expect("repeat evaluation");
        assert_eq!(again, first, "evaluation {i} must be byte-identical");
    }
    // A failure interleaved does not leak into the next success…
    standalone_wasm
        .evaluate("{broken")
        .expect_err("malformed input fails");
    assert_eq!(
        standalone_wasm
            .evaluate(&input)
            .expect("success after failure"),
        first,
        "an interleaved failure must not perturb the next result"
    );
    // …and a success clears the previous failure's error slot.
    assert_eq!(
        standalone_wasm.last_error(),
        None,
        "success must leave no stale error"
    );
}

#[test]
fn reset_restores_the_post_instantiation_observable_state() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));
    standalone_wasm
        .evaluate("{broken")
        .expect_err("malformed input fails");
    assert!(
        standalone_wasm.last_error().is_some(),
        "the failure left a diagnostic"
    );
    standalone_wasm.reset();
    assert_eq!(
        standalone_wasm.last_error(),
        None,
        "post-reset last_error() must be 0"
    );
}

#[test]
fn the_allocator_honors_the_spec_edge_cases() {
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(POSTURE));

    // alloc(0): a dangling well-aligned pointer that claims nothing.
    let zero_a = standalone_wasm.alloc(0);
    let zero_b = standalone_wasm.alloc(0);
    assert_ne!(zero_a, 0);
    assert_eq!(zero_a % 8, 0, "alloc(0) must be well-aligned");
    assert_eq!(zero_a, zero_b, "alloc(0) must not consume the arena");

    // dealloc(ptr, 0) is a no-op; dealloc of the top allocation rewinds.
    let p = standalone_wasm.alloc(16);
    standalone_wasm.dealloc(p, 0);
    let q = standalone_wasm.alloc(8);
    assert_eq!(q, p + 16, "dealloc(ptr, 0) must not have rewound");
    standalone_wasm.dealloc(q, 8);
    standalone_wasm.dealloc(p, 16);
    assert_eq!(
        standalone_wasm.alloc(0),
        p,
        "LIFO deallocation rewinds to the band start"
    );

    // Growth refusal (the module declares a 4096-page maximum): a clean
    // null, not a trap, and the instance stays usable.
    let refused = standalone_wasm.alloc(300_000_000);
    assert_eq!(refused, 0, "an unsatisfiable alloc must return null");
    let out = standalone_wasm
        .evaluate(&wire_v1(&alternating(4)))
        .expect("the instance survives a refused alloc");
    assert_eq!(signals(&out).len(), 4);
}

// ---------------------------------------------------------------------------
// 5. Determinism of compilation.

#[test]
fn compilation_is_byte_identical_and_sha_stable_across_runs() {
    let program = compile_csil(concat!(
        "indicator(\"Det\")\n",
        "thr = input.float(0.5, \"Threshold\")\n",
        "plot(close - open > thr ? 1 : -1, title = \"Long Short\")\n",
    ));
    let options = StandaloneWasmOptions {
        signal_plot: None,
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let first = compile_standalone_wasm_program(&program, &options).expect("first emit");
    let second = compile_standalone_wasm_program(&program, &options).expect("second emit");
    assert_eq!(
        first, second,
        "emission must be a pure function of the program"
    );
    assert_eq!(
        sha256(&first),
        sha256(&second),
        "and so must the artifact identity (the module hash)"
    );
}

// ---------------------------------------------------------------------------
// 6. Metadata.

#[test]
fn strategy_metadata_has_the_exact_spec_shape() {
    let wasm = compile_standalone_wasm(concat!(
        "indicator(\"Meta Demo\")\n",
        "plot(close > open ? 1 : -1, title = \"Long Short\")\n",
    ));
    let bytes = custom_section(&wasm, schema::MODULE_METADATA_SECTION)
        .expect("module_metadata section present");
    assert!(
        bytes.len() <= schema::MODULE_METADATA_MAX_BYTES,
        "must fit the 1 KiB cap, got {}",
        bytes.len()
    );
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    let object = doc.as_object().expect("an object");

    // Exactly the spec'd keys — no more, no fewer.
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = [
        "schema_version",
        "module_id",
        "display_name",
        "type",
        "abi_version",
        "signal",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected);

    assert_eq!(doc["schema_version"], 1);
    // `module_id` proposes a *catalog* id, which is kebab-case by the
    // catalog id rule in docs/aot.md §11 — unlike the underscore slugs
    // wire keys and `signal.source` use.
    assert_eq!(doc["module_id"], "meta-demo");
    assert_eq!(doc["display_name"], "Meta Demo");
    assert_eq!(doc["type"], "strategy");

    // Three contracts share the phrase "ABI version":
    // the standalone WebAssembly host contract in this metadata, the CSAW loader header, and
    // the artifact envelope's `aot.abiVersion` (which mirrors CSAW). Keep
    // them apart by name, and pin that they genuinely differ: the standalone WebAssembly
    // contract is frozen at v1 while CSAW moves with the memory layout.
    let standalone_wasm_abi_version = doc["abi_version"]
        .as_u64()
        .expect("standalone_wasm abi_version");
    let csaw_abi_version = u64::from(header::ABI_VERSION);
    assert_eq!(
        standalone_wasm_abi_version, 1,
        "the standalone WebAssembly host contract is v1"
    );
    assert_eq!(csaw_abi_version, 6, "the CSAW loader contract is v6");
    assert_ne!(standalone_wasm_abi_version, csaw_abi_version);

    // The signal object: exact shape, `source` slugged from the designated
    // plot's title by the same rule `parameter_schema` names use.
    let signal = doc["signal"].as_object().expect("signal object");
    let keys: BTreeSet<&str> = signal.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["source", "semantics", "price_source"]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(signal["source"], "long_short");
    assert_eq!(signal["semantics"], "position-events");
    assert_eq!(signal["price_source"], "close");
}

#[test]
fn an_untitled_designated_plot_omits_source() {
    let wasm = compile_standalone_wasm(POSTURE);
    let bytes = custom_section(&wasm, schema::MODULE_METADATA_SECTION).expect("section");
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    let signal = doc["signal"].as_object().expect("signal object");
    assert!(
        !signal.contains_key("source"),
        "no title, no source claim: {doc}"
    );
    assert_eq!(
        signal.len(),
        2,
        "semantics and price_source remain required"
    );
}

#[test]
fn an_indicator_type_module_has_no_signal_and_serializes_an_empty_series() {
    let wasm = compile_standalone_wasm_program(
        &compile_csil("indicator(\"I\")\nplot(close)\n"),
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("indicator-type emit");
    audit_standalone_wasm(&wasm).expect("passes the audit");

    let bytes = custom_section(&wasm, schema::MODULE_METADATA_SECTION).expect("section");
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    assert_eq!(doc["type"], "indicator");
    assert!(
        doc.get("signal").is_none(),
        "an indicator-type module must not carry a signal claim: {doc}"
    );

    // Its evaluations succeed with the empty result *document* — a valid
    // non-zero return, never an error, and never checked as a posture (this
    // program plots raw closes, which a strategy module would refuse).
    let mut standalone_wasm = StandaloneWasm::new(&wasm);
    let out = standalone_wasm
        .evaluate(&wire_v1(&alternating(4)))
        .expect("indicator evaluation succeeds");
    assert_eq!(out, b"{\"version\":1,\"signals\":[]}");
}

#[test]
fn non_standalone_wasm_artifacts_keep_their_bytes_free_of_standalone_wasm_metadata() {
    // The metadata gate: `abi_version`/`signal` exist only in standalone WebAssembly mode, so every
    // previously exported native/hosted artifact still re-emits byte-identically.
    let program = compile_csil(POSTURE);
    for wasm in [
        compile_program(&program).expect("native").wasm,
        compile_standalone_program(&program)
            .expect("standalone")
            .wasm,
    ] {
        let bytes = custom_section(&wasm, schema::MODULE_METADATA_SECTION).expect("section");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        let object = doc.as_object().expect("an object");
        assert!(
            !object.contains_key("abi_version") && !object.contains_key("signal"),
            "chart-interface metadata must be unchanged: {doc}"
        );
    }
    // And the writer the chart-interface paths share says the same thing directly.
    assert!(!schema::module_metadata_json(&program).contains("abi_version"));
}

#[test]
fn worst_case_standalone_wasm_metadata_stays_under_the_enforced_cap() {
    // A 100-char display name, a title that slugs to the 64-byte source
    // bound: still comfortably inside MODULE_METADATA_MAX_BYTES.
    let long_title = "Very Long Plot Title ".repeat(5);
    let source = format!(
        "indicator(\"{}\")\nplot(close > open ? 1 : -1, title = \"{}\")\n",
        "T".repeat(120),
        long_title.trim()
    );
    let program = compile_csil(&source);
    let json = schema::module_metadata_json_standalone_wasm(
        &program,
        StandaloneWasmModuleType::Strategy,
        Some(0),
        false,
        pinewasm_aot::StandaloneWasmSignalSemantics::PositionEvents,
    );
    assert!(
        json.len() <= schema::MODULE_METADATA_MAX_BYTES,
        "standalone WebAssembly metadata must fit the cap the host enforces, got {}",
        json.len()
    );
    let doc: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    let slug = doc["signal"]["source"].as_str().expect("source");
    assert!(
        (1..=64).contains(&slug.len()) && !slug.ends_with('_'),
        "source must respect the 64-byte bound cleanly, got {slug:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. The audit against hand-built modules no emitter path produces (the
// `standalone.rs` posture: bytes, never flags).

/// Knobs for a hand-built five-export module that passes `audit_standalone_wasm` when
/// every knob is at its default.
struct Surface<'a> {
    metadata: Option<&'a str>,
    drop_export: Option<&'a str>,
    extra_export: Option<&'a str>,
    alloc_gets_last_errors_type: bool,
    memory_max: Option<u64>,
}

const VALID_STRATEGY_METADATA: &str = concat!(
    "{\"schema_version\":1,\"module_id\":\"hand-built\",\"display_name\":\"Hand Built\",",
    "\"type\":\"strategy\",\"abi_version\":1,",
    "\"signal\":{\"semantics\":\"position-events\",\"price_source\":\"close\"}}"
);

impl Default for Surface<'_> {
    fn default() -> Self {
        Surface {
            metadata: Some(VALID_STRATEGY_METADATA),
            drop_export: None,
            extra_export: None,
            alloc_gets_last_errors_type: false,
            memory_max: Some(2),
        }
    }
}

fn surface_module(spec: &Surface) -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, CustomSection, ExportKind, ExportSection, Function, FunctionSection,
        Instruction as I, MemorySection, MemoryType, Module, TypeSection, ValType,
    };
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I64]); // 0: evaluate
    types.ty().function([ValType::I32], [ValType::I32]); // 1: alloc
    types.ty().function([ValType::I32, ValType::I32], []); // 2: dealloc
    types.ty().function([], [ValType::I64]); // 3: last_error
    types.ty().function([], []); // 4: reset

    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    let body = |instrs: &[I]| {
        let mut f = Function::new([]);
        for i in instrs {
            f.instruction(i);
        }
        f.instruction(&I::End);
        f
    };
    funcs.function(0);
    code.function(&body(&[I::I64Const(0)]));
    funcs.function(if spec.alloc_gets_last_errors_type {
        3
    } else {
        1
    });
    code.function(&body(&[if spec.alloc_gets_last_errors_type {
        I::I64Const(0)
    } else {
        I::I32Const(0)
    }]));
    funcs.function(2);
    code.function(&body(&[]));
    funcs.function(3);
    code.function(&body(&[I::I64Const(0)]));
    funcs.function(4);
    code.function(&body(&[]));

    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: spec.memory_max,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut exports = ExportSection::new();
    if spec.drop_export != Some("memory") {
        exports.export("memory", ExportKind::Memory, 0);
    }
    let names = [
        "evaluate_indicator_cabi",
        "alloc",
        "dealloc",
        "last_error",
        "reset",
    ];
    for (index, name) in names.iter().enumerate() {
        if spec.drop_export == Some(*name) {
            continue;
        }
        exports.export(name, ExportKind::Func, index as u32);
    }
    if let Some(name) = spec.extra_export {
        // Reuse `reset`'s body under the leaked name.
        exports.export(name, ExportKind::Func, 4);
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&funcs);
    module.section(&memory);
    module.section(&exports);
    module.section(&code);
    if let Some(metadata) = spec.metadata {
        module.section(&CustomSection {
            name: schema::MODULE_METADATA_SECTION.into(),
            data: metadata.as_bytes().into(),
        });
    }
    module.finish()
}

#[test]
fn the_hand_built_baseline_passes_so_each_knob_isolates_one_defect() {
    audit_standalone_wasm(&surface_module(&Surface::default())).expect("the baseline must pass");
}

#[test]
fn the_audit_refuses_broken_export_surfaces() {
    let surface_error = |spec: &Surface| match audit_standalone_wasm(&surface_module(spec)) {
        Err(AotError::StandaloneWasmExportSurface(reason)) => reason,
        other => panic!("expected StandaloneWasmExportSurface, got {other:?}"),
    };

    let reason = surface_error(&Surface {
        drop_export: Some("reset"),
        ..Surface::default()
    });
    assert!(
        reason.contains("reset"),
        "must name the missing export: {reason}"
    );

    // A leaked internal — `prepare` here — is an extra export, exactly the
    // violation the standalone WebAssembly exporter's prepare/run internalization exists for.
    let reason = surface_error(&Surface {
        extra_export: Some("prepare"),
        ..Surface::default()
    });
    assert!(
        reason.contains("prepare"),
        "must name the extra export: {reason}"
    );

    let reason = surface_error(&Surface {
        alloc_gets_last_errors_type: true,
        ..Surface::default()
    });
    assert!(
        reason.contains("alloc"),
        "must name the mis-signed export: {reason}"
    );

    let reason = surface_error(&Surface {
        memory_max: None,
        ..Surface::default()
    });
    assert!(
        reason.contains("maximum"),
        "must name the missing bound: {reason}"
    );

    let reason = surface_error(&Surface {
        drop_export: Some("memory"),
        ..Surface::default()
    });
    assert!(
        reason.contains("memory"),
        "must name the missing memory: {reason}"
    );
}

#[test]
fn the_audit_refuses_an_import_from_the_bytes() {
    use wasm_encoder::{EntityType, ImportSection, MemorySection, MemoryType, Module, TypeSection};
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut imports = ImportSection::new();
    imports.import("host", "ta", EntityType::Function(0));
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(2),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&memory);
    let error = audit_standalone_wasm(&module.finish()).expect_err("an import must be refused");
    let AotError::StandaloneWasmNotStandalone(reason) = &error else {
        panic!("expected StandaloneWasmNotStandalone, got {error:?}");
    };
    assert!(reason.contains("host.ta"), "must name the import: {reason}");
}

#[test]
fn the_audit_matches_the_hardened_host_feature_set() {
    // A module using SIMD validates under default features but will not
    // instantiate under the hardened host config, so the audit must refuse it.
    use wasm_encoder::{
        CodeSection, Function, FunctionSection, Instruction as I, MemorySection, MemoryType,
        Module, TypeSection,
    };
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(2),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&I::V128Const(0));
    f.instruction(&I::Drop);
    f.instruction(&I::End);
    code.function(&f);
    let mut module = Module::new();
    module.section(&types);
    module.section(&funcs);
    module.section(&memory);
    module.section(&code);
    let bytes = module.finish();

    assert!(
        wasmparser::Validator::new().validate_all(&bytes).is_ok(),
        "fixture must be valid under default features, or it tests nothing"
    );
    let error = audit_standalone_wasm(&bytes).expect_err("SIMD must fail the hardened validation");
    assert!(
        matches!(&error, AotError::StandaloneWasmExportSurface(reason) if reason.contains("not valid")),
        "got {error:?}"
    );
}

#[test]
fn the_audit_refuses_a_non_function_type_instead_of_panicking() {
    // The hardened feature set subtracts SIMD/relaxed-SIMD/threads/memory64/
    // multi-memory, but `WasmFeatures::default()` leaves `gc` on, so a module
    // whose type section carries a struct validates and reaches the audit's
    // type walk. A producer-side gate must answer with an `Err` for any input
    // — never abort the compiler.
    use wasm_encoder::{
        FieldType, MemorySection, MemoryType, Module, StorageType, TypeSection, ValType,
    };
    let mut types = TypeSection::new();
    types.ty().struct_([FieldType {
        element_type: StorageType::Val(ValType::I32),
        mutable: false,
    }]);
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(2),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut module = Module::new();
    module.section(&types);
    module.section(&memory);
    let bytes = module.finish();

    assert!(
        wasmparser::Validator::new().validate_all(&bytes).is_ok(),
        "fixture must be valid under default features, or it tests nothing"
    );
    let error = audit_standalone_wasm(&bytes).expect_err("a non-function type must be refused");
    assert!(
        matches!(&error, AotError::StandaloneWasmExportSurface(reason)
            if reason.contains("type 0") && reason.contains("not a function type")),
        "must name the offending type index, got {error:?}"
    );
}

#[test]
fn the_audit_refuses_nonconformant_metadata() {
    let metadata_error =
        |metadata: Option<&str>| match audit_standalone_wasm(&surface_module(&Surface {
            metadata,
            ..Surface::default()
        })) {
            Err(AotError::StandaloneWasmMetadata(reason)) => reason,
            other => panic!("expected StandaloneWasmMetadata for {metadata:?}, got {other:?}"),
        };

    let reason = metadata_error(None);
    assert!(reason.contains("missing"), "{reason}");

    let reason = metadata_error(Some("not json"));
    assert!(reason.contains("JSON"), "{reason}");

    // No abi_version: not loadable by a host that follows the contract.
    let reason = metadata_error(Some(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"indicator\"}",
    ));
    assert!(reason.contains("abi_version"), "{reason}");

    // The strategy ⇔ signal biconditional, both refusal directions.
    let reason = metadata_error(Some(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"strategy\",\"abi_version\":1}",
    ));
    assert!(reason.contains("signal"), "{reason}");
    let reason = metadata_error(Some(concat!(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"indicator\",\"abi_version\":1,",
        "\"signal\":{\"semantics\":\"position-events\",\"price_source\":\"close\"}}"
    )));
    assert!(reason.contains("signal"), "{reason}");

    // Unknown keys inside signal are unvalidated claims.
    let reason = metadata_error(Some(concat!(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"strategy\",\"abi_version\":1,",
        "\"signal\":{\"semantics\":\"position-events\",\"price_source\":\"close\",\"alternation\":\"strict\"}}"
    )));
    assert!(reason.contains("alternation"), "{reason}");

    // Closed enums: the sole legal values at v1.1.
    let reason = metadata_error(Some(concat!(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"strategy\",\"abi_version\":1,",
        "\"signal\":{\"semantics\":\"per-bar\",\"price_source\":\"close\"}}"
    )));
    assert!(reason.contains("semantics"), "{reason}");
    let reason = metadata_error(Some(
        "{\"schema_version\":1,\"module_id\":\"x\",\"display_name\":\"X\",\"type\":\"oscillator\",\"abi_version\":1}",
    ));
    assert!(
        reason.contains("type") || reason.contains("oscillator"),
        "{reason}"
    );

    // The 1 KiB cap is checked before anything is believed about the JSON.
    let oversized = format!(
        "{{\"schema_version\":1,\"pad\":\"{}\"}}",
        "a".repeat(schema::MODULE_METADATA_MAX_BYTES)
    );
    let reason = metadata_error(Some(&oversized));
    assert!(reason.contains("bytes"), "{reason}");
}

// ---------------------------------------------------------------------------
// 8. Runtime-side constants that must match the workspace definitions. The runtime
// crate hardcodes these offsets; this is the workspace-side handshake.

#[test]
fn the_header_offsets_the_blob_hardcodes_are_the_real_ones() {
    assert_eq!(header::OFF_BARS, 0x08);
    assert_eq!(header::OFF_BUILTIN_BASE, 0x10);
    assert_eq!(header::OFF_PLOTS_BASE, 0x18);
    assert_eq!(header::OFF_SCRATCH_BASE, 0x40);
    assert_eq!(status::INVALID_TA_LENGTH, 12);
    assert_eq!(status::SECURITY_TIMEFRAME_UNAVAILABLE, 13);
    assert_eq!(status::SECURITY_LENGTH_OUT_OF_RANGE, 14);
    assert_eq!(status::INT_FRACTION_RANGE, 15);
    assert_eq!(
        header::SIZE,
        0x60,
        "the input table base the runtime writes"
    );
}

// ---------------------------------------------------------------------------
// Interoperability artifact writer. Not a test of the crate: it emits a demo
// standalone WebAssembly artifact for manual testing with a compatible host.
// Run explicitly:  cargo test -p pinewasm_aot --test standalone_wasm -- --ignored write_demo
#[test]
#[ignore = "manual interoperability artifact writer, not a test"]
fn write_demo_artifact_for_conformance() {
    let wasm = compile_standalone_wasm(
        "indicator(\"standalone WebAssembly Demo\")\nplot(close > open ? 1 : close < open ? -1 : 0)\n",
    );
    let out = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("standalone-wasm-demo.wasm");
    std::fs::write(&out, &wasm).expect("write demo artifact");
    eprintln!("wrote {} ({} bytes)", out.display(), wasm.len());
}

/// A ta-backed signal, so the sequence below drives the linked engine rather
/// than a pure per-bar expression: `close` crossing its own 4-bar ema.
const TA_POSTURE: &str = "indicator(\"T\")\nema = ta.ema(close, 4)\nplot(close > ema ? 1 : -1)\n";

/// The standalone WebAssembly output for `source` over `bars`, and the same program's
/// edge-detected plot lane through the standalone tier — an independent
/// entry point and fill path for the same semantics, which the check compares
/// against, as `signals_match_the_edge_detected_standalone_plot_lane` does.
fn standalone_wasm_against_standalone(
    standalone_wasm: &mut StandaloneWasm,
    source: &str,
    bars: &[Bar],
) -> (SignalSeries, SignalSeries) {
    let got = signals(
        &standalone_wasm
            .evaluate(&wire_v1(bars))
            .expect("evaluate succeeds"),
    );
    let standalone = compile_standalone_program(&compile_csil(source)).expect("standalone");
    let plot = run_standalone_plot0(&standalone.wasm, bars);
    (got, edge_detect(&plot, bars))
}

fn assert_same_signals(got: &[Signal], expected: &[Signal], what: &str) {
    assert_eq!(got.len(), expected.len(), "{what}: signal count");
    for (got, expected) in got.iter().zip(expected) {
        assert_eq!(got.0, expected.0, "{what}: time");
        assert_eq!(got.1, expected.1, "{what}: direction");
        assert_eq!(
            got.2.to_bits(),
            expected.2.to_bits(),
            "{what}: price must be bit-exact"
        );
    }
}

#[test]
fn a_ta_backed_artifact_evaluates_through_the_whole_standalone_wasm_sequence() {
    // Every other executing test here uses a program with no `ta.*` call, so
    // until this focused test, none of them exercised the engine under the standalone WebAssembly
    // entry point at all — and that entry point is where the combination is new:
    // `standalone_wasm_arena_reloc`'s bound has to cover the engine region, and the
    // declaration table has to survive `memory.init` alongside the key table.
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(TA_POSTURE));
    let bars = alternating(40);
    let (got, expected) =
        standalone_wasm_against_standalone(&mut standalone_wasm, TA_POSTURE, &bars);

    // The threshold matters as much as the comparison. An artifact whose
    // engine never advances plots a constant, a constant still edge-detects
    // into ONE signal on the first bar, and BOTH paths here would agree on
    // it — so equality alone passes on a completely broken engine, which is
    // how the first draft of this test failed to fail. `close` alternates
    // 11/9 against a 4-bar ema, so a working engine crosses on most bars.
    assert!(
        expected.len() >= 20,
        "expected a crossing on most of 40 bars, got {}: the engine is not \
         advancing in either path",
        expected.len()
    );
    assert_same_signals(&got, &expected, "ta posture");
}

#[test]
fn a_resident_instance_resets_the_ta_engine_between_evaluations() {
    // The resident-instance statelessness rule, aimed at this state. A TA site
    // carries a ring and a recursion across bars; if `run` did not
    // re-initialize the engine, the second evaluation would warm up from the
    // first one's history and produce different signals. `POSTURE` cannot
    // catch that, because it has no state to carry.
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(TA_POSTURE));
    let bars = alternating(40);
    let (first, expected) =
        standalone_wasm_against_standalone(&mut standalone_wasm, TA_POSTURE, &bars);
    assert!(expected.len() >= 20, "the engine is not advancing");
    assert_same_signals(&first, &expected, "first evaluation");

    let input = wire_v1(&bars);
    for i in 1..8 {
        let again = signals(&standalone_wasm.evaluate(&input).expect("repeat evaluation"));
        assert_same_signals(&again, &expected, &format!("evaluation {i}"));
    }

    // A different feed in between must not leak either: the engine is
    // re-derived from the declaration table on every run, not carried.
    standalone_wasm
        .evaluate(&wire_v1(&alternating(24)))
        .expect("interleaved evaluation");
    let after = signals(
        &standalone_wasm
            .evaluate(&input)
            .expect("evaluation after another feed"),
    );
    assert_same_signals(&after, &expected, "after a different feed");
}

#[test]
fn the_ta_declaration_table_survives_the_standalone_wasm_arena() {
    // The key table is `memory.init`ed per call because a previous call's
    // input arena can land in the gap below the lane floor. The declaration
    // table now shares that gap, so it needs the same treatment — and five
    // sites make the table wide enough that a one-table-only
    // re-materialization would be visible.
    // Five live sites. The posture is the fast/slow pair; the other three
    // are summed into a term that appears on both sides, so they advance
    // every bar without dictating the signal.
    let source = "indicator(\"M\")\n\
                  fast = ta.ema(close, 3)\n\
                  slow = ta.rma(close, 9)\n\
                  live = ta.stdev(close, 4) + ta.highest(high, 6) + ta.wma(close, 7)\n\
                  plot(fast + live > slow + live ? 1 : -1)\n";
    let mut standalone_wasm = StandaloneWasm::new(&compile_standalone_wasm(source));
    let bars = alternating(48);
    let (first, expected) = standalone_wasm_against_standalone(&mut standalone_wasm, source, &bars);
    assert!(
        expected.len() >= 5,
        "five sites produced {} signal(s): the table did not reach the engine",
        expected.len()
    );
    assert_same_signals(&first, &expected, "first evaluation");

    // The first call runs with the arena at its initial base (which overlaps
    // the tables' slot); every later call runs with it relocated. Both must
    // agree, which is exactly what a clobbered table would break.
    let input = wire_v1(&bars);
    for i in 1..5 {
        let again = signals(&standalone_wasm.evaluate(&input).expect("repeat evaluation"));
        assert_same_signals(&again, &expected, &format!("evaluation {i}"));
    }
}

#[test]
fn ta_length_sweeps_reuse_one_standalone_wasm_instance_and_match_changed_defaults() {
    let bars: Vec<_> = (0..120)
        .map(|i| {
            let close = 100.0 + f64::from((i * 17) % 31);
            bar(
                1_700_000_000_000 + i64::from(i) * 60_000,
                close - 0.5,
                close,
            )
        })
        .collect();
    for expression in [
        "ta.sma(close, n)",
        "ta.ema(close, n)",
        "math.sum(close, n) / n",
        "ta.change(close, n)",
    ] {
        let source = format!("indicator(\"length sweep\")\nn = input.int(14, \"Length\", minval=1, maxval=100000)\nx = {expression}\nplot(close > x ? 1 : -1)\n");
        let wasm = compile_standalone_wasm(&source);
        let mut resident = StandaloneWasm::new(&wasm);
        let default = resident.evaluate(&wire_v1(&bars)).unwrap();
        for length in [50, 2, 14, 50] {
            let actual = resident
                .evaluate(&wire(
                    &format!("\"version\":1,\"params\":{{\"length\":{length}}},"),
                    &bars,
                ))
                .unwrap();
            let expected_wasm = compile_standalone_wasm(
                &source.replace("input.int(14", &format!("input.int({length}")),
            );
            let expected = StandaloneWasm::new(&expected_wasm)
                .evaluate(&wire_v1(&bars))
                .unwrap();
            assert_eq!(
                signals(&actual),
                signals(&expected),
                "{expression}, length={length}"
            );
            if expression == "ta.sma(close, n)" && length == 50 {
                assert_ne!(
                    signals(&actual),
                    signals(&default),
                    "the swept dimension must be live"
                );
            }
        }
        assert_eq!(
            resident.evaluate(&wire_v1(&bars)).unwrap(),
            default,
            "defaults restored after sweep"
        );
        for length in [0, -1, 100_001] {
            let error = resident
                .evaluate(&wire(
                    &format!("\"version\":1,\"params\":{{\"length\":{length}}},"),
                    &bars,
                ))
                .unwrap_err();
            assert!(error.contains("TA input length out of range"), "{error}");
            assert_eq!(
                resident.evaluate(&wire_v1(&bars)).unwrap(),
                default,
                "recover after invalid trial"
            );
        }
    }
}

#[test]
fn input_backed_history_does_not_reserve_maximum_rings_for_unrelated_slots() {
    // An indicator with hundreds of temporaries and two adjustable
    // averages. A uniform 131072-cell ring for all slots exceeds the 256 MiB
    // module limit even on one bar. This synthetic case is self-contained and
    // keeps the same pressure on the layout planner.
    let declarations: String = (0..600)
        .map(|i| format!("v{i} = close + {i}.0\n"))
        .collect();
    let prefix = "indicator(\"bounded slot history\", max_bars_back=31)\n\
                  n = input.int(5, \"Length\", minval=1, maxval=100000)\n\
                  lag = input.int(3, \"Lag\", minval=0, maxval=31)\n\
                  var float count = 0.0\ncount += 1.0\n\
                  var float held = na\nif bar_index % 7 == 0\n    held := close\n";
    let tail = "mean = ta.sma(close, n)\n\
                change = ta.change(close, n)\n\
                delayed = v599[lag]\n\
                plot(close > mean and change > 0 and delayed > 0 and count > 0 and not na(held) ? 1 : -1)\n";
    let source = format!("{prefix}{declarations}{tail}");
    let program = compile_csil(&source);
    assert!(program.series_slots.len() > 600);
    let wasm = compile_standalone_wasm(&source);
    let mut resident = StandaloneWasm::new(&wasm);
    let bars: Vec<_> = (0..257)
        .map(|i| {
            let close = 100.0 + f64::from((i * 17) % 31);
            bar(
                1_700_000_000_000 + i64::from(i) * 60_000,
                close - 0.5,
                close,
            )
        })
        .collect();
    resident
        .evaluate(&wire_v1(&bars[..1]))
        .expect("one bar fits");
    // Cross ring boundaries, change both dynamic histories, and revisit a
    // prior trial. Literal references have no input-binding layout at all.
    for (length, lag) in [(5, 3), (33, 31), (129, 0), (100_000, 3), (5, 3)] {
        let params = format!("\"params\":{{\"length\":{length},\"lag\":{lag}}},");
        let actual = resident
            .evaluate(&wire(&params, &bars))
            .expect("trial fits");
        let reference = format!("{prefix}v599 = close + 599.0\n{tail}")
            .replace(
                "input.int(5, \"Length\", minval=1, maxval=100000)",
                &length.to_string(),
            )
            .replace(
                "input.int(3, \"Lag\", minval=0, maxval=31)",
                &lag.to_string(),
            );
        let expected = StandaloneWasm::new(&compile_standalone_wasm(&reference))
            .evaluate(&wire_v1(&bars))
            .expect("literal reference");
        assert_eq!(
            signals(&actual),
            signals(&expected),
            "length={length}, lag={lag}"
        );
        assert!(resident.memory.data(&resident.store).len() < 16 * 1024 * 1024);
    }
    let invalid = resident.evaluate(&wire("\"params\":{\"length\":100001},", &bars));
    assert!(invalid
        .unwrap_err()
        .contains("TA input length out of range"));
    resident
        .evaluate(&wire_v1(&bars))
        .expect("valid trial after error");
}

#[test]
fn unobserved_standalone_wasm_slot_writes_keep_history_windows_and_persistent_branches() {
    let source = concat!(
        "indicator('Observed slot stores', max_bars_back=7)\n",
        "var float count = 0.0\ncount += 1.0\n",
        "var float held = na\nif bar_index % 3 == 0\n    held := close\n",
        "current = close * 2.0\n",
        "past = current[bar_index % 4]\nmean = ta.sma(current, 3)\n",
        "ready = count > 3 and not na(past) and not na(held)\n",
        "plot(ready and current > mean ? 1 : -1)\n",
    );
    let program = compile_csil(source);
    let ordinary = compile_standalone_program(&program).unwrap();
    let standalone_wasm = compile_standalone_wasm(source);
    let mut resident = StandaloneWasm::new(&standalone_wasm);
    let bars = alternating(41);
    for slice in [&bars[..1], &bars[..17], &bars[..41], &bars[..17]] {
        let expected = edge_detect(&run_standalone_plot0(&ordinary.wasm, slice), slice);
        let actual = signals(&resident.evaluate(&wire_v1(slice)).unwrap());
        assert_eq!(actual, expected);
    }
}

// ---------------------------------------------------------------------------
// Signal slice: only what can reach the designated plot is compiled.

/// A deterministic random walk, so signals depend on real price history
/// rather than on the alternating fixture's two states.
fn walk(n: usize) -> Vec<Bar> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut price = 100.0_f64;
    (0..n)
        .map(|i| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let step = ((state >> 33) % 2001) as f64 / 1000.0 - 1.0;
            let open = price;
            price = (price + step).max(1.0);
            bar(1_700_000_000_000 + i as i64 * 60_000, open, price)
        })
        .collect()
}

fn slice_options(plot: usize) -> StandaloneWasmOptions {
    StandaloneWasmOptions {
        signal_plot: Some(plot),
        module_type: StandaloneWasmModuleType::Strategy,
    }
}

fn compile_standalone_wasm_sliced(
    source: &str,
    plot: usize,
    signal_slice: bool,
) -> Result<Vec<u8>, AotError> {
    compile_standalone_wasm_program_with_config(
        &compile_csil(source),
        &slice_options(plot),
        &StandaloneWasmBuildConfig {
            signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
            signal_slice,
            ..StandaloneWasmBuildConfig::default()
        },
    )
}

fn posture_of(wasm: &[u8], bars: &[Bar]) -> SignalSeries {
    let mut module = StandaloneWasm::new(wasm);
    posture_signals(&module.evaluate(&wire_v1(bars)).expect("evaluate"))
}

#[test]
fn display_code_outside_the_signal_no_longer_refuses_the_script() {
    // The signal is the last plot. Everything above it is chart output the
    // importless tier cannot run: a higher-timeframe request, an array of
    // colors, a formatted table cell, and a label rebuilt on the last bar.
    let display = concat!(
        "indicator(\"SLICE\")\n",
        "daily = request.security(syminfo.tickerid, \"D\", close)\n",
        "plot(daily, \"Daily\")\n",
        "shades = array.new_color()\n",
        "array.push(shades, color.red)\n",
        "array.push(shades, color.green)\n",
        "barcolor(array.get(shades, close > open ? 1 : 0))\n",
        "var panel = table.new(position.top_right, 1, 1)\n",
        "if barstate.islast\n",
        "    table.cell(panel, 0, 0, str.format(\"{0,number,#.##}\", close))\n",
        "    var label note = na\n",
        "    label.delete(note)\n",
        "    note := label.new(bar_index, close, \"Distance: \" + str.tostring(math.round(close, 1)) + \"%\")\n",
    );
    let signal =
        "fast = ta.ema(close, 3)\nplot(close > fast ? 1 : close < fast ? -1 : 0, \"Signal\")\n";
    let source = format!("{display}{signal}");
    let bare = format!("indicator(\"SLICE\")\n{signal}");

    let unsliced = compile_standalone_wasm_sliced(&source, 1, false)
        .expect_err("the whole program needs a host");
    assert!(
        matches!(unsliced, AotError::StandaloneWasmNotStandalone(_)),
        "without the slice the display code refuses the script, got {unsliced:?}"
    );

    let wasm =
        compile_standalone_wasm_sliced(&source, 1, true).expect("only the signal is compiled");
    audit_standalone_wasm(&wasm).expect("the sliced module is standalone");
    let bars = walk(300);
    let got = posture_of(&wasm, &bars);
    let expected = posture_of(
        &compile_standalone_wasm_sliced(&bare, 0, true).expect("bare signal"),
        &bars,
    );
    assert!(expected.len() > 20, "the fixture must actually trade");
    assert_same_signals(&got, &expected, "display code must not change the signal");
}

#[test]
fn a_host_value_that_reaches_the_signal_still_refuses() {
    // A request for another symbol's feed, as a filter on the signal: the
    // slice keeps it and the script is refused for what really blocks it.
    let source = concat!(
        "indicator(\"SLICE-LIVE\")\n",
        "daily = request.security(\"NASDAQ:AAPL\", \"D\", close)\n",
        "label.new(bar_index, close, str.tostring(close))\n",
        "plot(close > daily ? 1 : -1, \"Signal\")\n",
    );
    let error =
        compile_standalone_wasm_sliced(source, 0, true).expect_err("the request feeds the signal");
    assert!(
        matches!(error, AotError::StandaloneWasmNotStandalone(_)),
        "a signal-relevant host value keeps the refusal, got {error:?}"
    );
}

/// A Pine `int / int` is arithmetic like any other: one that only display
/// code reads is erased, and a branch that decides nothing else goes with it.
#[test]
fn a_display_only_integer_division_is_erased_with_its_branch() {
    // `month()` needs the host, so the script builds only if the branch that
    // holds the display-only quotient is skipped along with its condition.
    let behind_a_branch = concat!(
        "indicator(\"DIV\")\n",
        "var int n = 0\n",
        "if month(time, \"UTC\") > 6\n",
        "    n := bar_index / 3\n",
        "plot(n, \"N\")\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    // A quotient kept in a shape the importless tier does not lower, read
    // only by a display plot: erased, so it cannot refuse the script.
    let straight_line = concat!(
        "indicator(\"DIV\")\n",
        "n = input.int(7, \"N\")\n",
        "k = n / 2 + bar_index % 3\n",
        "plot(k, \"K\")\n",
        "plot(str.length(str.tostring(k)), \"Digits\")\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    let bars = walk(300);
    for (what, source, plot) in [
        ("behind a display-only branch", behind_a_branch, 1),
        ("in straight-line display code", straight_line, 2),
    ] {
        let sliced = compile_standalone_wasm_sliced(source, plot, true)
            .unwrap_or_else(|e| panic!("{what}: sliced: {e:?}"));
        let expected = vm_posture(source, plot, &bars, "1");
        assert!(expected.len() > 5, "{what}: the fixture must trade");
        assert_same_signals(&posture_of(&sliced, &bars), &expected, what);
    }
}

#[test]
fn the_slice_never_changes_a_signal_the_whole_program_also_produces() {
    // Each script compiles both ways; the display code around the signal
    // shares storage with it in the ways the slice has to get right.
    let scripts: [(&str, &str, usize); 8] = [
        (
            "an integer field copied from another object controls dynamic history",
            concat!(
                "indicator(\"FIELDS\")\n",
                "type Pair\n",
                "    float price\n",
                "    int length\n",
                "Pair a = Pair.new(close, input.int(8, \"Length\"))\n",
                "Pair b = Pair.new(open, 3)\n",
                "a.length := b.length\n",
                "int k = a.length + 1\n",
                "plot(a.price, \"Price\")\n",
                "plot(close > close[k] ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "an array mutated through a second name",
            concat!(
                "indicator(\"ALIAS\")\n",
                "var seen = array.new_float()\n",
                "other = seen\n",
                "if close > open\n",
                "    array.push(other, close)\n",
                "plot(close, \"Price\")\n",
                "plot(array.size(seen) % 2 == 0 ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "an array held in an object field",
            concat!(
                "indicator(\"FIELD\")\n",
                "type Bag\n",
                "    array<float> items\n",
                "    float total\n",
                "var Bag bag = Bag.new(array.new_float(), 0.0)\n",
                "held = bag.items\n",
                "if close > open\n",
                "    array.push(held, close)\n",
                "    bag.total := bag.total + close\n",
                "if array.size(held) > 6\n",
                "    array.shift(held)\n",
                "plot(bag.total, \"Total\")\n",
                "plot(array.sum(bag.items) / array.size(bag.items) > close ? -1 : 1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "an object field written beside display code and read by the signal",
            concat!(
                "indicator(\"OBJECT\")\n",
                "type State\n",
                "    int side\n",
                "    float mark\n",
                "var State state = State.new(0, na)\n",
                "if ta.crossover(close, ta.sma(close, 5))\n",
                "    state.side := 1\n",
                "    state.mark := high\n",
                "if ta.crossunder(close, ta.sma(close, 5))\n",
                "    state.side := -1\n",
                "    state.mark := low\n",
                "plot(state.mark, \"Mark\")\n",
                "plot(state.side, \"Signal\")\n",
            ),
            1,
        ),
        (
            "var counters and history shared with display plots",
            concat!(
                "indicator(\"VAR\")\n",
                "var float ups = 0.0\n",
                "var float bars = 0.0\n",
                "var float peak = na\n",
                "bars += 1\n",
                "ups += close > open ? 1 : 0\n",
                "peak := na(peak) ? high : math.max(peak, high)\n",
                "ratio = ups / bars\n",
                "plot(peak, \"Peak\")\n",
                "plot(ratio, \"Ratio\")\n",
                "plot(ups[3] > ups[6] + 1 ? 1 : ups[3] < ups[6] + 1 ? -1 : 0, \"Signal\")\n",
            ),
            2,
        ),
        (
            "a display loop next to a signal loop",
            concat!(
                "indicator(\"LOOPS\")\n",
                "var levels = array.new_float()\n",
                "var marks = array.new_float()\n",
                "array.push(levels, close)\n",
                "if array.size(levels) > 8\n",
                "    array.shift(levels)\n",
                "float total = 0.0\n",
                "float seen = 0.0\n",
                "while seen < 3\n",
                "    total += array.sum(levels)\n",
                "    array.push(marks, total)\n",
                "    seen += 1\n",
                "if array.size(marks) > 50\n",
                "    array.clear(marks)\n",
                "plot(array.sum(marks), \"Marks\")\n",
                "plot(close > total / 3 / array.size(levels) ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a posture carried in a var beside alerts and bar colors",
            concat!(
                "indicator(\"POSTURE\")\n",
                "fast = ta.ema(close, 3)\n",
                "slow = ta.ema(close, 8)\n",
                "enter = ta.crossover(fast, slow)\n",
                "leave = ta.crossunder(fast, slow)\n",
                "var int posture = 0\n",
                "if enter\n",
                "    posture := 1\n",
                "else if leave\n",
                "    posture := 0\n",
                "barcolor(posture == 1 ? color.green : color.gray)\n",
                "alertcondition(enter, \"Enter\", \"enter\")\n",
                "plot(fast, \"Fast\", color = posture == 1 ? color.green : color.red)\n",
                "plot(posture, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a copied object that must not alias the original",
            concat!(
                "indicator(\"COPY\")\n",
                "type Box\n",
                "    float n\n",
                "var Box a = Box.new(0.0)\n",
                "b = Box.copy(a)\n",
                "b.n := 100.0\n",
                "a.n += close > open ? 1 : -1\n",
                "plot(b.n, \"Copy\")\n",
                "plot(a.n > 0 ? 1 : a.n < 0 ? -1 : 0, \"Signal\")\n",
            ),
            1,
        ),
    ];
    let bars = walk(400);
    for (what, source, plot) in scripts {
        let sliced = compile_standalone_wasm_sliced(source, plot, true)
            .unwrap_or_else(|e| panic!("{what}: sliced: {e:?}"));
        let whole = compile_standalone_wasm_sliced(source, plot, false)
            .unwrap_or_else(|e| panic!("{what}: whole program: {e:?}"));
        assert_ne!(
            whole, sliced,
            "{what}: the slice must have erased something"
        );
        // The interpreter gives the expected signals for both artifacts.
        let expected = vm_posture(source, plot, &bars, "1");
        assert!(
            expected.len() > 5,
            "{what}: the fixture must actually trade"
        );
        assert_same_signals(&posture_of(&sliced, &bars), &expected, what);
        assert_same_signals(&posture_of(&whole, &bars), &expected, what);
    }
}

#[test]
fn a_runtime_error_outside_the_signal_is_never_erased() {
    // `runtime.error` stops the run, which the signal does observe. standalone WebAssembly has
    // no native lowering for it, so the script stays refused: erasing the
    // call would turn a run that stops into one that keeps trading.
    let source = concat!(
        "indicator(\"ERROR\")\n",
        "if bar_index == 2\n",
        "    runtime.error(\"stop here\")\n",
        "plot(close, \"Price\")\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    for signal_slice in [false, true] {
        let error = compile_standalone_wasm_sliced(source, 1, signal_slice)
            .expect_err("runtime.error needs a host");
        assert!(
            matches!(error, AotError::StandaloneWasmNotStandalone(_)),
            "slice={signal_slice}: got {error:?}"
        );
    }
}

#[test]
fn a_scalar_read_from_an_object_does_not_tie_it_to_a_drawing() {
    // The label takes a float out of the settings object the signal also
    // reads. A float is not a handle, so the label and the settings object
    // stay in separate alias classes and the label is still erased.
    let source = concat!(
        "indicator(\"ALIAS\", overlay = true)\n",
        "type Cfg\n",
        "    float level\n",
        "    float scale\n",
        "var Cfg cfg = Cfg.new(0.25, 2.0)\n",
        "if barstate.islast\n",
        "    label.new(bar_index, cfg.level, str.tostring(close))\n",
        "plot(close - open > cfg.level * cfg.scale ? 1 : -1, \"Signal\")\n",
    );
    let whole =
        compile_standalone_wasm_sliced(source, 0, false).expect_err("the label needs a host");
    assert!(
        matches!(whole, AotError::StandaloneWasmNotStandalone(_)),
        "got {whole:?}"
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("the label is erased");
    audit_standalone_wasm(&wasm).expect("importless");
    let bars = walk(300);
    let expected = vm_posture(source, 0, &bars, "1");
    assert!(expected.len() > 5, "the fixture must actually trade");
    assert_same_signals(
        &posture_of(&wasm, &bars),
        &expected,
        "scalar field into a label",
    );
}

#[test]
fn a_handle_read_from_an_object_still_aliases_it() {
    // Two types use field 0: one holds an array, the other a float. The
    // array read out of the first is a handle, so a mutation through it,
    // made only beside display code, must survive the slice.
    let source = concat!(
        "indicator(\"ALIAS-HANDLE\")\n",
        "type Bag\n",
        "    array<float> items\n",
        "type Cfg\n",
        "    float level\n",
        "var Bag bag = Bag.new(array.new_float())\n",
        "var Cfg cfg = Cfg.new(1.5)\n",
        "held = bag.items\n",
        "if close > open\n",
        "    array.push(held, close)\n",
        "if array.size(held) > 6\n",
        "    array.shift(held)\n",
        "plot(cfg.level + array.sum(held), \"Display\")\n",
        "plot(array.sum(bag.items) / array.size(bag.items) > close * cfg.level / 1.5 ? -1 : 1, \"Signal\")\n",
    );
    let sliced = compile_standalone_wasm_sliced(source, 1, true).expect("sliced");
    let whole = compile_standalone_wasm_sliced(source, 1, false).expect("whole");
    assert_ne!(whole, sliced, "the slice must have erased the display plot");
    let bars = walk(400);
    let expected = vm_posture(source, 1, &bars, "1");
    assert!(expected.len() > 5, "the fixture must actually trade");
    assert_same_signals(&posture_of(&sliced, &bars), &expected, "sliced");
    assert_same_signals(&posture_of(&whole, &bars), &expected, "whole");
}

#[test]
fn erased_arithmetic_is_not_billed_per_placeholder_by_a_fuel_group() {
    // Each erased add becomes `Pop Pop PushNa`. A fuel window must bill the
    // free placeholders nothing: the whole program fits the per-bar budget
    // at this loop count, and so must the sliced one. The signal reads the
    // counter, so the loop runs in the sliced module too and only its
    // display arithmetic is erased; a loop nothing live reads is skipped
    // whole.
    let terms = vec!["i"; 24].join(" + ");
    let source = format!(
        concat!(
            "indicator(\"SLICE-FUEL\")\n",
            "n = input.float(0.0, \"N\")\n",
            "float i = 0.0\n",
            "float d = 0.0\n",
            "while i < n\n",
            "    d := {terms}\n",
            "    i += 1.0\n",
            "plot(d, \"Display\")\n",
            "plot(i >= n and close > open ? 1 : -1, \"Signal\")\n",
        ),
        terms = terms
    );
    let bars = walk(2);
    let input = wire("\"params\":{\"n\":1000000},", &bars);
    let whole = compile_standalone_wasm_sliced(&source, 1, false).expect("whole");
    let sliced = compile_standalone_wasm_sliced(&source, 1, true).expect("sliced");
    assert_ne!(
        whole, sliced,
        "the slice must have erased the display loop body"
    );
    let expected = StandaloneWasm::new(&whole)
        .evaluate(&input)
        .expect("the whole program fits the budget");
    let got = StandaloneWasm::new(&sliced)
        .evaluate(&input)
        .expect("erased code costs no more than it did");
    assert_same_signals(
        &posture_signals(&got),
        &posture_signals(&expected),
        "sliced under a budget the whole program fits",
    );
}

#[test]
fn erased_display_code_never_costs_more_budget_than_it_did() {
    // A display-only loop body heavy enough that charging every placeholder
    // would push the sliced run over the instruction budget the whole
    // program fits in. The signal reads the counter, so the loop itself
    // runs in the sliced module; a loop nothing live reads is skipped whole.
    //
    // The budget is the cumulative one, one billion for these 400 bars. The
    // whole program fits it with room to spare: it still does at 110,000
    // iterations. With its placeholders charged, the sliced run already
    // stops at 92,000.
    let source = concat!(
        "indicator(\"BUDGET\")\n",
        "float acc = 0.0\n",
        // A float counter: this tier refuses an integer loop counter whose
        // range it cannot bound.
        "float i = 0.0\n",
        "while i < 100000\n",
        "    acc := acc + close * open + high * low\n",
        "    i += 1\n",
        "plot(acc, \"Display\")\n",
        "plot(i > 0 and close > open ? 1 : -1, \"Signal\")\n",
    );
    let bars = walk(400);
    let expected = posture_of(
        &compile_standalone_wasm_sliced(source, 1, false).expect("whole"),
        &bars,
    );
    let sliced = posture_of(
        &compile_standalone_wasm_sliced(source, 1, true).expect("sliced"),
        &bars,
    );
    assert_same_signals(
        &sliced,
        &expected,
        "an erased loop body stays inside the budget",
    );
}

#[test]
fn an_input_bound_length_in_display_code_does_not_switch_the_slice_off() {
    // The validator wants every input-bound TA length to appear exactly
    // once, so those instructions stay in place; the display code around
    // them is still erased.
    let source = concat!(
        "indicator(\"BOUND\")\n",
        "length = input.int(5, \"Length\")\n",
        "lookback = input.int(3, \"Lookback\")\n",
        "average = ta.sma(close, length)\n",
        "drift = ta.change(close, lookback)\n",
        "plot(average, \"Average\")\n",
        "plot(drift, \"Drift\")\n",
        "var panel = table.new(position.top_right, 1, 1)\n",
        "if barstate.islast\n",
        "    table.cell(panel, 0, 0, str.format(\"{0,number,#.##}\", average + drift))\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    let unsliced =
        compile_standalone_wasm_sliced(source, 2, false).expect_err("the table text needs a host");
    assert!(
        matches!(unsliced, AotError::StandaloneWasmNotStandalone(_)),
        "got {unsliced:?}"
    );
    let wasm = compile_standalone_wasm_sliced(source, 2, true).expect("the slice still applies");
    let bars = walk(120);
    let bare = "indicator(\"BOUND\")\nplot(close > open ? 1 : -1, \"Signal\")\n";
    let expected = posture_of(
        &compile_standalone_wasm_sliced(bare, 0, true).expect("bare"),
        &bars,
    );
    assert_same_signals(
        &posture_of(&wasm, &bars),
        &expected,
        "input-bound display code",
    );
}

#[test]
fn a_branch_that_only_decides_display_code_is_skipped() {
    // A statistics table rolled once a month: the roll is decided by
    // calendar and timeframe calls the importless tier cannot run, and its
    // branches only ever feed the table. Nothing they decide can reach the
    // signal, so the conditions and the calls behind them go.
    let display = concat!(
        "indicator(\"BRANCH\")\n",
        "var monthly = array.new_float()\n",
        "var float base = 100.0\n",
        "var bool intramonth = timeframe.in_seconds() <= timeframe.in_seconds(\"1M\")\n",
        "newMonth = ta.change(time(\"1M\")) != 0\n",
        "skipped = na(time[1]) or intramonth ? 0 : (year(time, \"UTC\") - 1970) * 12 + month(time, \"UTC\")\n",
        "base *= 1.0 + ta.roc(close, 1) / 100.0\n",
        "if newMonth\n",
        "    if skipped > 1\n",
        "        for _ = 2 to skipped\n",
        "            array.push(monthly, 0.0)\n",
        "    array.push(monthly, base - 100.0)\n",
        "    if array.size(monthly) > 12\n",
        "        array.shift(monthly)\n",
        "    base := 100.0\n",
        "float spread = na\n",
        "if barstate.islast\n",
        "    spread := array.stdev(monthly)\n",
        "plot(spread, \"Spread\")\n",
    );
    let signal =
        "fast = ta.ema(close, 3)\nplot(close > fast ? 1 : close < fast ? -1 : 0, \"Signal\")\n";
    let source = format!("{display}{signal}");
    let bare = format!("indicator(\"BRANCH\")\n{signal}");

    let unsliced = compile_standalone_wasm_sliced(&source, 1, false)
        .expect_err("the whole program needs a host");
    assert!(
        matches!(unsliced, AotError::StandaloneWasmNotStandalone(_)),
        "got {unsliced:?}"
    );
    let wasm = compile_standalone_wasm_sliced(&source, 1, true)
        .expect("no display branch reaches the signal");
    audit_standalone_wasm(&wasm).expect("the sliced module is standalone");
    let bars = walk(300);
    let got = posture_of(&wasm, &bars);
    let expected = posture_of(
        &compile_standalone_wasm_sliced(&bare, 0, true).expect("bare signal"),
        &bars,
    );
    assert!(expected.len() > 20, "the fixture must actually trade");
    assert_same_signals(
        &got,
        &expected,
        "display branches must not change the signal",
    );
    // The interpreter runs the display code and plots the same signal.
    assert_same_signals(
        &got,
        &vm_posture(&source, 1, &bars, "1"),
        "the interpreter run of the whole script",
    );
}

#[test]
fn a_branch_that_decides_the_signal_keeps_its_condition() {
    // Calendar reads in the exchange timezone, which the request does not
    // carry, but now what they decide reaches the plot: by a store the
    // signal reads, by a loop bound, by a break, and by the right side of a
    // lazy `and`. Each script stays refused for the call that really stands
    // between it and its signal.
    let scripts = [
        (
            "a store the signal reads",
            concat!(
                "indicator(\"LIVE-STORE\")\n",
                "var int posture = 0\n",
                "if year > 2000\n",
                "    posture := close > open ? 1 : -1\n",
                "plot(close, \"Price\")\n",
                "plot(posture, \"Signal\")\n",
            ),
        ),
        (
            "a loop bound",
            concat!(
                "indicator(\"LIVE-BOUND\")\n",
                "int ups = 0\n",
                "for i = 0 to dayofweek > 1 ? 4 : 2\n",
                "    ups += close[i] > open[i] ? 1 : 0\n",
                "plot(close, \"Price\")\n",
                "plot(ups > 2 ? 1 : -1, \"Signal\")\n",
            ),
        ),
        (
            "a break",
            concat!(
                "indicator(\"LIVE-BREAK\")\n",
                "int run = 0\n",
                "for i = 0 to 10\n",
                "    if month(time[i]) != month(time)\n",
                "        break\n",
                "    run += 1\n",
                "plot(close, \"Price\")\n",
                "plot(run > 5 ? 1 : -1, \"Signal\")\n",
            ),
        ),
        (
            "the right side of a lazy and",
            concat!(
                "indicator(\"LIVE-LAZY\")\n",
                "wide = close > open and dayofweek > 1\n",
                "plot(close, \"Price\")\n",
                "plot(wide ? 1 : -1, \"Signal\")\n",
            ),
        ),
        (
            "an enclosing branch of the plot's own input",
            concat!(
                "indicator(\"LIVE-NEST\")\n",
                "var float level = na\n",
                "if dayofweek(time) != dayofweek.sunday\n",
                "    if close > open\n",
                "        level := close\n",
                "plot(close, \"Price\")\n",
                "plot(close > nz(level, close) ? 1 : -1, \"Signal\")\n",
            ),
        ),
    ];
    for (what, source) in scripts {
        let error = compile_standalone_wasm_sliced(source, 1, true)
            .expect_err("the host call decides something the signal reads");
        assert!(
            matches!(error, AotError::StandaloneWasmNotStandalone(_)),
            "{what}: got {error:?}"
        );
    }
}

#[test]
fn skipping_display_branches_never_changes_the_signal() {
    // Each script compiles both ways. The display code shares conditions,
    // loops, counters and storage with the signal in the ways skipping a
    // branch has to get right.
    let scripts: [(&str, &str, usize); 10] = [
        (
            "a display store and a signal store under one condition",
            concat!(
                "indicator(\"CD-A\")\n",
                "var int posture = 0\n",
                "var float mark = na\n",
                "up = close > open\n",
                "if up\n",
                "    mark := high\n",
                "if up and close > close[1]\n",
                "    posture := 1\n",
                "else if not up\n",
                "    posture := -1\n",
                "plot(mark, \"Mark\")\n",
                "plot(posture, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a loop that feeds both a display total and a signal count",
            concat!(
                "indicator(\"CD-B\")\n",
                "int n = 0\n",
                "float acc = 0.0\n",
                "for i = 0 to 9\n",
                "    acc += close[i]\n",
                "    if close[i] > open[i]\n",
                "        n += 1\n",
                "    else if close[i] < low[i] + 0.1\n",
                "        acc -= 1.0\n",
                "plot(acc, \"Acc\")\n",
                "plot(n > 5 ? 1 : n < 5 ? -1 : 0, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a display while loop with a break",
            concat!(
                "indicator(\"CD-C\")\n",
                "int depth = 0\n",
                "while depth < 50\n",
                "    if close[depth] < open[depth]\n",
                "        break\n",
                "    depth += 1\n",
                "plot(depth, \"Run\")\n",
                "plot(close > open ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a break that decides how far a signal loop runs",
            concat!(
                "indicator(\"CD-D\")\n",
                "int run = 0\n",
                "float reach = 0.0\n",
                "for i = 0 to 20\n",
                "    if close[i] < open[i]\n",
                "        break\n",
                "    run += 1\n",
                "    if high[i] > reach\n",
                "        reach := high[i]\n",
                "plot(reach, \"Reach\")\n",
                "plot(run >= 2 ? 1 : run == 0 ? -1 : 0, \"Signal\")\n",
            ),
            1,
        ),
        (
            "display branches nested inside a signal branch",
            concat!(
                "indicator(\"CD-E\")\n",
                "var int posture = 0\n",
                "var float lastHigh = na\n",
                "var int flips = 0\n",
                "average = ta.sma(close, 5)\n",
                "if ta.crossover(close, average)\n",
                "    posture := 1\n",
                "    if high > nz(lastHigh, high - 1)\n",
                "        lastHigh := high\n",
                "        flips += 1\n",
                "if ta.crossunder(close, average)\n",
                "    posture := -1\n",
                "    if flips > 3\n",
                "        flips := 0\n",
                "plot(lastHigh, \"High\")\n",
                "plot(flips, \"Flips\")\n",
                "plot(posture, \"Signal\")\n",
            ),
            2,
        ),
        (
            "a signal branch nested inside a display branch",
            concat!(
                "indicator(\"CD-F\")\n",
                "var int posture = 0\n",
                "var int quiet = 0\n",
                "if close > open\n",
                "    quiet := 0\n",
                "    if close > close[2]\n",
                "        posture := 1\n",
                "else\n",
                "    quiet += 1\n",
                "    if quiet > 2\n",
                "        posture := -1\n",
                "        quiet := 0\n",
                "plot(posture, \"Signal\")\n",
                "plot(close, \"Price\")\n",
            ),
            0,
        ),
        (
            "a lazy boolean whose right side only feeds display",
            concat!(
                "indicator(\"CD-G\")\n",
                "wide = high - low > ta.atr(5)\n",
                "loud = wide and volume > ta.sma(volume, 5)\n",
                "calm = not wide or close == open\n",
                "plot(loud ? 1 : calm ? -1 : 0, \"Loud\")\n",
                "plot(wide ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a switch with display arms beside a signal switch",
            concat!(
                "indicator(\"CD-H\")\n",
                "phase = bar_index % 4\n",
                "shade = switch phase\n",
                "    0 => close\n",
                "    1 => open\n",
                "    2 => high\n",
                "    => low\n",
                "side = switch\n",
                "    close > open and phase < 2 => 1\n",
                "    close < open => -1\n",
                "    => 0\n",
                "plot(shade, \"Shade\")\n",
                "plot(side, \"Signal\")\n",
            ),
            1,
        ),
        (
            "an array filled inside a branch and read by the signal",
            concat!(
                "indicator(\"CD-I\")\n",
                "var seen = array.new_float()\n",
                "var spare = array.new_float()\n",
                "if close > open\n",
                "    array.push(seen, close)\n",
                "    if array.size(seen) > 6\n",
                "        array.shift(seen)\n",
                "else\n",
                "    array.push(spare, close)\n",
                "    if array.size(spare) > 3\n",
                "        array.clear(spare)\n",
                "plot(array.size(spare), \"Spare\")\n",
                "plot(array.size(seen) % 2 == 0 ? 1 : -1, \"Signal\")\n",
            ),
            1,
        ),
        (
            "a var declared inside a display branch and a function used by both",
            concat!(
                "indicator(\"CD-J\")\n",
                "streak(float source, int length) =>\n",
                "    int count = 0\n",
                "    for i = 0 to length - 1\n",
                "        if source[i] > source[i + 1]\n",
                "            count += 1\n",
                "        else\n",
                "            break\n",
                "    count\n",
                "if close > open\n",
                "    var int greens = 0\n",
                "    greens += streak(high, 6)\n",
                "    if greens > 40\n",
                "        greens := 0\n",
                "plot(streak(volume, 4), \"Volume Streak\")\n",
                "plot(streak(close, 5) >= 2 ? 1 : streak(close, 5) == 0 ? -1 : 0, \"Signal\")\n",
            ),
            1,
        ),
    ];
    let bars = walk(400);
    for (what, source, plot) in scripts {
        let whole = compile_standalone_wasm_sliced(source, plot, false)
            .unwrap_or_else(|e| panic!("{what}: whole program: {e:?}"));
        let sliced = compile_standalone_wasm_sliced(source, plot, true)
            .unwrap_or_else(|e| panic!("{what}: sliced: {e:?}"));
        assert_ne!(
            whole, sliced,
            "{what}: the slice must have erased something"
        );
        // The interpreter gives the expected signals for both artifacts.
        let expected = vm_posture(source, plot, &bars, "1");
        assert!(
            expected.len() > 5,
            "{what}: the fixture must actually trade"
        );
        assert_same_signals(&posture_of(&sliced, &bars), &expected, what);
        assert_same_signals(&posture_of(&whole, &bars), &expected, what);
    }
}

#[test]
fn a_display_loop_is_skipped_whole() {
    // A display-only loop too heavy for one bar's budget stops the whole
    // program on its first bar. The signal never reads it, so the sliced
    // module does not run it at all.
    let source = concat!(
        "indicator(\"HEAVY\")\n",
        "float acc = 0.0\n",
        "for i = 0 to 20000000\n",
        "    acc := acc + close * open\n",
        "plot(acc, \"Display\")\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    let bare = "indicator(\"HEAVY\")\nplot(close > open ? 1 : -1, \"Signal\")\n";
    let bars = walk(40);
    let whole = compile_standalone_wasm_sliced(source, 1, false).expect("whole");
    let error = StandaloneWasm::new(&whole)
        .evaluate(&wire_v1(&bars))
        .expect_err("the display loop exhausts the first bar's budget");
    assert_eq!(
        error,
        format!("{INDICATOR_FAILURE}per-bar instruction budget exceeded")
    );
    let expected = posture_of(
        &compile_standalone_wasm_sliced(bare, 0, true).expect("bare"),
        &bars,
    );
    assert_same_signals(
        &posture_of(
            &compile_standalone_wasm_sliced(source, 1, true).expect("sliced"),
            &bars,
        ),
        &expected,
        "a skipped display loop",
    );
}

/// The names the refusal of `plot` of `source` carries, compiled the way
/// [`compile_standalone_wasm_sliced`] compiles it; empty when the program
/// is eligible.
fn host_features_of(source: &str, plot: usize) -> Vec<String> {
    standalone_eligibility(
        &compile_csil(source),
        &slice_options(plot),
        &StandaloneWasmBuildConfig {
            signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
            signal_slice: true,
            ..StandaloneWasmBuildConfig::default()
        },
    )
    .err()
    .map(|refusal| refusal.host_features().to_vec())
    .unwrap_or_default()
}

#[test]
fn a_refusal_names_the_calls_that_need_a_host() {
    // "program would import host.vm" names a mechanism. The calls behind it
    // are what a reader can find in the script and change.
    let source = concat!(
        "indicator(\"NAMES\")\n",
        "var monthly = array.new_float()\n",
        "if ta.change(time(\"1M\")) != 0\n",
        "    array.push(monthly, close)\n",
        "spread = array.median(monthly)\n",
        "label.new(bar_index, close, str.tostring(year(time, \"UTC\")))\n",
        "plot(str.length(ticker.standard()) > 3 ? spread : na, \"Spread\")\n",
        "plot(close > open ? 1 : -1, \"Signal\")\n",
    );
    let error =
        compile_standalone_wasm_sliced(source, 0, true).expect_err("the statistics need a host");
    assert!(
        matches!(error, AotError::StandaloneWasmNotStandalone(_)),
        "got {error:?}"
    );
    // The array could live in the module: `array.new_float`, `array.push`
    // and the rest are host sites only because `array.median` is, so they
    // are not named. The label and its text are chart output and are gone.
    // `ticker.standard()` reads `syminfo.tickerid` for its default
    // argument; the script did not write that, so only the call is named.
    assert_eq!(
        host_features_of(source, 0),
        ["array.median", "str.length", "ticker.standard", "time"]
    );
    // Written out, the argument is the script's own and is named too.
    let written = concat!(
        "indicator(\"WRITTEN\")\n",
        "plot(str.length(ticker.standard(syminfo.tickerid)) > 3 ? 1 : -1, \"Signal\")\n",
    );
    assert_eq!(
        host_features_of(written, 0),
        ["str.length", "syminfo.tickerid", "ticker.standard"]
    );
    // Nothing the signal reads needs a host.
    assert!(host_features_of(source, 1).is_empty());
    compile_standalone_wasm_sliced(source, 1, true).expect("the signal compiles");

    // A text input is named by the call that declared it, not by the input
    // type those calls have in common. An `options=` list would make the first
    // three standalone; the last two take no such argument.
    for (call, name) in [
        (
            "input.string(\"D\", \"Text\")",
            "input.string with no options list",
        ),
        (
            "input.timeframe(\"D\", \"Text\")",
            "input.timeframe with no options list",
        ),
        (
            "input.session(\"D\", \"Text\")",
            "input.session with no options list",
        ),
        ("input.symbol(\"D\", \"Text\")", "input.symbol"),
        ("input.text_area(\"D\", \"Text\")", "input.text_area"),
    ] {
        let source = format!(
            "indicator(\"INPUTS\")\ntext = {call}\nplot(text == \"D\" and close > open ? 1 : -1, \"Signal\")\n"
        );
        assert_eq!(
            host_features_of(&source, 0),
            [name, "string comparison (== / !=)"],
            "{call}"
        );
    }
    for call in [
        "input.string(\"D\", \"Text\", options = [\"D\", \"W\"])",
        "input.timeframe(\"D\", \"Text\", options = [\"D\", \"W\"])",
        "input.session(\"D\", \"Text\", options = [\"D\", \"W\"])",
    ] {
        let source = format!(
            "indicator(\"INPUTS\")\ntext = {call}\nplot(text == \"D\" and close > open ? 1 : -1, \"Signal\")\n"
        );
        assert!(host_features_of(&source, 0).is_empty(), "{call}");
    }
}

#[test]
fn the_run_budget_grows_with_the_bars_evaluated() {
    // The cumulative budget is one billion instructions or a million per
    // bar, whichever is larger: a script inside the per-bar budget on every
    // bar is not refused for the length of its dataset.
    let wasm = compile_standalone_wasm_sliced(
        "indicator(\"BUDGET\")\nplot(close > open ? 1 : -1, \"Signal\")\n",
        0,
        true,
    )
    .expect("compiles");
    for (bars, expected) in [
        (1usize, 1_000_000_000u64),
        (1_000, 1_000_000_000),
        (1_001, 1_001_000_000),
        (2_500, 2_500_000_000),
    ] {
        let mut module = StandaloneWasm::new(&wasm);
        module.evaluate(&wire_v1(&walk(bars))).expect("evaluate");
        let mem = module.memory.data(&module.store);
        let cell =
            (read_u32(mem, header::OFF_SCRATCH_BASE) + header::SCRATCH_OFF_BUDGET_PER_RUN) as usize;
        let budget = u64::from_le_bytes(mem[cell..cell + 8].try_into().unwrap());
        assert_eq!(budget, expected, "{bars} bars");
    }
}

// ---------------------------------------------------------------------------
// Same-symbol `request.security`: the module resamples the bars itself.

/// `walk` with an explicit bar spacing and optional holes, so higher-timeframe
/// buckets hold varying numbers of chart bars.
fn spaced_walk(n: usize, spacing_ms: i64, holes: bool) -> Vec<Bar> {
    let mut out = Vec::with_capacity(n);
    let mut time = 1_700_000_000_000_i64;
    for (i, mut b) in walk(n).into_iter().enumerate() {
        // Every 13th bar is followed by a three-bar hole (a closed session).
        if holes && i % 13 == 12 {
            time += 3 * spacing_ms;
        }
        b.time = time;
        b.volume = Some(100.0 + (i % 7) as f64);
        out.push(b);
        time += spacing_ms;
    }
    out
}

/// The interpreter's posture events for plot `plot` over `bars`.
fn vm_posture(source: &str, plot: usize, bars: &[Bar], timeframe: &str) -> SignalSeries {
    vm_posture_with(source, plot, bars, timeframe, &[])
}

/// [`vm_posture`] with string input overrides, keyed by input id.
fn vm_posture_with(
    source: &str,
    plot: usize,
    bars: &[Bar],
    timeframe: &str,
    inputs: &[(u32, &str)],
) -> SignalSeries {
    use pinewasm_mir::Value;
    use pinewasm_vm::{run, MarketData, RunOptions};
    let data = MarketData {
        open: bars.iter().map(|b| b.open).collect(),
        high: bars.iter().map(|b| b.high).collect(),
        low: bars.iter().map(|b| b.low).collect(),
        close: bars.iter().map(|b| b.close).collect(),
        volume: bars.iter().map(|b| b.volume.unwrap_or(f64::NAN)).collect(),
        time: bars.iter().map(|b| b.time).collect(),
        timeframe: timeframe.to_string(),
        ..MarketData::default()
    };
    let mut options = RunOptions::default();
    for (input, value) in inputs {
        options.inputs.insert(
            *input,
            pinewasm_hir::InputValue::String((*value).to_string()),
        );
    }
    let outcome = run(&compile_csil(source), &data, &options);
    assert!(
        outcome.errors.is_empty(),
        "interpreter run: {:?}",
        outcome.errors
    );
    let mut out = Vec::new();
    let mut previous = 0_i64;
    for (i, value) in outcome.plots[plot].values.iter().enumerate() {
        let current = match value {
            Value::Float(x) => *x as i64,
            Value::Int(x) => *x,
            _ => 0,
        };
        if current != previous {
            out.push((bars[i].time, current, bars[i].close));
        }
        previous = current;
    }
    out
}

#[test]
fn a_resident_standalone_indicator_collects_and_reuses_its_heap() {
    // A 30,000-element temporary every bar (240 KB), beside a `var` array and
    // a `var` object whose array field grows above the temporaries and is
    // replaced every seven bars. One resident instance evaluates slices of
    // different lengths in turn, as the loader does, and each evaluation
    // starts its heap over. Every slice must match the interpreter. The
    // instance's memory must stay below what the longest slice's temporaries
    // alone would need if no collection gave memory back.
    let source = concat!(
        "indicator('resident heap')\n",
        "type Basket\n    array<float> items\n    float w\n",
        "var keep = array.new_float()\n",
        "var Basket b = Basket.new(array.new_float(), 0.0)\n",
        "tmp = array.new_float(30000, close)\n",
        "array.push(keep, close)\n",
        "array.push(b.items, open)\n",
        "if bar_index % 7 == 0\n    b := Basket.new(array.new_float(3, close), close)\n",
        "s = array.sum(keep) / array.size(keep) + array.sum(b.items) / array.size(b.items) \
         + array.sum(tmp) / 30000.0\n",
        "plot(s > s[1] ? 1 : -1)\n",
    );
    let wasm = compile_standalone_wasm(source);
    let mut resident = StandaloneWasm::new(&wasm);
    let bars = alternating(400);
    for n in [1usize, 50, 400, 17, 400, 233] {
        let slice = &bars[..n];
        let actual = signals(&resident.evaluate(&wire_v1(slice)).unwrap());
        assert_eq!(
            actual,
            vm_posture(source, 0, slice, "1"),
            "slice of {n} bars"
        );
    }
    // The bound is below the memory needed to retain every temporary in the
    // longest slice. Persistent buffers can sit above the latest temporary,
    // preventing reclamation below the highest live record. Reuse must still
    // keep the resident instance within the asserted memory bound.
    let memory = resident.memory.data(&resident.store).len();
    assert!(memory < 64_000_000, "{memory} bytes");
}

#[test]
fn a_same_symbol_request_security_matches_the_reference_interpreter() {
    const MINUTE: i64 = 60_000;
    // (chart timeframe, bar spacing, requested timeframe)
    let frames = [
        ("60", 60 * MINUTE, "240"),
        ("60", 60 * MINUTE, "D"),
        ("240", 240 * MINUTE, "D"),
        ("15", 15 * MINUTE, "60"),
        ("60", 60 * MINUTE, "60"),
        ("D", 1440 * MINUTE, "3D"),
    ];
    let sources = [
        "close", "open", "high", "low", "volume", "hl2", "hlc3", "ohlc4", "hlcc4",
    ];
    let mut compared = 0;
    for (chart, spacing, requested) in frames {
        for holes in [false, true] {
            let bars = spaced_walk(360, spacing, holes);
            for (index, series) in sources.iter().enumerate() {
                for (gaps, lookahead) in
                    [(false, false), (false, true), (true, false), (true, true)]
                {
                    // The full source list on the default flags, and every
                    // flag combination on a rotating source, keeps the
                    // matrix broad without compiling 400 modules.
                    if (gaps || lookahead) && index % 3 != (compared % 3) {
                        continue;
                    }
                    let source = format!(
                        concat!(
                            "indicator(\"SEC\")\n",
                            "h = request.security(syminfo.tickerid, \"{tf}\", {series}, ",
                            "gaps = barmerge.{gaps}, lookahead = barmerge.{lookahead})\n",
                            "reference = {series} == volume ? volume : close\n",
                            "plot(na(h) ? 0 : reference > h ? 1 : -1, \"Signal\")\n",
                        ),
                        tf = requested,
                        series = series,
                        gaps = if gaps { "gaps_on" } else { "gaps_off" },
                        lookahead = if lookahead {
                            "lookahead_on"
                        } else {
                            "lookahead_off"
                        },
                    );
                    let what = format!(
                        "{series} {requested} on {chart} holes={holes} gaps={gaps} lookahead={lookahead}"
                    );
                    let wasm = compile_standalone_wasm_sliced(&source, 0, true)
                        .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                    audit_standalone_wasm(&wasm).expect("importless");
                    let expected = vm_posture(&source, 0, &bars, chart);
                    assert_same_signals(&posture_of(&wasm, &bars), &expected, &what);
                    compared += 1;
                }
            }
        }
    }
    assert!(
        compared > 100,
        "the matrix must stay broad, compared {compared}"
    );
}

#[test]
fn a_request_security_prefix_evaluation_resamples_only_the_prefix() {
    // `fraction` evaluates a prefix of the bars; the last bucket then closes
    // on the prefix's last bar, exactly as a run over those bars alone.
    let source = concat!(
        "indicator(\"SEC\")\n",
        "h = request.security(syminfo.tickerid, \"240\", close)\n",
        "plot(na(h) ? 0 : close > h ? 1 : -1, \"Signal\")\n",
    );
    let bars = spaced_walk(200, 3_600_000, false);
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    let mut module = StandaloneWasm::new(&wasm);
    let prefix = &bars[..(bars.len() as f64 * 0.37) as usize];
    let got = posture_signals(
        &module
            .evaluate(&wire("\"fraction\":0.37,", &bars))
            .expect("prefix evaluates"),
    );
    assert_same_signals(&got, &vm_posture(source, 0, prefix, "60"), "prefix");
}

#[test]
fn a_request_below_the_bar_spacing_resamples_into_its_own_buckets() {
    // The module is not told the chart timeframe, so it skips that timeframe
    // check. Each bar is then its own bucket
    // and the request reads the bar itself.
    let source = concat!(
        "indicator(\"SEC\")\n",
        "h = request.security(syminfo.tickerid, \"15\", open)\n",
        "plot(na(h) ? 0 : close > h ? 1 : -1, \"Signal\")\n",
    );
    let plain = "indicator(\"SEC\")\nplot(close > open ? 1 : -1, \"Signal\")\n";
    let bars = spaced_walk(80, 3_600_000, false);
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    assert_same_signals(
        &posture_of(&wasm, &bars),
        &vm_posture(plain, 0, &bars, "60"),
        "a 15 minute request on hourly bars",
    );
}

#[test]
fn requests_the_module_cannot_resample_still_refuse() {
    for (what, source) in [
        (
            "another symbol",
            "indicator(\"SEC\")\nh = request.security(\"NASDAQ:AAPL\", \"D\", close)\nplot(close > h ? 1 : -1)\n",
        ),
        (
            "a calendar week",
            "indicator(\"SEC\")\nh = request.security(syminfo.tickerid, \"W\", close)\nplot(close > h ? 1 : -1)\n",
        ),
        (
            "an expression over a calendar week",
            "indicator(\"SEC\")\nh = request.security(syminfo.tickerid, \"W\", ta.ema(close, 5))\nplot(close > h ? 1 : -1)\n",
        ),
    ] {
        let error = compile_standalone_wasm_sliced(source, 0, true).expect_err(what);
        assert!(
            matches!(error, AotError::StandaloneWasmNotStandalone(_)),
            "{what}: got {error:?}"
        );
    }
}

#[test]
fn a_request_security_timeframe_chosen_by_an_input_follows_the_selected_option() {
    let source = concat!(
        "indicator(\"SEC-INPUT\")\n",
        "tf = input.string(\"240\", \"Trend Timeframe\", options=[\"60\", \"240\", \"D\", \"\", \"W\"])\n",
        "h = request.security(syminfo.tickerid, tf, close)\n",
        "plot(na(h) ? 0 : close > h ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true)
        .expect("an input-selected timeframe compiles");
    audit_standalone_wasm(&wasm).expect("importless");
    let bars = spaced_walk(400, 3_600_000, true);
    let mut module = StandaloneWasm::new(&wasm);
    let mut evaluate =
        |params: &str| module.evaluate(&wire(&format!("\"params\":{{{params}}},"), &bars));

    for (index, option) in ["60", "240", "D", ""].into_iter().enumerate() {
        let got = posture_signals(
            &evaluate(&format!("\"trend_timeframe\":{index}")).expect("a servable option"),
        );
        let expected = vm_posture_with(source, 0, &bars, "60", &[(0, option)]);
        assert_same_signals(&got, &expected, &format!("option {option:?}"));
    }
    let default = posture_signals(&evaluate("").expect("the declared default"));
    assert_same_signals(
        &default,
        &vm_posture(source, 0, &bars, "60"),
        "no params selects the default option",
    );
    assert_same_signals(
        &posture_signals(&evaluate("\"trend_timeframe\":99").expect("fallback")),
        &default,
        "an out-of-range index falls back to the default option",
    );
    assert_eq!(
        evaluate("\"trend_timeframe\":4").expect_err("a calendar week"),
        "Indicator failure: this request.security timeframe is not available in standalone WebAssembly"
    );
}

#[test]
fn a_free_text_timeframe_input_runs_its_declared_default() {
    // `input.timeframe` without options has no wire lane, so the module is
    // pinned to the default the script declares.
    let source = concat!(
        "indicator(\"SEC-DEFAULT\")\n",
        "tf = input.timeframe(\"240\", \"Trend Timeframe\")\n",
        "h = request.security(syminfo.tickerid, tf, hl2, lookahead = barmerge.lookahead_on)\n",
        "plot(na(h) ? 0 : close > h ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles at the default");
    let bars = spaced_walk(300, 3_600_000, false);
    assert_same_signals(
        &posture_of(&wasm, &bars),
        &vm_posture(source, 0, &bars, "60"),
        "the declared default timeframe",
    );
}

#[test]
fn a_request_for_the_charts_own_period_reads_each_bar() {
    let source = concat!(
        "indicator(\"SEC-CHART\")\n",
        "h = request.security(syminfo.tickerid, timeframe.period, close)\n",
        "g = request.security(syminfo.tickerid, \"\", open, gaps = barmerge.gaps_on)\n",
        "plot(na(g) or na(h) ? 0 : h > g ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("the chart period compiles");
    for (chart, spacing) in [("60", 3_600_000), ("D", 86_400_000)] {
        let bars = spaced_walk(200, spacing, true);
        assert_same_signals(
            &posture_of(&wasm, &bars),
            &vm_posture(source, 0, &bars, chart),
            &format!("chart period on {chart}"),
        );
    }
}

#[test]
fn a_timeframe_input_with_no_servable_option_still_refuses() {
    let source = concat!(
        "indicator(\"SEC-CALENDAR\")\n",
        "tf = input.string(\"W\", \"Trend Timeframe\", options=[\"W\", \"M\"])\n",
        "h = request.security(syminfo.tickerid, tf, close)\n",
        "plot(close > h ? 1 : -1, \"Signal\")\n",
    );
    let error =
        compile_standalone_wasm_sliced(source, 0, true).expect_err("only calendar timeframes");
    assert!(
        matches!(error, AotError::StandaloneWasmNotStandalone(_)),
        "got {error:?}"
    );
}

#[test]
fn a_compound_request_security_expression_matches_the_reference_interpreter() {
    // Each expression is evaluated once per higher-timeframe bucket by the
    // module's own secondary context: its own ta state and its own history.
    let expressions = [
        "ta.ema(close, 5)",
        "ta.sma(hl2, 3)",
        "ta.change(close, 2)",
        "close[1]",
        "ta.sma(close, 3)[2]",
        "(high - low) / close * 100",
        "-ta.change(ohlc4, 1)",
        "ta.ema(ta.sma(close, 3), 4)",
        "ta.sma(close, 4) - ta.ema(close, 9) + close[3]",
        "close % 3 + high / low",
        "ta.sma(volume, 3) * 2",
    ];
    let bars = spaced_walk(420, 3_600_000, true);
    let mut compared = 0;
    for expression in expressions {
        for (gaps, lookahead) in [(false, false), (true, false), (false, true), (true, true)] {
            let source = format!(
                concat!(
                    "indicator(\"SEC-EXPR\")\n",
                    "h = request.security(syminfo.tickerid, \"240\", {expression}, ",
                    "gaps = barmerge.{gaps}, lookahead = barmerge.{lookahead})\n",
                    "base = request.security(syminfo.tickerid, \"240\", {expression})\n",
                    "plot(na(h) ? 0 : h > nz(base[5]) ? 1 : -1, \"Signal\")\n",
                ),
                expression = expression,
                gaps = if gaps { "gaps_on" } else { "gaps_off" },
                lookahead = if lookahead {
                    "lookahead_on"
                } else {
                    "lookahead_off"
                },
            );
            let what = format!("{expression} gaps={gaps} lookahead={lookahead}");
            let wasm = compile_standalone_wasm_sliced(&source, 0, true)
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
            audit_standalone_wasm(&wasm).expect("importless");
            let expected = vm_posture(&source, 0, &bars, "60");
            assert!(
                expected.len() > 3,
                "{what}: the fixture must actually trade"
            );
            assert_same_signals(&posture_of(&wasm, &bars), &expected, &what);
            compared += 1;
        }
    }
    assert!(
        compared >= 12,
        "the matrix must stay broad, compared {compared}"
    );
}

#[test]
fn a_compound_request_reads_its_lengths_and_timeframe_from_inputs() {
    let source = concat!(
        "indicator(\"SEC-EXPR-INPUT\")\n",
        "tf = input.string(\"240\", \"Trend Timeframe\", options=[\"120\", \"240\", \"D\"])\n",
        "length = input.int(4, \"Trend Length\", minval = 1)\n",
        "trend = request.security(syminfo.tickerid, tf, ta.sma(close, length))\n",
        "plot(na(trend) ? 0 : close > trend ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    audit_standalone_wasm(&wasm).expect("importless");
    let bars = spaced_walk(420, 3_600_000, false);
    let mut module = StandaloneWasm::new(&wasm);
    for (option_index, option) in ["120", "240", "D"].into_iter().enumerate() {
        for length in [2_i64, 4, 7] {
            let params = format!(
                "\"params\":{{\"trend_timeframe\":{option_index},\"trend_length\":{length}}},"
            );
            let got = posture_signals(&module.evaluate(&wire(&params, &bars)).expect("evaluates"));
            let expected = {
                use pinewasm_mir::Value;
                use pinewasm_vm::{run, MarketData, RunOptions};
                let data = MarketData {
                    open: bars.iter().map(|b| b.open).collect(),
                    high: bars.iter().map(|b| b.high).collect(),
                    low: bars.iter().map(|b| b.low).collect(),
                    close: bars.iter().map(|b| b.close).collect(),
                    volume: bars.iter().map(|b| b.volume.unwrap_or(f64::NAN)).collect(),
                    time: bars.iter().map(|b| b.time).collect(),
                    timeframe: "60".to_string(),
                    ..MarketData::default()
                };
                let mut options = RunOptions::default();
                options
                    .inputs
                    .insert(0, pinewasm_hir::InputValue::String(option.to_string()));
                options
                    .inputs
                    .insert(1, pinewasm_hir::InputValue::Int(length));
                let outcome = run(&compile_csil(source), &data, &options);
                assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
                let mut out = Vec::new();
                let mut previous = 0_i64;
                for (i, value) in outcome.plots[0].values.iter().enumerate() {
                    let current = match value {
                        Value::Float(x) => *x as i64,
                        Value::Int(x) => *x,
                        _ => 0,
                    };
                    if current != previous {
                        out.push((bars[i].time, current, bars[i].close));
                    }
                    previous = current;
                }
                out
            };
            assert_same_signals(&got, &expected, &format!("{option} length {length}"));
        }
    }
    let error = module
        .evaluate(&wire("\"params\":{\"trend_length\":0},", &bars))
        .expect_err("a zero length");
    assert_eq!(error, "Indicator failure: TA input length out of range");
}

#[test]
fn a_request_that_never_executes_cannot_fail_the_evaluation() {
    // The interpreter evaluates a request only when its instruction runs, so
    // a timeframe or length it could not serve is harmless behind a branch
    // that is never taken.
    let source = concat!(
        "indicator(\"SEC-LAZY\")\n",
        "useTrend = input.bool(false, \"Use Trend\")\n",
        "tf = input.string(\"240\", \"Trend Timeframe\", options=[\"240\", \"W\"])\n",
        "length = input.int(4, \"Trend Length\")\n",
        "float trend = na\n",
        "if useTrend\n",
        "    trend := request.security(syminfo.tickerid, tf, ta.ema(close, length))\n",
        "ok = na(trend) or close > trend\n",
        "plot(ok and close > open ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    let bars = spaced_walk(300, 3_600_000, true);
    let mut module = StandaloneWasm::new(&wasm);
    let mut evaluate =
        |params: &str| module.evaluate(&wire(&format!("\"params\":{{{params}}},"), &bars));

    let expected = vm_posture(source, 0, &bars, "60");
    for params in ["", "\"trend_timeframe\":1", "\"trend_length\":9"] {
        assert_same_signals(
            &posture_signals(&evaluate(params).expect("the request never runs")),
            &expected,
            params,
        );
    }
    assert_eq!(
        evaluate("\"use_trend\":true,\"trend_timeframe\":1").expect_err("a calendar week runs"),
        "Indicator failure: this request.security timeframe is not available in standalone WebAssembly"
    );
    // An input-bound length is validated before the first bar, executed or
    // not, in the interpreter and the module alike.
    assert_eq!(
        evaluate("\"trend_length\":0").expect_err("a zero length"),
        "Indicator failure: TA input length out of range"
    );
}

#[test]
fn a_non_finite_compound_result_reads_as_na() {
    let source = concat!(
        "indicator(\"SEC-INF\")\n",
        "h = request.security(syminfo.tickerid, \"240\", -(1e999))\n",
        "plot(na(h) ? 0 : 1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    let bars = spaced_walk(120, 3_600_000, false);
    let got = posture_of(&wasm, &bars);
    assert_same_signals(&got, &vm_posture(source, 0, &bars, "60"), "infinite result");
    assert!(got.is_empty(), "the request is na on every bar");
}

#[test]
fn a_request_security_resample_is_charged_to_the_budget_when_first_read() {
    // The interpreter charges a compound request's work per bar on first
    // use: a 100000-bar average over 700 bars exceeds the per-bar budget.
    let source = concat!(
        "indicator(\"SEC-BUDGET\")\n",
        "h = request.security(syminfo.tickerid, \"240\", ta.sma(close, 100000))\n",
        "plot(na(h) ? 0 : 1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    let mut module = StandaloneWasm::new(&wasm);
    let error = module
        .evaluate(&wire_v1(&spaced_walk(700, 3_600_000, false)))
        .expect_err("the resample alone exceeds the per-bar budget");
    assert_eq!(
        error,
        "Indicator failure: per-bar instruction budget exceeded"
    );
    // Fewer bars fit, and the same instance recovers.
    module
        .evaluate(&wire_v1(&spaced_walk(600, 3_600_000, false)))
        .expect("600 bars fit the budget");
}

#[test]
fn timeframe_options_read_like_the_reference_interpreter_at_the_edges() {
    // A blank option parses as one day, not the chart's own
    // period, and a timeframe beyond a hundred years is refused when selected.
    let source = concat!(
        "indicator(\"SEC-EDGES\")\n",
        "tf = input.string(\"240\", \"Trend Timeframe\", options=[\"240\", \" \", \"40000D\"])\n",
        "h = request.security(syminfo.tickerid, tf, close)\n",
        "plot(na(h) ? 0 : close > h ? 1 : -1, \"Signal\")\n",
    );
    let wasm = compile_standalone_wasm_sliced(source, 0, true).expect("compiles");
    let bars = spaced_walk(300, 3_600_000, true);
    let mut module = StandaloneWasm::new(&wasm);
    let blank = posture_signals(
        &module
            .evaluate(&wire("\"params\":{\"trend_timeframe\":1},", &bars))
            .expect("a blank option"),
    );
    assert_same_signals(
        &blank,
        &vm_posture_with(source, 0, &bars, "60", &[(0, " ")]),
        "a blank option is one day",
    );
    assert_eq!(
        module
            .evaluate(&wire("\"params\":{\"trend_timeframe\":2},", &bars))
            .expect_err("beyond the interpreter's ceiling"),
        "Indicator failure: this request.security timeframe is not available in standalone WebAssembly"
    );
}

#[test]
fn ordered_scan_source_preserves_standalone_signal_export_by_selecting_literal_target() {
    let source = format!("{}\nvote = array.sum(predictions)\nplot(vote > 0 ? 1 : vote < 0 ? -1 : 0, \"Backtest Signal\")\n", include_str!("../../../tests/regression/indexed_distance_scan.pine"));
    let primary = compile(&source, CompileOptions::default()).program.unwrap();
    let literal = compile(
        &source,
        CompileOptions {
            disable_distance_scan_optimization: true,
            ..CompileOptions::default()
        },
    )
    .program
    .unwrap();
    let options = StandaloneWasmOptions {
        signal_plot: Some(4),
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let config = StandaloneWasmBuildConfig {
        signal_semantics: StandaloneWasmSignalSemantics::PostureEvents,
        signal_slice: true,
        ..StandaloneWasmBuildConfig::default()
    };
    assert!(standalone_eligibility(&primary, &options, &config).is_ok());
    let a = compile_standalone_wasm_program_with_config(&primary, &options, &config)
        .expect("automatic literal-target fallback");
    let b = compile_standalone_wasm_program_with_config(&literal, &options, &config)
        .expect("explicit unoptimized compile");
    audit_standalone_wasm(&a).expect("zero imports, exact host surface");
    let input = wire_v1(&alternating(257));
    let actual = StandaloneWasm::new(&a).evaluate(&input).unwrap();
    let expected = StandaloneWasm::new(&b).evaluate(&input).unwrap();
    assert_eq!(actual, expected);
    assert_same_signals(
        &posture_signals(&actual),
        &vm_posture(&source, 4, &alternating(257), "60"),
        "indexed source versus standalone literal target",
    );
}
