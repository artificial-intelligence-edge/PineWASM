# JavaScript and WebAssembly integration

For how the browser engine participates in both interpreted and compiled runs,
see [Understanding interpretation and WebAssembly compilation](execution-explained.md).

`pinewasm_bindings` exposes the self-contained Rust compiler and runtime to
JavaScript. It is a serialization adapter over the same compiler, PineWASM VM,
search engine, and AOT compiler used by native callers; it contains no language
or runtime semantics of its own.

This repository provides the package, not an application shell. Consumers may
call it from a browser, a Web Worker, a server-side WebAssembly runtime, or
another JavaScript host. The host owns scheduling, source storage, market data,
editing controls, visualization, and artifact persistence.

## Build and import

Use the repository-owned package pipeline. It selects the pinned Rust
toolchain, builds a fresh pinned `wasm-bindgen-cli`, and runs the pinned
Binaryen optimizer:

```sh
cargo xtask setup
cargo xtask wasm
```

`cargo xtask wasm --migrate` builds the same package with the bindings'
`migrate` feature, which adds `migrateToV6(source, idiomatic)`: the Pine v5 to
v6 source migration ([migration.md](migration.md)) for hosts that offer a
conversion when a script is rejected with `E0249`. The standard package omits
it.

The generated `@aiedge/pinewasm` ES module and `.wasm` binary land in
`target/wasm-pkg/`:

```ts
import init, {
  AotHost,
  IncrementalSession,
  compile,
  compileStandaloneWasmProgram,
  compileAndRun,
  compileAndRunWithLibraries,
  compileChartWasmProgram,
  compileProgramToWasm,
  compileWithLibraries,
  compileResident,
  outline,
  runProgram,
  searchFiles,
  symbols,
  version,
} from "./target/wasm-pkg/pinewasm.js";

// Browser or bundler initialization: the generated loader resolves its .wasm
// module relative to pinewasm.js unless a URL/Response is supplied explicitly.
await init();
```

For Node.js, load the bytes explicitly so initialization does not depend on
`fetch()` support for local files:

```js
import { readFile } from "node:fs/promises";
import init, { compileAndRun } from "./target/wasm-pkg/pinewasm.js";

const bytes = await readFile("./target/wasm-pkg/pinewasm_bg.wasm");
await init({ module_or_path: bytes });

const source = `//@version=6
indicator("Example")
x = close - close[1]
plot(x)
`;
const close = [10, 12, 11];
const result = compileAndRun(source, {
  open: close,
  high: close,
  low: close,
  close,
  volume: [1, 1, 1],
});

if (result.diagnostics.length || result.runtimeErrors.length || !result.plots?.[0]) {
  throw new Error(JSON.stringify({
    diagnostics: result.diagnostics,
    runtimeErrors: result.runtimeErrors,
  }));
}
console.log(result.plots[0].values); // [ null, 2, -1 ]
```

`initSync` from the generated binding is available when a host already owns a
`BufferSource` or `WebAssembly.Module`. Initialize once before using any named
export.

The generated `pinewasm.d.ts` is the checked contract for every high-level
export and serialized data-transfer object below. `Program` is deliberately an
opaque branded type: JavaScript may inspect its `inputs` manifest, but
TypeScript consumers cannot fabricate a validated program. The `any` values
that remain under wasm-bindgen's `InitOutput` interface describe raw numeric
and externref ABI functions; application code should use the typed exports
above that low-level interface.

## API surface

| Export | Purpose |
| --- | --- |
| `compile(source, options?)` | Compile one source buffer to diagnostics and a validated CSIL program. |
| `compileWithLibraries(source, libraries, options?)` | Compile with an explicit in-memory library map. |
| `compileResident(source, libraries, options?)` | Compile with the same library map and keep CSIL in this Wasm instance. Returns a `CompiledProgram` handle instead of serialized CSIL. |
| `compileAndRun(source, marketData, compileOptions?, runOptions?)` | Compile and execute on OHLCV data. |
| `compileAndRunWithLibraries(source, libraries, marketData, compileOptions?, runOptions?)` | Link libraries, compile, and execute. |
| `runProgram(program, marketData, runOptions?)` | Revalidate and execute a cached CSIL program without recompiling. |
| `new IncrementalSession(program, marketData, runOptions?)` | Keep VM state across ordered realtime ticks. |
| `compileProgramToWasm(program)` | Emit the universal CSAW AOT artifact and layout metadata. |
| `compileChartWasmProgram(program)` | Emit a native chart module that links the VM as its own host and imports nothing (`linkedVm: true`). The loader passes the run's request to the module's `chartInit` export and reads the host's results through `chartCommand`, in place of `AotHost`. See [docs/aot.md §14](aot.md#14-linked-chart-modules). |
| `compileStandaloneWasmProgram(program, options)` | Emit or structurally refuse an artifact for the standalone WebAssembly v1 import-free signal-consumer profile. |
| `new AotHost(program, marketData, runOptions?, bars?)` | Back residual `host.vm` sites in a CSAW module over `bars` bars (by default the run's `maxBars` over the feed, the bars the interpreter runs); for a strategy module whose layout reports `hostBroker`, `drainStrategy(status, barsExecuted)` returns its strategy results, or null for a run that stopped before its first bar. |
| `searchFiles(files, query, options)` | Search bounded in-memory source buffers. |
| `outline(source)` / `symbols(source)` | Return parse-tolerant source-navigation data. |
| `version()` | Return compiler, CSIL, language, rendering contract, and optional source-revision metadata. `renderingContractVersion` equals `renderingContract().schemaVersion` and the `rendering_contract_version` of every program this compiler emits, so a host can reject an incompatible compiler at load. |

Compile options are camel-case booleans. Use either the `emitIl`
wire name or its descriptive `emitCsil` alias, but do not provide both in the
same object:

```ts
type CompileOptions = {
  emitAst?: boolean;
  emitHir?: boolean;
  emitMir?: boolean;
} & (
  | { emitIl?: boolean; emitCsil?: never }
  | { emitIl?: never; emitCsil?: boolean }
);
```

The current pre-1.0 JavaScript wire format uses the keys `emitIl`, `ilText`,
`ilMs`, and `ilVersion`. Deserialization also accepts the expanded `emitCsil`,
`csilText`, `csilMs`, and `csilVersion` spellings, while PineWASM emits the
keys listed above so that serialized records stay readable within the current
package line. Rust APIs and documentation use CSIL
terminology throughout. These names are documented behavior, not a promise of
pre-1.0 compatibility across future package versions.

A successful `CompileResult` has `success: true`, diagnostics, optional debug
dumps requested above, and a validated `program`. The program is a versioned
CSIL value that may be cached and passed to `runProgram`, `IncrementalSession`,
or either AOT compiler. Every execution entry point validates untrusted or
version-skewed programs before running them.

`MarketData` requires equal-length `open`, `high`, `low`, `close`, and `volume`
arrays. Optional fields include `mintick`, `time`, `timeClose`,
`requestTimeClose`, `requestBars`, `tradingDay`, `timeframe`,
`syminfo`, and explicit host tables for supported `request.*` and footprint
interfaces. When `time` is absent, the VM creates a deterministic daily lane;
when present, it must be strictly increasing and bar-aligned.
`syminfo.timezone` must be a fixed offset (`"UTC-5"`) or an IANA name
(`"America/New_York"`), and `syminfo.session_hours` a session
specification (`"24x7"`, `"0930-1600"`) or empty; market data with anything
else is refused. Higher-timeframe buckets on a non-UTC exchange need the
session declared (see [exchange time zones and sessions](runtime.md#exchange-time-zones-and-sessions)).
`timeClose` supplies authoritative chart closing instants. `requestTimeClose`
maps time-based Pine timeframe keys to bar-aligned requested closing instants
for the chart symbol and requires a complete `timeClose` lane. These clocks
carry timestamps only; the VM derives requested values from OHLCV.
`requestBars` maps timeframe keys to complete OHLCV/time feeds for same-symbol
requested bars, preserving higher-timeframe history omitted by sparse chart
bars. These feeds require aligned, ordered arrays spanning the chart history.
An incremental append with authoritative clocks requires rebuilding a complete
history snapshot. See [runtime publication clocks](runtime.md) for validation
and fallback behavior.

`RunOptions` controls deterministic resource limits, bar limits, profiling,
realtime simulation, input overrides, external source lanes, and strategy
overrides. Defaults are applied when the object is `null` or `undefined`.
The authoritative field definitions and defaults live in
`crates/pinewasm_vm/src/lib.rs`.

`RunResult` contains compile diagnostics, structured runtime errors, plot and
visual lanes, fill bands, drawings, logs, alerts, optional strategy results,
bar counts, optional realtime traces, and timing data. A language-level `na`
is serialized as `null`.

## Wire rules

- Data-transfer-object field names are camelCase except where the source
  language or an existing protocol supplies the key. In particular,
  `StandaloneWasmOptions` uses `signal_plot`, `module_type`, `signal_semantics`, and
  `max_collection_elements`; namespace-derived `syminfo`, `upcomingEvents`,
  footprint, and volume-row properties retain keys such as `main_tickerid`,
  `dividends_future_amount`, `buy_volume`, and `has_buy_imbalance`; and a
  ternary input-active expression uses `when_true` / `when_false`. These
  exceptions are represented literally in `pinewasm.d.ts`.
- Source spans are UTF-8 byte offsets. Navigation rows use zero-based UTF-16
  line and column coordinates suitable for JavaScript editors.
- Integer-valued Rust fields that require exactness—including Pine Script®
  `int` values and 64-bit indices or counts—cross the direct Wasm binding as
  `bigint`.
  Inputs may use safe JavaScript numbers where representable or `bigint` for
  exact integers. As a result, `CompileResult.program` and other returned DTOs
  can contain `bigint` values. Structured clone and IndexedDB preserve them;
  plain `JSON.stringify` throws on them, so JSON persistence requires an
  explicit `bigint` codec and matching decoder.
  Never round an unsafe value through JavaScript `number`.
- Pine `na` and Rust `Option::None` values on fields without an omission rule
  serialize as `null`. Fields marked optional in `pinewasm.d.ts` may instead be
  absent when they are empty or unavailable; test field presence rather than
  assuming every absent value is represented by `null`.
- Omit unused optional object properties rather than assigning JavaScript
  `undefined`. The Serde boundary rejects a present property with an
  `undefined` value; passing an entire optional function argument as
  `undefined` remains supported.
- Diagnostics and runtime failures are data. Malformed API arguments and
  serialization failures throw JavaScript string values; catch them as
  `unknown` and normalize with `String(error)` rather than assuming an `Error`
  instance.
- Compilation, execution, and AOT calls are synchronous after module
  initialization. A latency-sensitive host should put them on a worker or an
  equivalent interruptible execution boundary.

The compiler rejects a source buffer above 1 MiB. Multi-source compilation
accepts at most 128 libraries, each path is at most 256 UTF-8 bytes, each
library source is at most 1 MiB, and aggregate library source is at most 8 MiB.
A compile reports at most 1,000 errors and 1,000 warnings, whatever the source:
when it has more, `diagnostics` ends with an `E0002` or a `W0007` that has no
label and says the rest were left out (see
[diagnostic codes](language-support.md#diagnostic-codes)). `success` does not
depend on what was left out.
The VM adds instruction, stack, string, collection, and owned-memory budgets;
hosts embedding untrusted scripts should set explicit limits appropriate to
their service.

## Inputs (host-overridable defaults)

Every `input.*` declaration becomes an entry in `program.inputs`. Each entry
has a stable numeric id, kind, title, declared default, constraints,
presentation metadata, and any bounded host-reactive `active` expression.

At execution time, `RunOptions.inputs` is a map keyed by that numeric id. A
missing override uses the declared default. An override with the wrong type is
ignored and the default is used, so a host should validate user values against
the manifest before calling the VM.

Use a native JavaScript `Map`, not an object with numeric-looking string keys.
Every override is also externally tagged and carries exactly one variant tag; a
raw scalar or an object with multiple tags is not an input value:

```ts
import type { InputValue, RunOptions } from "@aiedge/pinewasm";

const examples: InputValue[] = [
  { Int: 20 },
  { Int: 9_007_199_254_740_993n },
  { Int: "9007199254740993" },
  { Float: 2.5 },
  { Bool: true },
  { String: "regular" },
  { Color: { r: 33, g: 150, b: 243, t: 0 } },
  { Source: "Close" },
  { Enum: { name: "Mode", index: 0, member: "Regular" } },
];

const runOptions: RunOptions = {
  inputs: new Map([[0, examples[0]]]),
};
```

The `Int` decimal-string form preserves an exact signed 64-bit value for hosts
that cannot use `bigint`. Plain objects such as `{ 0: { Int: 20 } }`, raw
entries such as `new Map([[0, 20]])`, and multi-tag values such as
`{ Int: 20, Float: 20 }` fail argument deserialization and throw.
A well-formed tag for the wrong declared input kind is instead ignored by the
runtime, which deterministically uses the declaration's default.

The TypeScript declarations type values in compiler metadata (an input's
`defval`, its `constraints` and the values in its `active` expression) as
`DeclaredInputValue`. There an `Enum` value always has `member` and a nullable
`title`, and an `Int` value is always a `bigint`. An override typed
`InputValue` may omit `member` and `title` (the override is selected by `name`
and `index`) and may give an `Int` as a number, a `bigint` or a decimal
string. Every `DeclaredInputValue` is also a valid
`InputValue`, so a host can pass a declared default back as an override.

`input.source` may select a builtin lane or an external source. External source
identity is supplied through `inputs[id]`; its numeric series is supplied
separately through `externalSources[id]`. This separation prevents stale data
from being reused after the selected source changes. The package deliberately
does not create controls or discover cross-script lanes for the host.

```ts
const source = {
  scriptId: "sample-script",
  plotSlot: 0,
  plotLabel: "Signal",
};

const crossScript: RunOptions = {
  inputs: new Map([[4, { ExternalSource: source }]]),
  externalSources: new Map([[4, { source, values: [null, 1, -1] }]]),
};
```

## Libraries

Libraries are supplied as an array of `[importPath, source]` pairs. The import path is an arbitrary key that the main source must use:

```ts
const libraries = [
  ["example-org/math-helpers", librarySource],
] as Array<[string, string]>;

const result = compileWithLibraries(mainSource, libraries, {
  emitIl: true,
});
```

The JavaScript boundary reads ordinary arrays through their own indexed data
properties. Each entry must be an array containing two string data properties;
sparse entries and accessors are rejected, and custom iterators are not used.
Failures while inspecting a proxy are rejected. Strings must contain
well-formed UTF-16. The boundary takes one immutable snapshot and checks UTF-8
lengths before copying sources into Wasm: at most 128 libraries, 256 bytes per
import path, 1 MiB per source, and 8 MiB of aggregate library source. A host must
provide `TextEncoder`; `String.prototype.isWellFormed` is optional because a
bounded fallback validates surrogate pairs.

Imports resolve against this array only. The compiler never accesses the
network. Diagnostic spans use source id `0` for the main buffer and `i + 1`
for `libraries[i]`, allowing the host to route each diagnostic back to the
correct source.

## Repeated independent evaluations

For a worker that compiles and immediately prepares a VM or standalone module,
`compileResident(source, libraries, options)` avoids serializing the entire CSIL
program to JavaScript and deserializing it back into Rust. Pass `null` or
`undefined` for no libraries. Its `ResidentCompileResult` keeps the usual success,
diagnostics, timings and requested debug fields; `program` is a `CompiledProgram`
handle on success and `null` on failure.

```js
const compiled = compileResident(source, libraries, { emitIl: false });
if (!compiled.success || !compiled.program) throw new Error('Compile failed');
const code = compiled.program;
try {
  const declarations = code.inputs;
  const vm = code.prepareVm();
  try {
    // vm.runColumns(...) or vm.runDataset(...), with fresh state each time.
  } finally {
    vm.free();
  }
  const artifact = code.compileStandaloneWasm({ module_type: 'strategy' });
  // artifact has the same success/refusal shape as compileStandaloneWasmProgram.
} finally {
  code.free();
}
```

`inputs`, `plots` and `metadata` return independent declaration copies. Full
inspection or transport is opt-in through `exportProgram()`. Mutating any copy
does not change retained code. `prepareVm()` copies and validates code within
Rust, and `compileStandaloneWasm()` uses the same planner, validation, emission
and byte audit as the serialized route. A prepared VM owns its code and remains
valid after the compiled handle is freed.

Handles belong to one Wasm instance and cannot be transferred to another worker.
Call `free()` on eviction, including when a later preparation step fails. Each
source edit requires a new compilation; this API does not cache analyses or
runtime state between edits. The existing serialized APIs remain available.

`new PreparedProgram(program)` validates a compiled program once and retains
only immutable code. `runColumns(columns, metadata, options, numeric_plots)`
starts fresh VM, broker, input, and execution-budget state on each call. Free
the handle when it leaves a worker cache. `runProgram` remains available.

`columns` is a `Float64Array` of six consecutive equal-length lanes in
open/high/low/close/volume/time order. Pass a `MarketData` metadata object with
empty primary lanes; optional host feeds remain supported. Times must be safe
integer milliseconds. Malformed lengths, nonempty primary metadata lanes,
invalid times, or invalid options reject the call.

With `numeric_plots: false`, the return value is the complete `RunResult`.
With `true`, the return value is `NumericRunResult`: numeric plots and the
strategy's equity/margin lanes use owned `Float64Array` values, with `NaN` for
missing numeric cells. Visual, color, fill-band, and drawing serialization is
omitted from this explicit projection; script execution and the full native
strategy metadata, diagnostics, logs, and alerts are retained. Incompatible
plot types or unsafe integer values reject the numeric projection. Returned
arrays remain valid after later evaluations, memory growth, and handle release.

## Incremental execution

`IncrementalSession` validates the program, installs past market data,
and executes that confirmed prefix once. `advance(ticks)` applies ordered
realtime snapshots to the same VM state. `result()` returns the current
`RunResult`; `committedBars`, `residentBars`, and `failed` expose bounded
session state.

Call the generated `free()` method when a session is no longer needed. A failed
session is terminal and should be replaced rather than reused.

## Ahead-of-time artifacts

`compileProgramToWasm(program)` returns a `Uint8Array` plus CSAW ABI and layout
metadata. Its `executionMode` is `native` when strict lowering succeeds and
`hosted` when the artifact delegates to the interpreter. A native
artifact may still import `host.vm`; importlessness is a separate property.
An AOT reroute status such as 6 (`ABI_MISMATCH`) requires the embedding host
to discard the native result and rerun the original request from fresh state
on the VM. `AotHost.vm` returns the status without performing that
rerun; it is an import adapter, not a complete run-selection controller.

`compileChartWasmProgram(program)` returns the same shape for a
[linked chart module](aot.md#14-linked-chart-modules), with `linkedVm: true`
and `importsVm: false`. The module carries the VM as its own host
and imports nothing, so its loader needs no `AotHost`: it instantiates the
module against an empty import object, calls `prepare`, writes the lanes and
numeric inputs, passes the request from `chartRequestJson(data, runOptions,
bars)` to `chartInit` (in a `chartAlloc` block), calls `run`, and reads the
host's side of the result through `chartCommand`. An instance runs once.
Pass the `runOptions` object itself to `chartRequestJson`: its `Map`-backed
`inputs` and `externalSources` are encoded as objects there, which
`JSON.stringify` would turn into `{}`. `chartInit` refuses `realtime` and
`profile` runs with the ABI-mismatch status, so those runs stay on the
interpreter. `bars` must equal `min(maxBars, data bars)`, and a non-finite
value in an auxiliary lane (`requestData`, `requestBars`, `securityLowerTfData`,
`bid`, `ask`, or a footprint field) declines the run with a reason. The reroute
statuses mean what they mean above. Every error of a bar is status 5, and
`chartCommand(0)` returns it as the interpreter raises it, the stops of the
string budget and of `max_bars_back` included: the loader of a linked
module builds no error itself, and an instruction budget stop of the
module's own code is handed back (status 6). A program whose colors
`compileProgramToWasm`
hosts on the interpreter (color inputs, fractional transparency) compiles
natively here. The function returns `{ unsupported }` for a program the
native tier refuses.

`compileStandaloneWasmProgram(program, { signal_plot?, module_type, signal_semantics?,
max_collection_elements?, signal_slice? })` targets the stricter
[standalone WebAssembly v1 import-free signal-consumer profile](aot.md#12-standalone-webassembly-interfaces).
`signal_plot` accepts a JavaScript `number` or `bigint`, matching its Rust
`usize` boundary. Each optional property also accepts explicit `null`, with the
same effect as omitting it. `signal_slice` defaults to `true`: only what can
reach the designated plot is compiled, so display code cannot refuse the
script or fail the run (see the signal slice in
[aot.md](aot.md#12-standalone-webassembly-interfaces)); pass `false` for an
artifact that reproduces every failure of the interpreter run.
Success returns `{ wasm, moduleType, signalPlot,
moduleSha256 }`. A valid but ineligible program returns
`{ refused: { code, message, imports?, feature?, class, kind, site? } }`;
malformed arguments still throw.

For an indicator source, a strategy-type module uses `signal_plot` (or the
only plot by default). `signal_semantics` defaults to reversal-only
`position-events`; a caller that designates a true +1/0/-1 posture lane may
request `posture-events`, which serializes transitions back to `0` as flat
exits and records that contract in module metadata. An indicator-type module
must not designate a plot and returns an empty signal series. A `strategy()` source
compiles through the linked broker when eligible: its effective module type is
always `strategy`, `signalPlot` is `null`, and its signals are flat-aware
bar-close posture events from broker state.

Native strategy evaluation accepts optional positive finite `mintick`,
`mincontract`, and `pointvalue` fields in the JSON request. They configure the
linked broker's price precision, quantity increment, and contract value.
Omitted fields keep their default values; invalid or duplicate fields
fail evaluation. They do not add a general currency or symbol-provider API.

A request may also name the chart timeframe of its bars in a `timeframe`
string, in Pine notation (`"240"`, `"1D"`), the value the interpreter reads
from `MarketData.timeframe`. A module whose script reads it, through
`timeframe.in_seconds()`, `timeframe.multiplier`, a `timeframe.is*` predicate
other than `timeframe.isticks`, `time_close`, `timenow`, or a fixed-width
`time(timeframe)` or `timeframe.change`, lists
`"required_fields": ["timeframe"]` in its `pinewasm_capabilities` section and
refuses a request without it; every other module ignores the field. Send the
timeframe of the bars being evaluated with every request: the same module
serves each dataset it is evaluated on. Bar times must be safe integer
milliseconds in a JSON request, as they must in binary input. See
[the chart timeframe in the AOT guide](aot.md#the-chart-timeframe-and-fixed-timezone-calendars).

A module that groups bars into higher-timeframe buckets (a same-symbol
`request.security`, or a served `time(timeframe)` or `timeframe.change`) also
lists `"timeframe"`, `"timezone"` and `"session_hours"`: the chart
timeframe and the market data's `syminfo.timezone` and
`syminfo.session_hours`, sent as strings (`"session_hours":""` for none). It refuses a request without them. It groups
bars on a fixed-offset clock only, and an executed site stops on an IANA
timezone or an unsettled session with an `Indicator failure` message; see
[the exchange clock and session](aot.md#the-exchange-clock-and-session).

For native `strategy()` modules, `include_results: true` adds numeric `plots`
and a `strategy` object alongside the existing signals. The strategy object
contains stored trades (IDs, direction, quantity, entry/exit bars and prices,
profit and commission), equity, final position, net/gross profit and loss,
closed-trade count, maximum drawdown, and the first stored trade index.
Non-finite numeric values serialize as `null`. This is a numeric execution
result, not a chart-object or complete host-report serialization. Omitting the
flag produces the signals-only document. Each evaluation resets the
flag and symbol economics, including reuse after an earlier full-result call.

Two compact alternatives avoid the plot and equity lanes. `objective_results:
true` returns the `retained-objectives-v1` metrics, and `trade_results: true`
returns the `broker-trades-v1` closed-trade ledger. Each row preserves its raw
`exitId` and carries optional `comment` and `exitOrderName` strings when the
broker supplies them. For an exit display label, prefer a nonempty `comment`,
then `exitOrderName`, then `exitId`. Empty comments are absent, as in the
interpreter. Neither projection carries signals. Each is advertised in the
module's `pinewasm_capabilities` section
and cannot be combined with the other or with full results; see the [strategy
projections in the AOT guide](aot.md#compact-strategy-trade-ledger).

Native-heap standalone WebAssembly modules default to the conservative 100,000-element ceiling.
Because a standalone WebAssembly host cannot overwrite the CSAW header after `prepare`, a caller
may bake a larger `max_collection_elements` into the artifact, up to the
4,000,000-element browser ceiling.

The current pre-1.0 package reports these machine-readable refusal classes:

| Code | Meaning |
| --- | --- |
| `StandaloneWasmNotStandalone` | The projected indicator still requires `host.vm`; `imports` names the module imports and `feature` names the Pine calls that need them (comma-separated). |
| `StandaloneWasmStrategyFeatureUnsupported` | A strategy feature cannot be represented by the native broker path; `feature` names it. |
| `StandaloneWasmNoDesignatedPlot` | The plot designation is missing, out of range, or forbidden for the requested/source module type. |
| `StandaloneWasmSourceInputUnsupported` | An `input.source` default is not one of the six raw builtin lanes carried by the wire format. |
| `StandaloneWasmProgramUnsupported` | The program is outside the shared AOT subset or is invalid/version-skewed. |
| `StandaloneWasmAuditFailed` | Emitted bytes failed the producer-side audit; this is an implementation defect, not a script correction. |

A refusal code does not say whether the compiler declined the program for a
declared reason or contradicted itself: an unsupported opcode and a type
conflict both arrive as `StandaloneWasmProgramUnsupported`. Three further
properties of `refused` carry that, and a host classifies on them rather than
on `message`:

| Property | Value |
| --- | --- |
| `class` | `"capability"`: a declared limit of the compiler or of the requested build, to report to the user. `"internal"`: not reachable for a program this compiler produced. For such a program it is a compiler defect to report, not a script to change; for a hand-built, stale or tampered program it is an ordinary rejection. |
| `kind` | The compiler's name for the refusal, finer than `code` and stable across programs, for example `UnsupportedOpcode`, `TypeConflict`, `StandaloneWasmNotStandalone`. It is Rust's `AotError::kind()`. |
| `site` | Present only when the compiler reports one: `{ pc }`, `{ input }` or `{ plot }`. `input` is an input declaration id and `plot` is the `signal_plot` that was passed. `pc` is an instruction index in the program the compiler examined; with `signal_slice` that may be the signal slice, whose indices differ from the supplied program's, so treat it as a diagnostic. A refusal raised while emitting or auditing the module has no site. |

For example, a script that reads a string input, and a program whose one slot
is stored as both an integer and a float:

```json
{ "refused": { "code": "StandaloneWasmNotStandalone",
               "message": "standalone WebAssembly export requires a standalone-eligible program: program would import host.vm",
               "imports": ["host.vm"],
               "feature": "input.string with no options list, string comparison (== / !=)",
               "class": "capability", "kind": "StandaloneWasmNotStandalone", "site": { "pc": 0 } } }

{ "refused": { "code": "StandaloneWasmProgramUnsupported",
               "message": "type conflict: slot 0 stored as F64 but pinned I64",
               "class": "internal", "kind": "TypeConflict" } }
```

`class`, `kind` and `site` are additional properties. A
`StandaloneWasmNotStandalone` refusal's `feature` names the Pine calls that
need `host.vm`, for the program the refusal reports on (the signal slice when
one applies). A strategy refusal's `feature` names the unsupported feature and
never an argument the compiler supplied for a call, such as the
`timeframe.period` that `timeframe.in_seconds()` reads. Collection and object operations that
have a native lowering are left out while any other call is named, because
they usually stay on the host only on its account; they are named when nothing
else is.

Artifact bytes, layout fields, loader obligations, and the `AotHost` import
contract are documented in [Ahead-of-time compilation](aot.md) and the
[normative CSAW ABI specification](spec/csaw-module-abi.md).

## Search and navigation

`searchFiles` accepts `[{ path, content }]` plus:

```ts
interface SearchOptions {
  caseSensitive: boolean;
  smartCase?: boolean;
  wholeWord: boolean;
  isRegex: boolean;
  maxResults: number;
}
```

It allows at most 128 files, 1 MiB per file, 8 MiB total content, 256 UTF-8
bytes per path, a 4 KiB query, and 20,000 returned ranges. Output contains
line-local UTF-16 ranges, a total hit count, and `hitLimit` when truncated.
Malformed regular expressions reject the call.

`outline` returns a hierarchical tree; `symbols` returns flat source symbols.
Both tolerate incomplete source but retain the compiler's 1 MiB source limit.

## Cold start of the compiler package

In a fresh JavaScript engine, the first `compile()` and
`compileStandaloneWasmProgram` calls spend most of their time compiling the
compiler. V8 compiles each Wasm function lazily, in full and with its baseline
compiler, on its first call. Compiling every function up front
(`--no-wasm-lazy-compilation`) removes most of the time a cold `compile()` or a
cold emission takes. The cold cost follows the size of every function the calls
enter, including branches they never take.

`wasm-opt -O` inlines each function that has a single caller, whatever its
size. As the compiler grew, that fused its pipelines into functions of up to
about 100 KiB, such as `compile_with_libraries_profiled` and the emitter's
`emit_module`, so a cold call compiled their unused branches too. The
package build therefore inlines only single-caller functions of at most 50
Binaryen expressions (`WASM_OPT_ARGS` in `crates/pinewasm_xtask/src/package.rs`),
which reduces the compiler code a cold `compile()` or a first emission
compiles. The AOT runtimes an artifact links do not pass through this step, so
emitted artifacts are byte-identical.

The fused functions also delayed the host's `WebAssembly.compile` of the
emitted module, although that module compiles quickly on its own. During the
two calls, V8 recompiles the hottest compiler functions with its optimizing
tier on background threads, and the new module's decoding and validation wait
for a free thread (Node.js runs four by default). With smaller functions those
jobs finish quickly. In Node.js, an `await WebAssembly.compile(...)` with
nothing else pending on the event loop also waits until every background V8
task has finished; `new WebAssembly.Module(bytes)` does not.

To see what a cold call compiles, trace one fresh process of a script that
loads the package and makes the calls:

```sh
node --liftoff-only --trace-wasm-compilation-times host-script.mjs
```

Each `Compiled function` line names a function index and its body size. The
same package optimized again with `wasm-opt -g` added to `WASM_OPT_ARGS` keeps
an identical code section and also carries the function names.

## Host responsibilities

The package intentionally leaves the following outside the repository:

- source and library persistence;
- editor integration and diagnostic rendering;
- market-data acquisition, provenance, and licensing;
- charts, input controls, and strategy-report presentation;
- worker lifecycle, timeouts, cancellation, and cache eviction;
- artifact storage and integrity policy.

This boundary keeps one language implementation while allowing independent
applications to embed it without coupling the compiler release to a sample UI.

### Standalone WebAssembly naming and migration

Use **WebAssembly compilation** for compiling a script, **Compiled WebAssembly**
for the execution path, and **Standalone WebAssembly** for a module verified to
have no imports. The browser's **Bytecode Interpreter** also runs inside
WebAssembly; its execution model is distinct from compiling each script into
its own module.

The consumer API is `compileStandaloneWasmProgram(program, options)`, with
`StandaloneWasmOptions`, `StandaloneWasmCompileResult`,
`StandaloneWasmRefusalCode`, and `StandaloneWasmRefusalClass`. The Rust equivalents use `StandaloneWasm*` types
and `compile_standalone_wasm_program`. The low-level chart-interface compiler
`compile_standalone_program` remains separate: it returns the ordinary chart
layout after a zero-import audit, rather than the consumer interface below.

Hosts import the current `StandaloneWasm*` API names, handle the refusal codes
above, and check packet signatures against the layouts below. Compiled modules
are regenerated when the compiler changes, and their pinned hashes change with
them.
The eight-byte signatures are `PWSWBIN1` (binary input), `PWSWBIN2` (resident
upload), `PWSWREF1` (resident reference), and `PWSWOUT1` (binary output). Their
layouts and version numbers are fixed. JSON-v1 requests and the exported
evaluation functions keep the shapes described here. Artifact notices use
`standalone_wasm_abi_version`. Binary packets must match the module build they were compiled for.

Execution names do not determine output scope: describe **signal output**,
**full strategy results**, **objective results**, or **trade-ledger results**
separately.

### Standalone WebAssembly binary input, version 1

The zero-import `evaluate_indicator_cabi` entry point accepts its original JSON
input or an additive binary packet. Bytes 0..8 are ASCII `PWSWBIN1`, bytes 8..12
are the little-endian u32 bar count, and bytes 12..16 are the little-endian u32
metadata byte length. UTF-8 JSON metadata begins at byte 16 and uses the existing
version/params/symbol-economics/include_results fields and `timeframe`. It must
omit bars and fraction. Pad to an 8-byte boundary, then append six little-endian f64 lanes in
open/high/low/close/volume/time order. The packet length must match exactly.
Timestamps must be safe integer milliseconds; non-finite price/volume cells
become NaN. Slice data before encoding when a prefix is required.

The runtime validates the allocation, packet lengths, metadata and timestamps,
then initializes the same input defaults/source lanes and executes the same
compiled program. Full JSON results and error classes are unchanged; the
resident allocator/reset protocol is unchanged. Malformed packets return the
error slot without trapping and do not prevent a subsequent valid run.

A strategy caller can also opt into `binary_results: true` alongside
`include_results: true`. The returned allocation then contains `PWSWOUT1`
(eight bytes), u32 plot count, u32 bar count, u32 metadata length, and a reserved
zero u32. All integers are little-endian. A JSON document follows at byte 24;
it contains the same signals, trade ledger and strategy metrics, with empty
`plots` and `strategy.equity` arrays. Pad the end of that metadata to eight
bytes, then read one f64 lane per plot followed by the f64 equity lane. Each
lane has the declared bar count; non-finite cells represent JSON null. Check
the exact packet length before reading. All lanes are copied into the returned
allocation and remain valid until deallocation/reset; hosts must copy before
another evaluation. This changes transport only, with every numeric result
preserved. The existing JSON output remains the default.


### Resident datasets for repeated VM evaluations

`new PreparedDataset(columns, metadata)` owns a copy of the six numeric input
lanes and the normal `MarketData` metadata. It accepts 1 to 1,000,000 primary
bars and the same safe-integer timestamp and empty-primary-metadata rules as
`PreparedProgram.runColumns`. `barCount()` and `primaryBytes()` describe the
owned primary lanes; optional feeds and metadata are not included in that byte
count. The constructor fails before producing a handle if admission fails.

`preparedProgram.runDataset(dataset, options, numeric_plots)` borrows that
immutable dataset inside Wasm, eliminating repeated numeric packing, metadata
deserialization and boundary copies. Each call still initializes a fresh VM
and broker, validates the current run options, observes the same instruction
limits, and produces the same owned full or numeric output as `runColumns`.
Caller mutation after construction cannot change the dataset. A reported
runtime error cannot change its immutable content, so a subsequent fresh run
may reuse it. A Wasm trap requires discarding the worker/engine instance.

Reuse the dataset across eligible prepared programs or parameter trials, then
call its generated `free()` method. Program and dataset lifetimes are explicit
and independent. This API caches no indicator calculations or trial results;
the `runColumns` and `runProgram` methods remain available.

### Vertical gradient fills

The vertical `fill(plot1, plot2, top_value, bottom_value, top_color,
bottom_color, ...)` overload retains four per-bar channels in `fills[].colors`.
The `top_value` and `bottom_value` roles contain numeric values; `top_color`
and `bottom_color` contain color strings or null. The existing `color` role
continues to describe ordinary fills. Hosts must use the two referenced plots
as the fill mask and the gradient values as independent color-stop positions.
A mask smaller than the gradient interval therefore shows only part of the
color ramp. These channels also survive input overrides and serialization.

Gradient fills currently use the interpreter-hosted tier when compiling a
program to WebAssembly: the native packed-color output layout cannot encode
numeric gradient channels. The compiler selects this fallback explicitly
instead of treating numeric bounds as packed colors.

### Host chart theme

`CompileOptions.chartBackground` and `chartForeground` accept RGB triples
(values 0–255). They supply `chart.bg_color` and `chart.fg_color` before
semantic analysis, so theme-dependent Pine expressions and color selection
use the real chart theme. Omitted values preserve the headless defaults of
white background and black foreground. The shared default registry is not
mutated. Hosts must include these options in compiled-program cache keys and
recompile/re-run on chart theme changes; runtime input overrides do not change
these compiled theme values.
