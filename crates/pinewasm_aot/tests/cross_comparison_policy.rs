//! The TA and pure-function cross policy is tested separately from script comparison operators.

use std::io::Write;
use std::process::{Command, Stdio};

use pinewasm_aot::{
    compile_standalone_wasm_program, header, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};
use serde_json::json;

// Numeric test program for the cross policy, with no string operations or
// runtime.error call. The input scale and bar-indexed sample array keep the
// operands runtime-dependent, and the program has six plot lanes.
const SOURCE: &str = r#"//@version=6
indicator("Cross policy", overlay=false)
float scale = input.float(1.0, "scale")
var array<float> samples = array.from(-0.0000000002, -0.00000000005, 0.0, 0.00000000005, 0.0000000002, 0.00000000005, 0.0, -0.00000000005, -0.0000000002)
float a = array.get(samples, bar_index % 9) * scale
float b = 0.0
bool up = ta.crossover(a, b)
bool down = ta.crossunder(a, b)
bool any = ta.cross(a, b)
bool exprUp = a > b and a[1] <= b[1]
bool exprDown = a < b and a[1] >= b[1]
plot(up ? 1 : 0)
plot(down ? 1 : 0)
plot(any ? 1 : 0)
plot(exprUp ? 1 : 0)
plot(exprDown ? 1 : 0)
plot(math.sign(a))
"#;

// Expected values for the six lanes over the nine bars, written out by hand.
// Lane order: crossover, crossunder, cross, script up, script down, sign.
const EXPECTED: [[f64; 9]; 6] = [
    [0., 0., 0., 1., 0., 0., 0., 0., 0.],
    [0., 0., 0., 0., 0., 0., 0., 1., 0.],
    [0., 0., 0., 1., 0., 0., 0., 1., 0.],
    [0., 0., 0., 0., 1., 0., 0., 0., 0.],
    [0., 0., 0., 0., 0., 0., 0., 0., 1.],
    [-1., -1., 0., 1., 1., 1., 0., -1., -1.],
];

// Exercise a real V8 WebAssembly module with an empty import object. Reading
// numeric plot lanes avoids imposing the indicator signal-serialization policy
// on this comparison test. The module's C ABI still parses the complete input
// document and executes every bar through its linked TA and numeric runtime.
const NODE_RUNNER: &str = r"
import {readFileSync} from 'node:fs';
const request = JSON.parse(readFileSync(0, 'utf8'));
const module = new WebAssembly.Module(Uint8Array.from(request.wasm));
const imports = WebAssembly.Module.imports(module);
if (imports.length !== 0) throw new Error('standalone module has imports');
const {exports: e} = new WebAssembly.Instance(module, {});
const input = new TextEncoder().encode(request.input);
const pointer = e.alloc(input.length);
if (!pointer) throw new Error('standalone input allocation failed');
new Uint8Array(e.memory.buffer, pointer, input.length).set(input);
const result = e.evaluate_indicator_cabi(pointer, input.length);
if (result === 0n) {
    const error = e.last_error();
    const offset = Number(error >> 32n), length = Number(error & 0xffffffffn);
    throw new Error(new TextDecoder().decode(new Uint8Array(e.memory.buffer, offset, length)));
}
const memory = new DataView(e.memory.buffer);
const bars = memory.getUint32(request.barsOffset, true);
if (bars !== 9) throw new Error('standalone did not execute every bar');
const base = memory.getUint32(request.plotsOffset, true);
const plots = Array.from({length: 6}, (_, lane) =>
    Array.from({length: bars}, (_, bar) => {
        const value = memory.getFloat64(base + (lane * bars + bar) * 8, true);
        if (!Number.isFinite(value)) throw new Error('nonfinite comparison value');
        return value;
    }));
process.stdout.write(JSON.stringify({route: 'standalone-native-v8', imports, bars, plots}));
";

#[test]
fn cross_and_sign_policy_is_tested_separately_from_comparison_operators() {
    let compiled = compile(SOURCE, CompileOptions::all());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    let program = compiled.program.expect("compiled comparison projection");

    // Deterministic bars supply bar_index; operands come from the nine sample
    // values in SOURCE.
    let data = MarketData {
        open: vec![1.0; 9],
        high: vec![1.0; 9],
        low: vec![1.0; 9],
        close: vec![1.0; 9],
        volume: vec![0.0; 9],
        time: (0_i64..9)
            .map(|bar| 1_700_000_000_000 + bar * 60_000)
            .collect(),
        ..MarketData::default()
    };
    let reference = run(&program, &data, &RunOptions::default());
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    assert_eq!(reference.plots.len(), EXPECTED.len());
    for (lane, expected) in reference.plots.iter().zip(EXPECTED) {
        let wanted: Vec<_> = expected.into_iter().map(Value::Float).collect();
        assert_eq!(lane.values, wanted, "VM plot {}", lane.slot);
    }

    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("the comparison projection has a standalone native route");
    let bars: Vec<_> = (0..9)
        .map(|index| {
            json!({
                "time": data.time[index], "open": 1.0, "high": 1.0,
                "low": 1.0, "close": 1.0, "volume": 0.0,
            })
        })
        .collect();
    let request = json!({
        "wasm": wasm,
        "input": json!({"version": 1, "params": {}, "bars": bars}).to_string(),
        "barsOffset": header::OFF_BARS,
        "plotsOffset": header::OFF_PLOTS_BASE,
    });
    let mut node = Command::new("node")
        .args(["--input-type=module", "-e", NODE_RUNNER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node.js must be available for the real native execution regression");
    let mut stdin = node.stdin.take().expect("Node input pipe");
    stdin
        .write_all(request.to_string().as_bytes())
        .expect("write Node request");
    drop(stdin);
    let output = node
        .wait_with_output()
        .expect("wait for native Node execution");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("Node output");
    assert_eq!(observed["route"], "standalone-native-v8");
    assert_eq!(observed["imports"], json!([]));
    assert_eq!(observed["bars"], 9);
    let lanes: [[f64; 9]; 6] = serde_json::from_value(observed["plots"].clone())
        .expect("six complete numeric value lanes");
    assert_eq!(lanes, EXPECTED);
}
