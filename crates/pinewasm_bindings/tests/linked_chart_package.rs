//! Runs a linked chart module from the generated JavaScript package in V8.
//!
//! `pinewasm_aot`'s `chart_wasm` suite holds linked chart modules to the
//! interpreter in `wasmi`. This holds the package to the same: that the
//! compiler it ships was built with the chart runtime at all (a package
//! built without one has `compileChartWasmProgram` but refuses every
//! program), and that a module it emits loads with nothing to import and
//! returns the interpreter's result in the engine a browser host uses: a
//! finished run's, and the error of a run that a string budget or an
//! instruction budget stops.
//!
//! Build the package with `cargo xtask wasm`, then run:
//! `cargo test --locked -p pinewasm_bindings --test linked_chart_package -- --ignored`

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires cargo xtask wasm output and a Node.js runtime"]
fn generated_package_runs_a_linked_chart_module() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm-pkg")
        .canonicalize()
        .expect("generate target/wasm-pkg with cargo xtask wasm before this test");
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", LINKED_CHART_REGRESSION])
        .arg(package)
        .output()
        .expect("Node.js is required for the generated package regression");
    assert!(
        output.status.success(),
        "linked chart regression failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The loader of docs/aot.md §14, as small as it can be written: `prepare`,
/// the lanes and limits, `chartInit`, `run`, then the plot lane from memory
/// and everything else through `chartCommand`.
const LINKED_CHART_REGRESSION: &str = r##"
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
const pkg = resolve(process.argv.at(-1));
const api = await import(pathToFileURL(`${pkg}/pinewasm.js`).href);
await api.default({ module_or_path: readFileSync(`${pkg}/pinewasm_bg.wasm`) });

// A color input and a fractional transparency (both keep a program out of
// the import-based native tier), text, a drawing and an alert.
const source = [
  '//@version=6',
  'indicator("Linked", overlay=true)',
  'tint = input.color(#3366ff, "Tint")',
  'mode = input.string("fast", "Mode", options=["fast", "slow"])',
  'fast = ta.sma(close, 3)',
  'slow = ta.sma(close, 9)',
  'basis = mode == "fast" ? fast : slow',
  'if bar_index % 7 == 0',
  '    label.new(bar_index, high, str.tostring(basis, "#.00"))',
  'if ta.crossover(close, basis)',
  '    alert("above " + str.tostring(close))',
  'plot(basis, color=color.new(tint, close > open ? 12.5 : 60))',
].join('\n');
const compiled = api.compileWithLibraries(source, []);
assert.equal(compiled.success, true, JSON.stringify(compiled.diagnostics));
const program = compiled.program;

const bars = 120;
const data = { open: [], high: [], low: [], close: [], volume: [], time: [] };
for (let i = 0; i < bars; i++) {
  const base = 100 + i * 0.05 + Math.sin(i * 0.7) * 2;
  data.open.push(base);
  data.high.push(base + 1);
  data.low.push(base - 1);
  data.close.push(base + Math.cos(i * 1.3) * 0.5);
  data.volume.push(1000 + (i % 97) * 10);
  data.time.push(1_000_000_000_000 + i * 86_400_000);
}

const artifact = api.compileChartWasmProgram(program);
assert.equal(artifact.unsupported, undefined, 'the package links the chart runtime');
assert.equal(artifact.linkedVm, true);
assert.equal(artifact.importsVm, false);
assert.equal(artifact.importsInterpreter, false);
assert.equal(artifact.executionMode, 'native');
assert.ok(artifact.hostSites > 0, 'the label and the alert are host sites');
// The same program is not native where it must import its host.
assert.equal(api.compileProgramToWasm(program).executionMode, 'hosted');

const module = new WebAssembly.Module(artifact.wasm);
assert.deepEqual(WebAssembly.Module.imports(module), []);
assert.deepEqual(
  WebAssembly.Module.exports(module).map((entry) => entry.name).sort(),
  ['chartAlloc', 'chartCommand', 'chartInit', 'memory', 'prepare', 'run'],
);

// One run of the module, as a loader makes it: an instance runs once. The
// instruction budgets go into the header as well as into the request.
const instructionsPerBar = 1_000_000_000;
const runLinked = (runOptions) => {
  const { memory, prepare, run, chartAlloc, chartInit, chartCommand } =
    new WebAssembly.Instance(module, {}).exports;
  assert.equal(prepare(bars), 0);
  const view = new DataView(memory.buffer);
  const builtins = view.getUint32(0x10, true);
  ['open', 'high', 'low', 'close', 'volume', 'time'].forEach((lane, index) => {
    new Float64Array(memory.buffer, builtins + index * 8 * bars, bars).set(data[lane]);
  });
  view.setBigUint64(0x30, BigInt(runOptions.maxInstructionsPerBar ?? instructionsPerBar), true);
  view.setBigUint64(view.getUint32(0x40, true), 1_000_000_000_000n, true); // per run
  view.setFloat64(0x38, 0.01, true); // mintick
  view.setBigUint64(0x48, 1_000_000n, true); // collection elements
  view.setBigUint64(0x50, 1_000_000n, true); // objects

  const request = new TextEncoder().encode(JSON.stringify({ data, runOptions, bars }));
  const at = chartAlloc(request.length);
  assert.notEqual(at, 0);
  new Uint8Array(memory.buffer, at, request.length).set(request);
  const command = (op, a = 0, b = 0, c = 0) => {
    const packed = chartCommand(op, a, b, c);
    const bytes = new Uint8Array(memory.buffer, Number(packed & 0xffff_ffffn), Number(packed >> 32n));
    const reply = JSON.parse(new TextDecoder().decode(bytes));
    assert.equal(reply?.chartError, undefined);
    return reply;
  };
  assert.equal(chartInit(at, request.length), 0, JSON.stringify(command(9)));
  const status = run();
  const header = new DataView(memory.buffer);
  return { status, command, memory, errorBar: header.getUint32(0x24, true) };
};

// The interpreter's answer, with its 64-bit integers as plain numbers.
const plain = (value) =>
  JSON.parse(
    JSON.stringify(value, (_key, entry) =>
      typeof entry === 'bigint' ? Number(entry) : entry instanceof Map ? Object.fromEntries(entry) : entry,
    ),
  );
const interpret = (runOptions) =>
  plain(api.runProgram(program, { ...data, mintick: 0.01 }, runOptions));

const { status, command, memory } = runLinked({});
assert.equal(status, 0, JSON.stringify(command(1)));
const reference = interpret(null);
assert.deepEqual(reference.runtimeErrors, []);

const view = new DataView(memory.buffer);
const lane = new Float64Array(memory.buffer, view.getUint32(0x18, true), bars);
const plot = reference.plots[0];
assert.equal(plot.values.length, bars);
plot.values.forEach((expected, bar) => {
  if (expected === null) assert.ok(Number.isNaN(lane[bar]), `bar ${bar}`);
  else assert.equal(lane[bar], expected, `bar ${bar}`);
});

const { series, index } = command(7, 0, 0, 0);
assert.equal(index.length, bars);
assert.deepEqual(
  {
    role: series.role,
    values: index.map((entry) => series.values[entry]),
    paletteIndices: index.map((entry) => series.paletteIndices[entry]),
  },
  plot.colors[0],
);
assert.ok(new Set(index).size >= 2, 'both transparencies appear');

assert.deepEqual(command(6, bars).plots[0], plot.style);
assert.ok(reference.alerts.length > 0 && reference.drawings.labels.length > 0);
assert.deepEqual(command(3), reference.alerts);
assert.deepEqual(command(2), reference.logs ?? []);
assert.deepEqual(command(5), reference.drawings);
assert.equal(command(0), null, 'no host error');

// The string budget. The interpreter reserves string bytes where the module
// runs the instruction itself (the constant `"fast"` on every bar) and where
// its host does (the label's and the alert's text), and the module keeps one
// count of both: under every budget that stops the interpreter it stops on
// the same bar, with status 5 and the interpreter's error.
let stopped = 0;
for (const maxStringBytesPerRun of [0, 3, 4, 100, 137, 500, 2_000, 1_000_000]) {
  const expected = interpret({ maxStringBytesPerRun });
  const linked = runLinked({ maxStringBytesPerRun });
  if (expected.runtimeErrors.length === 0) {
    assert.equal(linked.status, 0, `string budget ${maxStringBytesPerRun}`);
    continue;
  }
  assert.equal(expected.runtimeErrors[0].kind, 'string_limit_exceeded');
  assert.equal(linked.status, 5, `string budget ${maxStringBytesPerRun}`);
  assert.deepEqual(linked.command(0), expected.runtimeErrors[0]);
  assert.equal(linked.errorBar, expected.runtimeErrors[0].barIndex);
  stopped += 1;
}
assert.ok(stopped >= 6, `${stopped} budgets stopped the run`);

// An instruction budget. A stop of the module's own code is handed back,
// and one of a hosted instruction is too when only the interpreter's second
// run can say where in the source the stop is. Otherwise the hosted
// instruction's error is the interpreter's.
{
  const runOptions = { maxInstructionsPerBar: 12 };
  const expected = interpret(runOptions);
  assert.equal(expected.runtimeErrors[0].kind, 'instruction_limit_exceeded');
  const linked = runLinked(runOptions);
  if (linked.status === 6) {
    assert.match(linked.command(1), /^an instruction budget stop /);
  } else {
    assert.equal(linked.status, 5);
    assert.deepEqual(linked.command(0), expected.runtimeErrors[0]);
  }
}
console.log(
  `linked chart package checks passed: ${bars} bars, ${artifact.hostSites} host sites, ${stopped} string budget stops`,
);
"##;
