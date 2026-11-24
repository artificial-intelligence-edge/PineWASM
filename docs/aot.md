# Ahead-of-time compilation to WebAssembly

<!-- BEGIN GENERATED ARTIFACT POLICY -->
This block is generated from PineWASM's artifact notice model.
Notice revision: artifact-notice-v1
Project license: AGPL-3.0-only
Runtime output exception: none
Dependency-license status: recorded

PineWASM distributes artifacts under AGPL-3.0-only: when an artifact is conveyed, its corresponding source must be made available from the PineWASM repository at the commit recorded in the artifact notice bundle or through another AGPL-compliant method; the repository URL and commit identify that source, and no separate runtime output exception is granted.

Dependency license elections:
- Apache-2.0 OR BSL-1.0 -> Apache-2.0
- MIT OR Apache-2.0 -> MIT
- Unlicense OR MIT -> MIT
- MIT OR Apache-2.0 OR LGPL-2.1-or-later -> Apache-2.0
<!-- END GENERATED ARTIFACT POLICY -->

The generated block is the authoritative notice-model statement. The
remaining prose in this engineering document is informational.

PineWASM can lower a validated CSIL program to a per-script WebAssembly
module. PineWASM's deterministic VM remains the internal semantic baseline:
native AOT is an acceleration tier, while the universal artifact path preserves
the VM execution path for programs outside the native subset.

The module format is not implied by this engineering overview. The normative
contract is the Apache-2.0-licensed
[CSAW module ABI specification](spec/csaw-module-abi.md). The implementation
currently emits CSAW ABI **version 6** from `pinewasm_aot`.
`pinewasm_bindings` exposes the bytes, layout metadata, and `AotHost` support a
loader needs; a conforming loader must reject an ABI-version mismatch before
running.

The **Standalone WebAssembly interface v1** defines the import-free
signal-consumer profile. It exposes a designated indicator signal
or strategy posture plus bounded parameter metadata, rather than the complete
chart-facing result. Its exact request, export, audit, and refusal contract is
defined in [§12](#12-standalone-webassembly-interfaces).

## 1. The artifact classes

The public APIs make portability explicit instead of treating every valid
WebAssembly file as standalone:

| Class              | Compiler entry point         | Imports           | Meaning                                                                                                    |
| ------------------ | ---------------------------- | ----------------- | ---------------------------------------------------------------------------------------------------------- |
| Native             | `compile_program`            | none or `host.vm` | CSIL is lowered instruction by instruction. A host import may execute specifically bounded residual sites. |
| Standalone native  | `compile_standalone_program` | none              | The native module passed a byte-level audit for zero imports, valid Wasm, and bounded memory.              |
| Interpreter-hosted | `compile_program_to_wasm`    | `host.interpret`  | A fixed wrapper calls the interpreter once. This is universal packaging, not native compilation.          |
| Linked chart       | `compile_chart_wasm_program` | none              | The native module with the interpreter linked in as its host, so its residual sites need no import ([§14](#14-linked-chart-modules)). |

`compile_program_to_wasm` first attempts strict native compilation. If a valid
program has a native blocker, it records that reason as artifact metadata and
emits the hosted wrapper. Invalid or tampered CSIL still fails validation; the
hosted tier never hides a validation error.

The distinction matters to consumers:

- a `.wasm` file can be genuine and valid while still requiring PineWASM's
  host;
- `executionMode: "native"` says the CSIL body was lowered, not that the
  module has zero imports;
- the standalone gate is the supported way to ask whether an unrelated host
  can instantiate the module without PineWASM services.

## 2. Architecture

```text
validated CSIL v2
      |
      +--> strict native analysis --> native CSAW v6 module
      |                                  |-- zero imports, or
      |                                  `-- host.vm for bounded residual sites
      |
      `--> native blocker ----------> hosted Wasm wrapper
                                         `-- host.interpret
```

`pinewasm_aot` emits bytes with `wasm-encoder`. The numeric runtime and,
when needed, the `ta.*` engine are compiled separately from
`crates/pinewasm_aot/runtime` and spliced into the emitted module. The
four checked-in runtime blobs (base, TA, standalone WebAssembly, and standalone WebAssembly-with-broker) are
reproducible build products; CI rebuilds
them with the pinned Rust toolchain and requires a byte-for-byte match.
The [AOT runtime memory-safety validation](aot-runtime-safety.md) document
records its complete source-level unsafe inventory, executable checks, and the
target-specific behavior those checks cannot cover.

The reviewed runtime checksums for this source revision are:

| Runtime artifact | SHA-256 |
| ---------------- | ------- |
| `pinewasm_aot_runtime.wasm` | `d044afc79acd193fbe71d0fc177bfea52c0a1ab72c7f4968b475e6431fba2249` |
| `pinewasm_aot_runtime_standalone_wasm.wasm` | `fda1c0a8ddf4e35ccc429a799f40cb48aa4ff4149e2efd483d8ec82b37984bf6` |
| `pinewasm_aot_runtime_broker.wasm` | `21d9255b27e7a0465dcfb2f5bb5f373d592d28e3c6246d9b54766f9b4f94d66e` |
| `pinewasm_aot_runtime_ta.wasm` | `ad7042309d0bce20783c7aacfc5eafd89ec351bec3a5f2a58db5cd616b62efc6` |

Run `cargo xtask aot-runtime-all` to regenerate all four. Both `cargo xtask ci` and the
dedicated hosted-CI reproducibility job then reject any byte difference from
the tracked artifacts. A runtime-source or toolchain change must update the
binaries and this checksum table in the same reviewed change.

A fifth runtime, the chart runtime behind linked chart modules
([§14](#14-linked-chart-modules)), is built from the same crate and is not
checked in. It holds the interpreter, so almost every compiler change
changes its bytes. `cargo xtask aot-runtime-chart` builds it from the
checkout, by the recipe of the four above under its own Cargo profile
(`chart`, which favors speed where theirs favors size), and the compiler
links it when its build is given the result (`PINEWASM_AOT_CHART_RUNTIME`).
`cargo xtask wasm` does both for the JavaScript package. A compiler built
without it refuses `compile_chart_wasm_program` by name.

There is no mixed interpreter/native operand stack. A native module owns the
bar loop and its typed lanes. A `host.vm` site has an explicit site id and
scratch-cell contract, so only that operation crosses the boundary. A hosted
module delegates the complete run to the interpreter.

Every cell of that contract has a static type. The host returns status 6
when a result does not fit its cell (a string or a heap handle in an `f64`
cell, a float in an `i64` cell). The embedding host must discard the native
result and rerun the original request from fresh state on the interpreter.
The exported `AotHost.vm` returns the status; it does not perform that rerun. A result's cell type is the one its instruction fixes, where it
fixes one: the builtin reads with a fixed runtime type, string symbol reads
such as `syminfo.ticker` and `syminfo.tickerid` included; a field read that
provably holds a heap handle; and a built-in object's field (`chart.point`)
read on a receiver of proven type, from the type's schema. Any other result takes the
type its consumers state, and a collection call states a handle for its
collection operand. A result nothing states defaults to `f64`, which carries
`na` and any float, and an int only as a float.

## 3. Native value model

CSIL validation establishes the type of every stack position, allowing the
native backend to use WebAssembly values and typed memory lanes:

| CSIL value       | Native representation                            | `na` representation |
| ---------------- | ------------------------------------------------ | ------------------- |
| float            | `f64`                                            | NaN                 |
| int              | `i64`                                            | `i64::MIN` sentinel |
| bool             | `i32` Kleene value (`false=0`, `na=1`, `true=2`) | `1`                 |
| color            | packed `i32` RGBT (whole transparency)            | `0xFFFF_FFFF`       |
| string/reference | `i32` handle                                     | `-1`                |

The color row carries exact RGB bytes and a whole transparency percentage,
not selected palette-source identity. A program that reads `input.color`
therefore selects the interpreter-hosted universal artifact; strict native
and standalone entry points report `color input provenance`. Fractional or
unproven transparency also uses the hosted tier. The full interpreter result
retains `paletteIndices`, including independent inputs or fractional constants
that happen to paint identical RGBA. Native constant-color lanes map exact
RGBT values and roles to the compiler palette before paint quantization;
the frontend rejects cached native artifacts with unrepresentable palette
sources. This boundary preserves execution semantics but can reduce native
coverage for color-input programs outside the pinned fixture census.

The integer lane reserves `i64::MIN` for `na` and therefore cannot encode a
present minimum integer in a cell whose execution continues. Static analysis
proves bounded producers safe where possible. Ambiguous producers and checked
arithmetic instead carry minimum guards: a present result equal to the sentinel
stops native execution with status 6 (`ABI_MISMATCH`) before it can be mistaken
for `na`. The embedding chart host must then rerun the original request on
the interpreter, which preserves the full integer domain. Standalone request parsing rejects a minimum
integer parameter before evaluation; it has no interpreter fallback.

Guards cover minimum-capable literals and inputs, checked arithmetic, remaining
float-to-int casts/rounding, timestamps that the float input lane cannot encode
exactly, and integer TA results. Integer `ta.valuewhen` and `fixnan` also guard
fractional captures. Reads of user slots, object fields, and native array cells
reuse the producer guarantees. A producer for which neither a safe domain nor a
sound guard is available still fails closed; linked broker per-trade time reads
remain one such boundary. See local regression tests
for the guard table and focused evidence.

The implementation includes experimental tagged arithmetic and memory helpers
using an explicit `(i64 value, i32 valid)` pair. They distinguish present MIN
from missing values, but the shipped series/input/heap layout still uses the
sentinel. A complete tagged representation would need validity across operand
locals, history, inputs, calls, collections, and the ABI. An aligned payload/tag
cell can occupy 16 bytes rather than the current 8; this is a layout trade-off,
not a measured performance claim. The current guard contract remains necessary
until those paths are integrated and differentially validated.

A Pine `int / int` quotient is statically int but keeps its fraction, which
an `i64` cell cannot hold. The typed pass lowers the division only where no
consumer can tell an exact quotient from an uneven one (see
local regression tests) and refuses
any other use as `UnsupportedOpcode("PineIntDiv")`. The chart tier and the
standalone WebAssembly compile then rewrite the values that quotient reaches
into `f64` code and compile the result; other integers keep their `i64` lane,
and a program the typed pass accepts as written is not rewritten. An `f64` holds
every integer exactly only below 2^53 in magnitude. From there up, the module
checks each integer it widens into that code and each `+`, `-` and `*` on two
whole operands. When the result would lose bits, a standalone module stops
the run with status 15, reported as `Indicator failure: a whole number too
large to hold exactly met the result of dividing two whole numbers`, and a
chart-tier module returns the reroute status 6. The embedding host must
rerun the original request on the interpreter; the native module does not
return a silently rounded result. On the chart tier a program that passes such a value to a `host.vm`
site (text formatting, logging) stays interpreter-hosted.

The emitter preserves checked integer behavior for the admitted subset,
non-finite-to-`na` normalization, three-valued boolean logic, and source-mapped
structured errors. Native heap handles use a private tagged representation;
consumers must not interpret them.

## 4. CSAW ABI v6

A native module exports:

- `memory`;
- `prepare(bars: i32) -> status`;
- `run() -> status`.

The loader calls `prepare`, verifies the CSAW magic and ABI version, writes
market data and resolved inputs, applies supported resource-limit overrides,
then calls `run`. On success it decodes plots, visual channels, fill bands,
and fired alert conditions from output lanes. On failure it maps the header's
status, bar, and CSIL instruction offset back to the structured runtime error
contract.

The linear memory contains a fixed header followed by declaration-dependent
regions for inputs, builtin series, user-series rings, plot values, packed
color roles, the native heap, technical-analysis state, scratch cells, and
alert output. ABI v6 extends the color region with `fill(...)` lanes; a v5
reader would silently omit those lanes, so the version check is mandatory.

Native build metadata also publishes `styleSlots`, the emission planner's
ordered fixed presentation cells (slot identity, typed-cell tag, byte offset
relative to the slot base, and per-slot ring mask). After execution, chart
loaders read these cells at the last completed bar and call the engine's
`AotHost.resolveDisplayStyles`. Import-free numeric modules can need this
host for their final presentation snapshot. The resolver uses the same value
decoder and fixed-style conversion as the interpreter, including the host's
executed string table. Loaders must not reevaluate Pine expressions, guess
ring offsets, or pass unresolved runtime slots into a rendering result.
Empty executions resolve these cells to `na`, as interpreter snapshots do.
This additive build metadata leaves the ABI v6 memory layout unchanged.

Exact offsets, field encodings, layout equations, status codes, export
signatures, and loader requirements are defined in the
[normative specification](spec/csaw-module-abi.md). Keep the following version
numbers separate:

- CSIL version **2** versions the compiler bytecode;
- CSAW ABI version **6** versions the native module contract;

## 5. Host imports and linked runtimes

A current native module imports at most one function:

```wat
(import "host" "vm" (func (param i32 i32) (result i32)))
```

The parameters are `(site_id, bar)`. Operands, results, and the remaining
per-bar instruction budget travel through the artifact's scratch region
according to the emitted host-site plan. Before `run`, scratch cell 0 instead
carries the loader-selected cumulative run budget; `run` copies it to a local
before the first host call. This path covers residual string-content, effect,
request, drawing, or heap operations for which the analyzer did not select a
native plan.

Older `host.ta`, `host.pure`, and `host.alert` imports no longer exist:

- exact numeric functions are linked from `pinewasm_num` through the
  reproducible runtime blob;
- stateful `ta.*` functions are linked from `pinewasm_ta` with state stored
  in the module;
- eligible standalone WebAssembly strategy artifacts link the `pinewasm_broker` strategy broker shared
  with the interpreter;
- fired `alertcondition` events are written to an output lane.

An interpreter-hosted artifact instead imports
`host.interpret() -> status`. It carries no native claim: the host supplies the
complete interpreter run and returns its ordinary `RunResult`.

## 6. Driver and resource contract

The bar loop lives inside a native module. For each bar it initializes or
carries forward user slots, resets the per-bar instruction budget, executes
the compiled CSIL body, and commits output lanes only for completed bars.

Every native module, chart and standalone WebAssembly alike, runs each bar
in an internal function, `bar_step(bar)`, that the loop in `run` calls once
per bar. An engine that switches a WebAssembly function to optimized code
only on its next call (V8 has no on-stack replacement for WebAssembly) would
otherwise run a whole chart in the baseline code of a single `run` call.
`bar_step` is not exported, and it keeps the run counter in a mutable global
that is not exported either; the exports, imports, layout and statuses are
those of §§3–9 of the specification (and of §12 for standalone
WebAssembly). A chart module's `bar_step` is its last function; standalone
WebAssembly's sits just before `evaluate_indicator_cabi`, which stays last.

Every native module, chart and standalone WebAssembly alike, stops the run
through two internal functions instead of writing the error triple inline at
every site: `stop(code, pc, bar)` records `{code, bar, csil_pc}` and returns
`code`, and `budget_stop(pc, budget, cost, bar)` picks the per-bar status
when the per-bar counter is short and the run status otherwise, as the two
separate checks do. A charged instruction outside a window tests both
counters in one branch. Shared stop functions reduce repeated error-path code
without changing the source-mapped status or instruction accounting.

A native module with more than 256 user slots, chart or standalone
WebAssembly, keeps the current-bar cells of the 64 slots its loops use most
in typed locals (each current-bar read and store weighs `16^d`, `d` the
number of loops around it, counted up to four) and the rest in private
mutable globals. A module with at most 256 slots keeps every current-bar cell
in a typed local. Liftoff, V8's baseline compiler, snapshots the typed locals
at each structured control-flow boundary, so the number of locals times the
number of branches bounds its compile memory; the cap keeps a large script's
compile memory bounded by the capped local set. These choices reduce compiler
work for large functions; their effect depends on the host engine and workload.

A module whose per-bar body has more than 1,000 CSIL instructions
can run it as several internal functions, its pieces
(`crates/pinewasm_aot/src/body/split.rs`). V8 optimizes functions individually,
so splitting can make optimized code available for subsequent calls sooner.
The emitter lays
the body out as nested Wasm blocks by walking its dominator tree (§7); a
piece is the code of one block of that tree and every block it dominates,
which control enters only at that block. Such a region becomes a piece
when it is not inside a loop, every branch out of it goes to the same
label, and no operand-stack value crosses into or out of it. Pieces are
chosen bottom-up, moving regions out of any function whose own code would
exceed about 1,000 instructions; a loop always stays whole in one
function, a basic block is never divided, and a region that cannot move
stays in its parent, so some pieces are larger. In straight-line code each
piece holds the rest of the body after its own part, so the calls nest as
a chain. The caller replaces the region's code with a call and then takes
that branch itself. A piece returns 0 when it leaves through its label,
and otherwise the nonzero status of a stop inside it, which each caller
returns unchanged. The instructions, their order, the fuel windows, the
stops and the error triples are the unsplit body's. Between functions,
every current-bar cell lives in its own private global and the two
counters in two more; each piece keeps both counters, and up to 64 of the
cells its own code uses most, in typed locals, and puts them back before a
call and when it leaves. `bar_step` resets the bar's output cells, carries
the slots forward, runs the heap collection check of §13 (so a collection
sees the same roots as in an unsplit bar) and runs the first piece.
For a standalone strategy, the wrapper runs the broker's pre-script phase
before the piece tree and its post-script close pass and equity sample
after the tree returns 0. A nonzero stop returns immediately, unchanged,
without running that post-script phase. A normal CSIL `Return` leaves the
body and still completes the post-script phase.
The pieces sit after the heap helpers and before the stop functions, so
`bar_step` stays last for chart artifacts and just before the consumer
entry point for standalone artifacts. Engine compilation, tiering,
execution, and memory costs need separate measurements; no public speed or
memory result is established by this implementation description. The host
tiering model is described in the [V8 compilation pipeline](https://v8.dev/docs/wasm-compilation-pipeline).

Native execution preserves the relevant VM limits:

- the per-bar instruction budget is stored in the header and charged before
  instruction effects;
- the cumulative instruction budget is loaded from scratch cell 0 and charged
  by the same native pre-effect checks; host-op extras are mirrored into both
  counters, while an ambiguous two-counter host call returns the explicit
  status 10 reroute before effects;
- a native module, chart or standalone WebAssembly, charges a straight-line
  window of instructions with one check
  (`crates/pinewasm_aot/src/body/chart_fuel.rs`). When both counters cover
  the window's summed charge, it subtracts the sum and runs the window
  without further checks; otherwise it runs the same instructions with their
  own checks, so a budget stop happens at the same instruction, after the
  same effects, with the same status and error triple. A window may hold
  native heap reads and writes, pure calls, slot stores, field reads and its
  block's terminator; it holds no `host.vm` call and no instruction whose
  charge is computed at run time, never reads or changes either counter, and
  is left only by falling through or by stopping the run with a nonzero
  status. The checked path keeps the two counters in private globals while
  it runs and charges each instruction with one call to an internal
  `charge(pc, cost, bar)`, which stops through `budget_stop`. The path is
  taken only when a counter is below the window's summed charge, so it always
  ends in a stop (a budget stop, or an earlier instruction's own stop) and
  therefore runs at most once per run. Neither counter is observable once
  `run` returns, so a window changes no result
  ([`chart_fuel_groups_preserve_every_budget_cut`](../crates/pinewasm_aot/tests/differential.rs),
  [`standalone_fuel_windows_preserve_every_budget_cut`](../crates/pinewasm_aot/tests/differential.rs),
  [`every_chart_window_keeps_its_counters_private`](../crates/pinewasm_aot/tests/chart_fuel_groups.rs),
  [`every_standalone_window_keeps_its_counters_private`](../crates/pinewasm_aot/tests/chart_fuel_groups.rs));
- user-series history uses a bounded power-of-two ring per slot derived from
  `max_bars_back` and validated window requirements;
- unlinked native and standalone modules declare a 256 MiB memory maximum;
  linked chart modules declare 1 GiB. A loader verifies the appropriate
  declaration before instantiation;
- the native heap enforces run-wide element and object budgets;
- technical-analysis state and layout arithmetic are checked before a run;
- malformed market data, unsafe numeric options, unsupported cross-script
  inputs, a VM-memory override below the artifact's declared representation
  cap, an oversized projected JS result copy, or an insufficient stack-depth
  override require the host to use the interpreter.

The two engines enforce equivalent finite memory boundaries, not byte-for-byte
allocation identity. The interpreter meters native Rust value/state
representations through `maxVmBytesPerRun`; native AOT is independently
bounded by its declared linear-memory maximum and a host-side result-copy
projection. Consequently, memory exhaustion can occur at a different point
and with a different structured classification on the two tiers. Semantic
results remain the differential contract for runs that complete within both
representations' limits.

Realtime tick replay and instruction profiling are interpreter-only. They
require rollback/`varip` state or per-instruction counters that the native ABI
does not expose.

## 7. Control flow

The backend starts from validated, reducible CSIL control flow. It splits the
instruction stream into basic blocks, checks branch/stack compatibility, and
emits structured Wasm blocks and loops. The CSIL operand stack becomes typed
Wasm locals where a value must survive a branch; straight-line values can stay
on the Wasm value stack.

Irreducible control flow is rejected rather than routed through an unchecked
state machine. The current compiler does not generate irreducible CSIL, but
the guard is required for imported artifacts.

## 8. Native coverage

Native support is decided by the analyzer over the validated program, not by
a marketing list of Pine Script® features. The current backend includes:

- structured control flow and persistent-slot guards;
- float, int, bool, and color operations with `na` behavior;
- builtin/source input lanes and bounded history reads;
- scalar `int`/`float`/`bool` casts, numeric pure functions, and the linked
  technical-analysis engine;
- plot values, style colors, visuals, fill bands, and alert-condition output;
- analyzable collection and UDT operations on the bounded native heap;
- explicit `host.vm` plans for supported residual operations.

On the ordinary chart tier a `strategy()` declaration lowers natively and keeps
the interpreter's broker in the host (`host_broker` in the layout). Orders,
`strategy.*` reads, trade accessors and currency conversions are `host.vm`
sites. Before one runs, the host brings its broker up to the current bar over
the bars the module has finished, in the interpreter's order for a close-only
strategy; after the run it finishes the broker over the executed bars and
returns the interpreter's own strategy results, so the native result ABI still
encodes no broker state. Recalculating after fills, on every historical tick or
only over the last `calc_bars_count` bars changes which executions happen, so a
declaration that asks for one stays interpreter-hosted, and a run override that
asks for one, market data the interpreter rejects as malformed, or a bar of the
run whose closing tick would not show the script the supplied bar (a high or
low that does not bound the bar's other prices, or is missing), makes the host
refuse with that reason: the native run stops at its
first host call with an ABI mismatch, and draining the strategy results fails
the same way. A strategy module with no host site at all has no broker to drain
and stays interpreter-hosted. The narrower standalone WebAssembly export can
lower eligible strategy sources natively by linking the shared broker and
emitting posture events. Other blockers are reported by `AotError` and recorded
as `nativeBlocker` by the universal path.

The reproducible fixture census classifies compiler-valid programs as
native or interpreter-hosted, then separates standalone modules from bounded
`host.vm` residuals and records use of the `ta.*` engine. These are
repository-fixture measurements, not a claim about all Pine Script® v6
programs. Reproduce the current aggregate with:

```sh
cargo xtask aot-census
```

On the current 51 compiler-supported fixtures, 48 compile natively and
3 use the interpreter-hosted tier; 31 of the native modules also pass the
zero-import standalone audit. One hosted fixture is the `strategy()` source
that places no orders and reads no broker state, so its module would have no
host to return strategy results from. `inputs.pine` uses the hosted tier to
preserve color-input identity. `drawings.pine` uses same-position table
replacement, which the strict native AOT tier declines so the interpreter can
preserve table and alias state. The census pins the color-identity and table
replacement fixtures by name as well as the aggregate, so a broader coverage
change cannot hide behind the same total. These figures describe this
repository corpus, not a general measure of language support. The exact fixture
counts are asserted by the test, so an intentional boundary change must update
that assertion and reassess this description. Separate regression gates also
require both tiers to stay exercised and enforce minimum fixture/native
counts.

## 9. Differential validation

The interpreter is PineWASM's internal semantic baseline. Tests compile
eligible fixtures both ways and compare decoded outputs, colors, events, and
structured failures.
The test suite also checks:

- every valid fixture emits Wasm accepted by `wasmparser`;
- standalone bytes have zero imports and bounded memory;
- runtime numeric and `ta.*` functions match the VM;
- native collection/UDT operations respect aliasing and budgets;
- standalone WebAssembly export surfaces and metadata satisfy their byte-level audit;
- the four linked runtime blobs rebuild deterministically;
- ABI mismatches and requests that are ineligible for native execution are
  detected before effects, allowing a host to choose the interpreter.

The hosted forwarding test is intentionally narrower: it proves that the
wrapper calls the interpreter and returns its result. It cannot
prove native-code correctness because the wrapper compiles no script body.

## 10. Host run selection

The compiler reports artifact layout and execution-mode metadata; it does not
silently replace `runProgram` with an AOT loader. An embedding host should use
native code only when the artifact and request make that useful and correct:

- hosted artifacts call the interpreter through `host.interpret`;
- chart-tier strategy modules take their complete broker/report results from
  the host's broker, drained before the host's alerts; a request that broker
  does not reproduce ends in an ABI mismatch; the embedding host must rerun
  the original request on the interpreter;
  eligible standalone WebAssembly strategy artifacts use the linked broker and
  their narrower posture-event result;
- native modules with residual heap traffic stay on the interpreter;
- a native module with other `host.vm` calls is selected only when loop-bound
  native work is expected to amortize the boundary;
- realtime, profiled, invalid-option, and incompatible-input requests stay on
  the interpreter path.

This separates emission coverage from acceleration policy. Exporting a native
module is still useful even when a host chooses the interpreter for a
particular run. Artifact caching, eviction, and worker lifecycle are host
responsibilities.

## 11. Distribution and metadata

`compileProgramToWasm` and `compileStandaloneWasmProgram` return genuine WebAssembly bytes
to the caller. This repository does not define a UI-specific file envelope or
persist artifacts on a caller's behalf. A distributor should record compiler
and source provenance, hash the bytes, and revalidate untrusted CSIL and Wasm
before execution.

The returned bytes are not, by themselves, a notice-complete distribution
bundle. Native artifacts can splice compiled first-party runtime code from
`crates/pinewasm_aot/runtime` (`pinewasm_aot_runtime`), including its standalone WebAssembly
scanner, serializer, arena allocator, and broker binding. That runtime links
`pinewasm_num` and, depending on the selected tier, `pinewasm_ta` and
`pinewasm_broker`. The AOT compiler in `crates/pinewasm_aot/src` also emits the
artifact's glue instruction sequences, tables, and data segments. Parts that
merely represent caller input are distinguished from copied implementation
bytes below. The copied runtime components retain the repository's AGPL-3.0-only
license and can also incorporate permissively licensed dependencies listed in
`THIRD_PARTY_LICENSES.txt`. Do not assume that an
emitted module is license-free merely because it was generated from
caller-supplied source. [Generated-artifact
distribution](artifact-distribution.md) draws the exact boundary between the
bytes that represent the caller's input and the bytes copied from the
runtime, lists the notice bundle, and records the distribution model.

PineWASM distributes artifacts under AGPL-3.0-only: when an artifact is
conveyed, its corresponding source must be made available from the PineWASM
repository at the commit recorded in the artifact notice bundle or through
another AGPL-compliant method; the repository URL and commit identify that
source, and no separate runtime output exception is granted.

Before conveying generated artifacts, review the AGPL obligations for the
concrete distribution and use the license's applicable source-provision method;
include the bundle written by `cargo xtask artifact-notice --out DIR`
(`LICENSE`, `NOTICE`, the runtime-closure license texts, and
`artifact-manifest.json`). Network deployment has a distinct source-access
analysis under the AGPL and should be reviewed for the actual service
architecture. Compiling caller-supplied Pine source with PineWASM does not give
AI Edge LLC any ownership of that source or of the parts of the output that
merely represent it. Processing market data does not either. Commercial use is
permitted under AGPL-3.0-only, subject to that license's conditions;
alternative commercial terms are optional, never a prerequisite. Organizations
that require different terms for portions for which AI Edge LLC holds the
necessary rights can review
[the licensing guide](licensing.md). This is a project packaging
policy and high-level notice, not legal advice.

Every emitted module carries `parameter_schema`; standalone WebAssembly artifacts also carry the
required `module_metadata` section. These JSON custom sections describe
supported numeric/boolean inputs and module identity without making them part
of the hot run ABI.

Both native tiers reserve the maximum legal history for an input-backed
average or change only in that operation's history slots. Unrelated
temporaries retain only the capacity required by their own reads and carry, so a few adjustable
lookbacks do not multiply the maximum window by every variable in a large
script; under one uniform ring, a single adjustable `ta.sma` length in a
script with several hundred variables would exceed the memory maximum. Slot
rings are internal working state (CSAW ABI §5.4), so this is a producer
layout choice: the header's `ring_mask` still publishes the deepest ring, and
the standalone WebAssembly exports, JSON protocol, parameter bounds, and
256 MiB memory maximum are unchanged.

standalone WebAssembly keeps current values in typed Wasm locals. When a slot has no past read,
window reader, or persistent carry, its backing history writes and per-bar
memory clears are omitted. A written-flag store is emitted only when an
initialize-once branch observes it. Local initialization and logical work
charges remain in place. The universal tier continues writing its public
lanes; both tiers retain the existing allocation layout and host protocol.


The schema publishes three types — `int`, `float`, and `bool`. Time and enum
inputs collapse into `int` and price inputs into `float`, giving a numeric
domain a search can range over. Inputs with no representation among the three
are omitted rather than emitted in a shape the format does not define. Standalone WebAssembly
makes one deliberate exception: a finite `input.string(options = [...])` is
published as an `int` over `0..options.length - 1`, with `step: 1` when there
is more than one option. The index is declaration order; option labels are not
added to this fixed schema shape. Unbounded strings stay omitted and cannot
become standalone WebAssembly parameters. Colors are omitted too, and standalone WebAssembly evaluates their declared
default.

## 12. Standalone WebAssembly interfaces

`compile_standalone_program` emits only when the final bytes import nothing,
validate under WebAssembly rules, and declare bounded memory. The audit reads
the bytes rather than trusting the layout flags, so an emitter bug cannot
make a non-standalone module pass by repeating its own incorrect plan.

`compile_standalone_wasm_program` is a separate zero-import consumer profile. It accepts
indicator or strategy programs that either already need no `host.vm` or become
host-independent under the standalone WebAssembly-only projection described below. Indicator
sources may designate a plot as either a reversal-only position-event signal
or a flat-aware posture-event signal. Eligible
`strategy()` sources instead link the shared broker, force the strategy module
type, and derive signals from its bar-close posture; they must not designate a
plot. Both paths emit exactly the standalone WebAssembly export surface:

- `memory`;
- `evaluate_indicator_cabi`;
- `alloc` and `dealloc`;
- `last_error`;
- `reset`.

The standalone WebAssembly audit verifies hardened Wasm features, bounded memory, exact exports
and signatures, metadata size/shape, zero imports, and supported source-input
defaults. The producer runs it on every artifact it emits; there, a runtime
body that is byte for byte the canonical body the compiler build already
validated under the same feature set, in an identical type, function, global,
memory, and data-count prefix, is not validated a second time, while every
section, every program body, and every stub is. The public
`audit_standalone_wasm` validates everything. An indicator classified as a strategy emits `position-events` from
its designated plot by default: transitions into `1` or `-1`, with flat
stretches omitted. A caller may explicitly select `posture-events` for a
designated +1/0/-1 plot, in which case transitions to `0` are serialized and
the module metadata declares that exact contract.
A `strategy()` source emits `posture-events` from the linked broker: transitions
among `1`, `0`, and `-1`, where `0` explicitly closes to flat. Both event forms
use the event bar's close as the wire price. Indicator-type modules emit an
empty signal array.

Native strategy requests may additionally opt into numeric plot and broker
results with `include_results: true`, and supply positive finite symbol price,
quantity, and contract-value increments. The legacy signal document and the
five-export surface remain available unchanged. See the [browser evaluation
contract](browser.md#ahead-of-time-artifacts) for the additional fields.

The same explicit standalone WebAssembly build configuration can raise a native-heap artifact's
`max_collection_elements` above the conservative 100,000 default, up to the
bounded 4,000,000-element browser ceiling. Standalone WebAssembly needs this compile-time
setting because its consumer ABI has no loader hook that can rewrite the heap
budget after `prepare`; ordinary CSAW artifacts keep their loader-controlled
behavior unchanged.

The broker is initialized fresh for every evaluation and follows the same
historical bar phases as the VM: pending fills before the script body,
`process_orders_on_close` after it, then equity and posture sampling. The standalone WebAssembly
path refuses unsupported behavior by feature name rather than approximating
it. Current examples include `calc_on_order_fills`,
`calc_on_every_history_tick`, unsupported currency codes, `calc_bars_count`,
the string-valued per-trade accessors (`entry_id`, `exit_id`, `entry_comment`,
`exit_comment`), currency conversions, and host-dependent operations that
survive projection (for example `str.tostring` labels, tables, and drawings
created inside a loop). Drawing effects the projection can certify are
erased, and `alertcondition` then lowers natively; see the projection
below. The numeric `strategy.opentrades.*` and
`strategy.closedtrades.*` accessors lower natively to the linked broker's
trade view (`pinewasm_rt_broker_trade_f64` / `_i64`) and read the same current
state as the interpreter; the time-valued ones share the `time` builtin's
integer-sentinel gate. `calc_on_every_tick` is accepted but has no effect because
the standalone WebAssembly request format contains historical bars, not realtime ticks.

### The chart timeframe and fixed-timezone calendars

The bars do not say which timeframe they were sampled at. The interpreter
takes it from `MarketData.timeframe`, which defaults to `1D`. A standalone
WebAssembly request names it in a top-level `timeframe` string, in the same
notation (`"240"`, `"1D"`, `"30S"`), in JSON requests and binary metadata
alike. The module then answers these reads itself:

- `timeframe.multiplier` and the `timeframe.is*` predicates
  (`timeframe.isticks` is always false, as in the interpreter, and needs no
  timeframe);
- `timeframe.in_seconds()`, and `timeframe.in_seconds(timeframe.period)` or
  `timeframe.in_seconds(timeframe.main_period)` written out;
- `time_close`, the bar's open time plus the timeframe's nominal length (a
  month is 30 days), as the interpreter computes it without a `timeClose`
  lane, and `timenow`, the last bar's `time_close`;
- `time(timeframe)` over a fixed-width literal, and `timeframe.change`, which
  lowers onto it (below).

The runtime's `standalone_wasm_fill_timeframe` reads the timeframe as the
interpreter does: a bare number is minutes; a trailing `S`, `D`, `W` or `M`,
in either case, is the unit; any other trailing letter means one day; and a
numeral that does not parse counts as one. It accepts ASCII text without
escapes and trims spaces. Like the interpreter, it refuses a timeframe whose
length has no millisecond value and one that puts a bar's close beyond the
`i64` range. A module that reads
the timeframe refuses a request without the field (`missing required field:
timeframe`), because the interpreter's `1D` default would silently mismeasure
an intraday feed, and lists it in `pinewasm_capabilities` as
`"required_fields": ["timeframe"]`. A module that reads no timeframe ignores
the field, so a host can send it with every request. A bar time, in a JSON
request as in binary input, must be a safe integer, at most 2^53 − 1
milliseconds from the epoch: the time lane holds a double, and a served read
never sees a rounded time.

`timeframe.period` itself is a string the module does not have. Its only
standalone use is as the argument of the `timeframe.in_seconds` call right
after it. Any other use that reaches the module's output, such as comparing
it or choosing between it and another string, keeps `host.vm`.

`time(timeframe)` over a fixed-width literal (`time("240")`, `time("2D")`),
with no session, timezone or `bars_back`, is answered by the module too, and
so is `timeframe.change`, which the compiler lowers onto exactly that call.
Like the interpreter's `time`, a site compares the literal's length with the
chart timeframe's: a shorter one reads `na` (and every bar is a change), the
chart's own reads the bar's open time, and only a longer one reads a lane.
That lane is a same-symbol `request.security` lane of a new source, the open
time of each bucket's first bar, grouped as such a lane is: on the UTC clock,
a width under a day from midnight and a wider one from the first bar's day.
Its first read takes the interpreter's one-time charge of a unit per bar;
every spelling of a timeframe (`"D"`, `"1D"`) keeps its own lane, as the
interpreter keeps a cache entry, and a charge, per string. The interpreter
also reserves memory for that cache entry, which the module, like a
`request.security` lane, does not count. A literal that trims to nothing
(`""`, `" "`) is the chart's own period: the bar's open time, with no lane,
no charge and no need for the `timeframe` field. A calendar week or month
follows trading days the request does not carry, so such a
`timeframe.change` or `time` keeps `host.vm`, and a strategy refusal names
`timeframe.change` for the former; so does a fixed width too long for a
lane. The lane assumes the exchange keeps UTC, as a `request.security` lane
does: for another exchange timezone the interpreter's fixed buckets start at
that timezone's midnight instead.

### The exchange clock and session

A module that groups bars into buckets, through a same-symbol
`request.security` lane or a served `time(timeframe)` or
`timeframe.change`, requires the request's `timeframe`, `timezone` and
`session_hours` strings and lists them in `"required_fields"`. They are the market data's
`syminfo.timezone` and `syminfo.session_hours` (`docs/runtime.md`, "Exchange
time zones and sessions"); a host sends `"session_hours":""` when the feed
declares no session. A request without either is refused (`missing required
field: timezone`), as is one whose `session_hours` is not a session
specification.

The module groups bars as the interpreter does, on the exchange's clock, but
carries no time zone database: it reads only a fixed-offset `timezone`
(`pinewasm_tz::parse_fixed`). The decision for each lane is made when the
request fills it, and acted on only when a site reading it executes, as in
the interpreter:

- a lane no longer than the chart's timeframe: each bar is its own bucket,
  whatever the session or timezone, as in the interpreter;
- a fixed offset, with buckets the session settles (`docs/runtime.md`'s
  table, without a `tradingDay` lane, which requests do not carry): grouped
  from local midnight, bar for bar as the interpreter groups them;
- a fixed offset with a session that leaves the buckets' start unsettled:
  status 16, `Indicator failure: where this market's session starts each
  requested bucket is not settled in PineWASM yet` (the interpreter refuses
  the same grouping);
- any other timezone, an IANA name included: status 17, `Indicator failure:
  standalone WebAssembly groups bars only on a fixed-offset exchange clock,
  and this request's timezone is not one`. The interpreter follows such a
  zone; a module refuses rather than group on a clock that may be wrong.
  The module cannot tell an IANA name from an unknown one; a host sends a
  timezone the interpreter accepts (its market-data validation refuses any
  other).

The calendar functions `year`, `month`, `dayofmonth`, `dayofweek`, `hour`,
`minute`, `second` and `weekofyear`, applied to a time in a timezone written
as a string literal (`dayofmonth(time, "UTC")`), call the runtime's
`standalone_wasm_calendar`. It does the interpreter's civil-calendar
arithmetic at the literal's fixed offset, read by the shared
`pinewasm_tz::parse_fixed`: whitespace at either end is trimmed, an
upper-case `UTC` or `GMT` prefix is dropped, and what is left is a sign and
one or two hour digits, four digits `HHMM`, or one or two digits on either
side of a colon. Hours run to 14 and minutes to 59. Anything else keeps
`host.vm`: an IANA name, whose clocks may change and whose rules a module
does not carry, and a name the interpreter would stop on. An `na` time gives
`na`. A calendar call without a timezone
reads the exchange timezone, which the request does not carry, and one whose
timezone is chosen at run time reads a string the module does not have; both
keep `host.vm`.

`timestamp(year, month, day, hour, minute, second)` and
`timestamp(timezone, year, ...)` with a timezone literal of those fixed-offset
forms call the runtime's `standalone_wasm_timestamp`. It does the
interpreter's arithmetic: months carry into the year, days, hours, minutes and
seconds out of range carry through, an `na` field or an intermediate that
leaves `i64` gives `na`, and the timezone form is `na` within two days of the
`i64` limits before it moves the reading by the literal's offset. The literal
need not sit right before the call: the projection follows the stack back
through the call's own block to the `PushStr` that pushed it, so fields
computed at run time (`timestamp("UTC", year(time, "UTC"), 1, 1, 0, 0)`) are
served too. A timezone from any other producer, such as `syminfo.timezone` or
a conditional, and an IANA name keep `host.vm`.

A served call is charged what the interpreter charges for it: one unit, then
one per byte of its string argument after the first, the timezone literal or
the trimmed timeframe. A `timestamp` in UTC has no string argument. An instruction budget therefore runs out at the same
instruction. The interpreter also reserves a string constant's bytes against
the run's string budget each time it pushes one; a standalone module keeps no
string budget, for a timezone literal as for any other constant.

### Resident primary data for sweeps

The optional `pinewasm_capabilities` custom section (schema version 1)
advertises `input_protocols`. A host must check for `PWSWBIN2` and `PWSWREF1`
before using resident data. Existing JSON-v1 and `PWSWBIN1` calls, metadata,
and the five function exports retain their contracts.

Both new packets begin with an eight-byte magic, a little-endian `u32` bar
count at byte 8, a `u32` UTF-8 metadata length at byte 12, and a nonzero
opaque `u64` dataset identifier at byte 16. JSON metadata starts at byte 24,
omits `bars` and `fraction`, and retains the existing version, parameter,
economics and output options and the `timeframe` field. Pad the packet to an eight-byte boundary.
`PWSWBIN2` then appends six little-endian `f64` lanes in open, high, low,
close, volume, time order. `PWSWREF1` ends after the padding: its identifier
and bar count must match the most recent successful resident upload on the
same instance. An identifier is an instance-local lease, not a content hash.

After an upload, reuse the instance without calling `reset`. Allocate and
write each request, evaluate, copy the owned output, then deallocate output
and input in that order. The relocated input/output arena remains above the
primary lanes. Every evaluation still initializes fresh parameters, series,
technical-analysis state, execution limits and broker state. Only immutable
primary data is reused; there is no trial-result or indicator cache.

At most one dataset is resident. A new upload replaces it. Full `reset`, a
legacy input call, a malformed request, or any reported evaluation error
invalidates the lease. A host must discard a trapped instance and terminate
its worker to preempt a synchronous call. It must not mutate resident linear
memory, reuse stale output handles, or retain output views across evaluations.
Release all handles and reset or replace the instance when changing sweeps.
Memory remains bounded by the artifact's existing maximum. This adds protocol
code to standalone WebAssembly artifacts; report module size and cold setup alongside sweep
throughput rather than assuming a single-run improvement.

The fixture census separately reports strategy-type and indicator-type
standalone WebAssembly eligibility under the default signal designation. Reproduce the current
aggregate with:

```sh
cargo xtask aot-census
```

The exact fixture counts are release-gated by the census assertion; an
intentional eligibility change must update that assertion and reassess this
description.

**The standalone WebAssembly-only projection.** standalone WebAssembly has no charting host, so it runs a dedicated
projection before its own analysis and emission. `compile_program` and the
interpreter never consult it, and standard CSAW v6 modules keep
using the PineWASM host for these surfaces.

Finite string-option inputs cross the JSON parameter wire as integer indexes.
Inside the module each validated index maps back to the canonical
constant-pool handle for the declared option, which makes equality and
inequality against canonical string constants native; an invalid index falls
back to the declaration default. Dynamic strings, unbounded string inputs,
ordering or content operations, and any noncanonical string value that can
influence computation keep `host.vm` and make standalone WebAssembly fail closed.

The projection erases a drawing or table output effect, or a formatting,
concatenation, or construction value whose complete use chain ends in such
effects, only when the instruction has a no-failure certificate: it cannot
fail in the interpreter. Erasing a failing instruction would let a standalone
run complete where the VM reports an error. The erasure rests on one
hypothesis: a certified pc never fails in the VM. The certificates below are
about the operands a pc can receive in an admitted run (validated, well-typed, and within the request
bound), so they establish that hypothesis for the VM restricted to admitted
runs; every other request is refused. The certificates
(`projection::certify`) are:

- `DeleteDrawing`, for every drawing kind: an `na` receiver is a no-op, and
  the only other failure is a receiver that is neither `na` nor a reference,
  which the analysis's `Ref` operand typing excludes.
- `RegisterDrawing`, only immediately after a certified drawing `NewObject`
  in the same basic block (the `*.new` lowering): its operand is the fresh
  reference that `NewObject` pushed, and pop-then-push cannot overflow.
- `NewObject` of that constructor pattern, as a program-level certificate.
  The erased site still charges `exec_new_object`'s fuel extra
  (`field_count - 1` after the dispatch unit), so fuel runs out at the same
  bar and pc. For the heap caps, every certified constructor must be off
  every control-flow cycle, so it allocates at most one object of
  `field_count` elements per bar, and nothing else in the program may
  allocate on the VM heap. After `bars` bars at most `bars x sites` objects
  and `bars x sum(field_count)` elements were ever allocated, garbage the
  bar-boundary sweeps have not collected yet included. The artifact's
  bound is therefore
  `max_bars = min(max_collection_elements / sum(field_count), 1,000,000 / sites)`,
  with `max_collection_elements` the build's baked element budget. A build
  whose bound is 0 certifies no constructor.

The bound is published as `max_bars` in the `pinewasm_capabilities` custom
section, and the entry point refuses a request whose executed bar count
(after `fraction`) exceeds it, before any work, with the request error
`request executes N bars, but this artifact's drawing allocation bound admits
at most M (pinewasm_capabilities max_bars)`. Failing closed is what the
soundness relation allows. With the default 100,000-element budget one
`line.new` site (10 fields) admits 10,000 bars; raising
`max_collection_elements` (up to 4,000,000) raises the bound, and the VM run
it stands in for uses the same element budget.

`str.tostring`, string concatenation, and table cells stay uncertified
(string byte limits, `na` receivers, out-of-range cells), as does a
constructor inside a loop, a `line.all`-style registry read, or a drawing
whose value reaches anything but another drawing effect. Such a program keeps
`host.vm` and standalone WebAssembly fails closed. A standalone WebAssembly
consumer receives the signal series, never labels, tables, or drawing
registries.

`alertcondition` joins the `host.vm` plan only when a program needs `host.vm`
anyway. Once the drawing sites are erased it lowers to the native alert lane,
as in indicators. The interpreter records `alertcondition` events for a
strategy too, but they are not part of its strategy outcome, and strategies
ignore `alertcondition`; the standalone strategy output
(posture events, and the opt-in plots and broker results) carries no alerts
either, so the two agree.

Strategy sources take the same projection around the broker. Values reaching
an order opcode are never treated as chart-only, and the broker reads the
projected program's canonical string pool, so the projection cannot change
order identifiers or order behavior.

**The signal slice.** A plot-designated standalone module serializes one plot,
yet everything a script does is typed and lowered, so display code the
importless tier cannot run refuses the whole script. With
`StandaloneWasmBuildConfig::signal_slice` on, a pass before typing
(`slice.rs`) erases every instruction that cannot influence the designated
plot: other plots, bar colors, drawings, tables, alerts and the arithmetic
feeding them become `na` placeholders. A conditional jump is kept whenever
something it decides can reach the plot: a live instruction, or a store to a
slot a live instruction reads, between the jump and the point where its paths
rejoin (its immediate post-dominator). A jump that decides only display code
becomes a plain jump to that point, so a display-only branch or loop is
skipped whole and its condition is erased with it; this is what lets a script
through whose statistics table is rolled by calendar or timeframe calls the
importless tier cannot run. Two checks keep that rewrite closed. The operand
stack must reach the rejoin point holding, in each position, the value it held
right after the jump or one an instruction on the way pushed (the right side
of a lazy `and` is the case in practice); a jump whose paths never rejoin, or
rejoin with the stack rearranged, stays as written. And a `var` slot whose
run-once guard decides anything live keeps every store to it, so a skipped
store cannot change which way such a guard goes. Slot stores and every other
jump are never rewritten, storage is flow-insensitive, heap objects are
grouped into alias classes (a value that provably cannot be a heap handle,
such as a number read from an object field every write to which is a number,
joins none), and `runtime.error`, `math.random` and unknown
opcodes are always live. An input-bound TA length stays in place. The sliced
program is re-validated, and if an indicator's slice does not compile the
whole program is compiled as before. A strategy's slice is its last attempt
(see below), so its refusal is the one reported.

The slice trades the projection's failure equivalence for acceptance, which
is why it is opt-in. With it off (the Rust default) the artifact reproduces
every failure of the interpreter run but one, the cumulative-budget stop
described under the run budget below, and writes every plot lane. With it on,
the designated plot is unchanged wherever the interpreter run succeeds, but a
runtime error raised only by erased code no longer stops the run (a display
loop that never ends included), an erased collection no longer counts against
the heap budget, an erased instruction never costs more instruction budget
than it did, and plot lanes other than the designated one are not written.
`compileStandaloneWasmProgram` turns it on unless the host passes
`signal_slice: false`. Indicator-type modules are never sliced.

A `strategy()` source reports what its broker did and, on request, the value
of every plot, so its slice is anchored to its orders and its plot values
instead of one plot, and every plot lane is still written. Order comments
(`comment` and its per-fill variants) remain live because trade results expose
them. Alert messages reach no output and are erased with the rest. A comment
that needs unsupported dynamic string operations now refuses the standalone
WebAssembly build rather than disappearing from its results. A strategy is
compiled whole first, and from its slice only when the whole program is
refused.

**A strategy's heap.** The linked broker allocates in the arena band while
`run()` executes, and the native heap's own bump region grows towards that
band. A strategy module therefore takes its heap allocations from the arena
allocator as well: one band holds both, and the broker reset at the start of
the next run rewinds it whole, which keeps a resident instance's memory flat.
Like the slice, the heap is offered to a strategy only on the second attempt,
with or without `signal_slice`, so arrays and user-defined types that decide a
strategy's orders no longer refuse it and nothing that built before changes.

**The run budget.** A standalone WebAssembly host cannot write the cumulative
instruction budget into the module, so `prepare` seeds it: one billion
instructions, or one million per bar evaluated, whichever is larger. A flat
ceiling refused a script that stayed inside the per-bar budget on every bar
for the length of its dataset alone, because a script that loops over its own
history does more work per bar the longer the chart is. The per-bar budget is
unchanged, and a host's own fuel and time limits still bound the run. Ordinary
CSAW artifacts keep the flat default, which their loader may overwrite.

This is a deliberate departure from the interpreter run, with the slice on or
off. The interpreter's own cumulative budget (`max_instructions_per_run`)
defaults to one billion whatever the bar count, so on more than 1,000 bars a
script can run to the end in the module where an interpreter run with default
options stops with `RunInstructionLimitExceeded`. A host that compares the two
gives the interpreter the same budget.

**Naming a refusal.** `StandaloneWasmNotStandalone` says only that `host.vm`
would be imported. The refusal `standalone_eligibility` returns names the Pine
calls that need it (`StandaloneRefusal::host_features`), for the same program
the refusal reports on (the signal slice when one applies). When collections
and objects stay on the host only because another call does, the heap
operations are left out, so the list holds causes and not their consequences;
`compileStandaloneWasmProgram` returns it as the refusal's `feature`. The
native heap takes a program whole or not at all, so the list cannot tell an
operation the heap would decline on its own account from one that only follows
another call: while any other call is named, the operations that have a native
lowering are not, and they are named once nothing else is. An argument the
compiler supplied for a call, such as the `timeframe.period` that
`timeframe.in_seconds()` reads for its omitted argument, is named only through
the call, here and in a strategy's `StrategyFeatureUnsupported` alike.

**Same-symbol `request.security`.** Standalone WebAssembly serves a
`request.security` read itself when the request is for the chart's own symbol
over a fixed-width timeframe: a plain price series (`open`, `high`, `low`,
`close`, `volume`, `hl2`, `hlc3`, `ohlc4`, `hlcc4`) or a compound expression.
Each request is a key-table entry of kind 4; after the bars are loaded the
runtime resamples them into one extra lane per request with the interpreter's
same-feed model: buckets on the UTC clock (the module is not
told a timezone, and the interpreter's default is UTC), so a `60` bucket opens
on the hour whatever bar the feed starts on, and the interpreter's visibility
rules for `gaps` and `lookahead` (with gaps
on, a value appears on the bar where the visible bucket changes). A read is a
lane load at the current bar. The lane is rebuilt on every evaluation from the
evaluated prefix, so `fraction` behaves as a run over those bars alone.
Identical plain requests share a lane.

As in the interpreter, a request is settled when it first executes. Each lane
has a status cell in the scratch region: the runtime leaves either the
resample's one-time budget charge (one unit per bar for a plain series; for a
compound one, the expression's work plus two per bar and one unit per node) or
a negated run status, and the first read charges or stops accordingly. A
request behind a branch that is never taken therefore costs nothing and cannot
fail the run.

The timeframe may be a literal, the chart's own period (`timeframe.period` or
`""`), or a string input. A string input with declared options resolves on
every evaluation from the option index in the input table, through a
per-option seconds table in the entry's payload; an out-of-range index falls
back to the declared default, and executing a request whose selected option
the module cannot serve (a calendar week or month, or a timeframe beyond the
interpreter's hundred-year ceiling) stops the run as `Indicator failure: this
request.security timeframe is not available in standalone WebAssembly`. A
free-text timeframe input has no wire lane, so the module runs its declared
default.

A compound expression (`request.security(sym, tf, ta.ema(close, 20))`) is
encoded as a postfix program in the entry's payload, which is counted in
8-byte words. The runtime evaluates it once per bucket with its own state: an
EMA smoother per ta slot, and one scratch lane per ta slot and per history
slot holding that slot's value per bucket. It mirrors the interpreter's
`eval_htf` node for node for `ta.ema`, `ta.sma`, `ta.change`, history reads,
arithmetic and comparisons, including `na` propagation, a non-finite result
reading as `na`, and input-bound lengths read from the input table. An
`input.source` inside the expression reads its declared default, as everywhere
in standalone WebAssembly.

A request carries the chart's own bars and none of the other feeds or clocks
the interpreter's host can supply, which leaves these documented differences
from the interpreter:

- `request.security` does not use the request's `timeframe`. For the chart's
  own period every bar is its own bucket, where the interpreter walks
  period-wide buckets from the first bar; the two agree wherever no two bars
  open within one period of each other. And the interpreter's error for a
  request below the chart timeframe cannot be raised: such a request
  resamples into its own narrower buckets, which on regular bars reads the
  chart bar itself.
- Host-supplied request clocks and feeds (`request_time_close`,
  `request_bars`) do not exist, so confirmation is never deferred past a
  bucket's last supplied bar.
- The interpreter's resample-cache memory reservation is not modeled; the
  lanes are part of the artifact's fixed per-bar layout.
- Plain requests share a lane, and its one-time charge, only when they are
  written identically. The interpreter shares by resolved timeframe, so the
  same series requested once with a literal timeframe and once through an
  input (or the chart period) that resolves to it is billed once there and
  once per spelling here: one extra unit per bar.
- The runtime fills every request lane before the run; only the budget charge
  waits for the first read. A request that never executes, or one whose charge
  the budget then refuses, has therefore already done its resample, so the
  instruction budget does not bound that work. It is bounded by the bar count
  times the expression's ta lengths (each at most 100000, at most 16 slots).

These keep `host.vm` and fail closed: another symbol's feed; a calendar month
(it needs the exchange timezone); a timeframe computed by an input expression;
a boolean-valued expression, `barstate.*` in the secondary context, a history
offset computed from inputs, and `ta.atr`, `ta.hma` or `ta.vwma` there;
expressions needing more than 16 ta slots, 16 history slots or 32 stack cells;
and a literal `NaN` constant.

## 13. Native heap and run selection

The native heap is the collection/UDT acceleration path. Supported object and
collection sites use typed regions in the artifact's linear memory, preserve
handle aliasing, and charge the same run-wide element/object limits exposed in
the CSAW header. A heap failure returns a reroute status. The embedding host
must rerun the original request on the interpreter to reproduce the exact
structured error instead of exposing an AOT-only failure shape.

Array elements use one of four lanes: `float` (`f64`), `int` (`i64`), and
`bool` and `color`, which both store the value's native `i32`. Native
collection calls are the constructors (`array.new_float`/`new_int`/`new_bool`/
`new_color`, `array.from`), `push`, `get`, `set`, `size`, `shift`, `pop`,
`first`, `last`, `clear`, and the aggregates `sum`, `avg`, `min`, `max`,
`stdev` and `variance`. The aggregates need an `int` or `float` lane; they skip
`na` and add in element order, as the interpreter does. The heap is all or
nothing: one collection call outside this list keeps every heap operation of
the program on `host.vm`.

A typed constructor gives its array a lane at once. An `array.from` array's
lane is the type of its arguments, which only the typed pass knows, so the
first element pass leaves it open, and an open lane admits only `size` and
`clear`. When that first gate fails, the analyzer takes each `array.from`
site's lane from the typed pass and runs the element pass and the typed pass
again with those lanes. It keeps the second analysis only when the second
typed pass gives every site the same lane and the native plan then passes;
otherwise the program keeps its first analysis, unchanged.

The interpreter collects its heap at each bar boundary, just after the slot
preamble, when its arena or its live elements have reached a threshold, and
returns the budget of every object nothing can reach. A native-heap module
does the same when its analysis has a collector plan (`analysis::GcPlan`). It
counts the interpreter's arena (one object per native allocation and per
hosted one) and keeps both thresholds as the interpreter does. Each record
ends in a `{mark, link}` trailer: a collection marks every native record
reachable from the slot cells the interpreter roots, the current one and as
many bars back as any instruction reads the slot, tracing objects through
their reference fields. It sends every host reference it meets to the host
through the two reserved `host.vm` sites (docs/spec/csaw-module-abi.md
§7.1); the host collects its own heap from those and its drawing registry and
reports what stays live. The budgets become the limits minus what is live on
both heaps, so the module's counts equal the interpreter's at every
instruction and it hands a run back exactly where the interpreter stops. A
collection also lowers the bump pointer to the end of the highest record
still reached, so temporaries allocated after the live records are reused
(a strategy's heap shares its arena band with the broker and keeps its
memory). Memory below the highest live record is not reused, so a module that
keeps placing live records above its garbage can still fill its linear
memory, which hands the run back like any memory failure (§6). A program
whose analysis cannot count
exactly (a cell or field that can hold references of more than one kind, a
native reference stored in a host object, `*.all`, or a reference slot whose
history depth depends on an input) gets no collector: its counts then only
ever run ahead of the interpreter's, and a spent budget hands the run back
(docs/runtime.md).

Some programs still have heap operations that analysis cannot bind to a
native plan; those sites use `host.vm` and set `vmHeapResidual`. A host should
not run that artifact natively. When all heap work is native but other
host sites remain, `nativeHeapHot` records whether enough sites are inside
natural loops to justify the boundary. This is a performance policy only:
the artifact remains valid and differentially tested either way.

### Compact strategy objectives

Strategy-source standalone WebAssembly modules advertise `retained-objectives-v1` in the optional
`pinewasm_capabilities.output_projections` array. Set `objective_results: true`
in request metadata to receive `{version: 1, projection: "retained-objectives-v1",
bars, firstRetainedTradeIndex, metrics}`. Do not combine it with enabled
`include_results` or `binary_results`. The flag defaults to false on every call.

The nine numeric metrics are `initialCapital`, `netProfit`, `grossProfit`,
`grossLoss` (positive loss magnitude), `closedTrades`, `totalCommission`,
`finalEquity`, `positionSize`, and `maxDrawdown`. Trade totals accumulate the
trade ledger in order, matching projection of the full output.
Drawdown is peak-to-trough absolute drawdown of bar-close equity, starting at
initial capital; it is not the broker's intrabar drawdown metric. The trade
retention offset is explicit because these totals need not equal lifetime
trade totals after the retention window advances. Missing or non-finite
completed report data refuses this projection rather than inventing a score.

This path still executes the complete program, indicators, plot writes and
broker, retaining histories and all state required by subsequent instructions.
It avoids constructing unused extended report aggregates, copying the trade
ledger into a result vector, serializing full plots/trades/equity and scanning
posture events. It is a report projection, not dead-code elimination or a
change to fill semantics. Compare its metrics against full reports on the same
parameters and rerun the selected parameter sets with full results. Indicator-source
modules do not advertise this projection. The existing full and legacy output
contracts remain unchanged.

## 14. Linked chart modules

`compile_chart_wasm_program` emits a chart module that imports nothing. It is
the native module of §§3–9 with one difference: the interpreter that such a
module reaches through its `host.vm` import is linked into the module, and
each host site calls it directly. The instruction a site executes, the
handler that executes it, the scratch-cell contract and the statuses are
those of §5. Only the boundary moved, from a WebAssembly import to a call
inside the module. With its host inside it, a module also leaves the stops
of its own code to that host (Limits and errors, below).

The linked host is the chart runtime: the runtime crate built with its
`chart` feature, which adds `pinewasm_vm`'s `VmHost` to the numeric and
`ta.*` families (`runtime/src/chart.rs`). It is spliced whole, with its
table and element sections, because it calls through a table and the splice's
direct-call graph does not describe such calls.

### Exports and load sequence

A linked chart module exports `memory`, `prepare`, `run`, and three entry
points for its loader:

| Export                           | Use                                                                                         |
| -------------------------------- | ------------------------------------------------------------------------------------------- |
| `chartAlloc(len) -> address`     | An 8-aligned block for the request. 0 on failure, and before `prepare`.                    |
| `chartInit(address, len) -> status` | Build the host from the request. Nonzero declines the request.                           |
| `chartCommand(op, a, b, c) -> i64` | Read one part of the host's result as JSON: `(length << 32) \| address`, valid until the next call. |

A loader runs a module once, in this order:

1. Instantiate with no imports, and call `prepare(bars)`.
2. Write the market data lanes, the inputs it owns, and the limits, as for
   any native module (§4).
3. Write the request as JSON into a `chartAlloc` block and call `chartInit`.
   The JavaScript package builds it with `chartRequestJson(data, runOptions,
   bars)`: `JSON.stringify` would write a `Map`-backed option as `{}`, and a
   non-finite primary sample as `null`. `data` and `runOptions` are the values
   `runProgram` takes. The host is built here, and here it writes the cells
   of the inputs it owns (below).
4. Call `run()`.
5. Read plot lanes from memory, and everything else through `chartCommand`:
   the structured error of a status 5, the reason for a status 6, the logs,
   the alerts, the strategy results (before the alerts, as with `AotHost`),
   the drawings, the resolved display styles, and each color lane. The alert
   command covers both kinds of module: one with host sites sends its alerts
   through the host, and one without writes them to the alert lane of §4,
   which the command then returns.

The entry points hold a loader to that order, because the module's memory
depends on it. The linked host is ordinary Rust, and its allocator takes
memory with `memory.grow`, above the layout `prepare` made. So `prepare`
succeeds once and returns status 6 after that; `chartAlloc`, `chartInit` and
`chartCommand` do nothing before `prepare`; `chartInit` succeeds once; and
`run` returns status 6 unless `chartInit` succeeded, and after its first
call, which leaves the first run's results as they were. A loader that needs
a second run instantiates the module again. The compiled
`WebAssembly.Module` is what a host caches.

`chartInit` first validates `data` with the same `MarketData::validate`
check used by the interpreter and incremental sessions. It then declines,
with status 6 and its reason as command 9, a request that lies outside the
module's memory, whose `bars` differ from `prepare`'s or from
`min(maxBars, data bars)`, a `realtime` or `profile` run, a `maxStackDepth`
below the program's stack depth, a `maxVmBytesPerRun` below the default, a
non-finite external-source sample, options it cannot read, and a strategy the native bar loop cannot follow (§8). A declined request,
like every status 6, is the embedding host's to rerun on the interpreter.

### What the linked host adds

A linked host is present before the run, holds state a packed cell cannot,
and is the only reader of its module's slot memory. That removes three
reasons a program leaves the native tier or calls its host, and one cost of
every bar:

- **Inputs.** A string or color input is the same on every bar. `chartInit`
  resolves each one as the hosted `ReadInput` does and writes its cell into
  the input table, and the module reads it like a numeric input. The loader
  leaves those cells to the host.
- **Strings.** The linked host gives every run string one handle: a string a
  hosted instruction produces takes the handle of the table entry with the
  same text. The compiler's constant pool never holds a text twice, so two
  handles are equal exactly when their strings are, and the module compares
  them itself. `na` compares unequal, as in the interpreter. A program whose
  pool does repeat a text (possible only for a program not from this
  compiler) keeps its string comparisons as host sites.
- **Colors.** A color that fits the packed cell of §3 stays there. One that
  does not (a fractional transparency, or a color read from a color input,
  whose identity selects the palette entry) lives in the host's run-local
  color table, and its cell holds a handle: the low byte is 254, a value no
  transparency has, and the upper bits index the table. A program that
  needs such colors has its color constructors, channel reads and color
  comparisons as host sites, and compiles natively where `compile_program`
  refuses it (`fractional color transparency`, `unproven color
  transparency`, `color input provenance`). A program whose colors all fit
  keeps them native, as in §3. The loader reads a color lane through
  `chartCommand`, which returns the lane's distinct colors as the
  interpreter writes them, palette entries included, and an index of which
  one each bar shows.

  Few calls at such a site meet a full color. So at a channel read,
  `color.new` and `color.rgb` the module first tests the operands it has.
  When every color among them is a packed cell or `na`, and the
  transparency the result takes is a whole percentage (a float among +0, 1,
  ..., 100, or any integer, which the clamp to 0..=100 makes whole), it runs
  the packed lowering of §3, which returns the cell the host would. A
  handle takes the call to the host, and so does a float transparency that
  is fractional, a negative zero, outside 0..=100 or `na`.
  `color.from_gradient`, a color comparison and a fractional color constant
  always do.
- **Slot lanes.** A module that imports its host writes every slot's cell of
  every bar. A linked module writes a slot's lane only when something reads
  it back: the slot carries its value to the next bar, an instruction reads
  its history or a window over it (an input-sized average or change
  included), or the linked host reads it to resolve a display style. It
  keeps a slot's written flag only for a slot an initialize-once branch
  tests. That is every lane a standalone module keeps, and the style slots.
  Nothing else addresses a linked module's slot lanes: its host reaches the
  module through the scratch cells, and its loader reads the plot lanes and
  asks `chartCommand` for the rest. The current value of every slot is
  unaffected. In a script with thousands of temporaries this is most of
  what a bar's entry does.

A `strategy()` program's broker is the linked host's, as in §8, whether or
not the program has a host site.

### Limits and errors

A linked module stops a run where the interpreter stops it, with the
interpreter's error, or hands the run back. Every error of a bar is status
5, and `chartCommand(0)` returns the error. The loader builds none itself,
and statuses 1, 4 and 9 do not leave a linked module. The module's own code
still detects those stops, at the instruction where the interpreter does.
Its stop function then asks the linked host what the stop becomes. The
statuses a run returns before its first bar (8 and 12, §9 of the
specification) are what they were.

- **Instruction budgets.** A module hands back every per-bar or per-run
  budget stop its own code detects, with status 6, and `chartCommand(1)`
  gives the reason. Two things make such a stop the interpreter's to report.
  The interpreter does not always report it at the instruction that spent
  the budget: when the compiler's optimizer deleted instructions, the
  program carries the code it had before (`budget_reference`), the
  interpreter runs that code too after such a stop and, when that run stops
  on the same bar with the same results, reports the source location where
  it stopped (`pinewasm_vm::run`). Only a second run finds that location.
  And the error depends on the order in which an instruction makes its own
  checks before it charges for its work, which is the interpreter's to
  define: a copy of a missing object is reported as the missing object even
  where the bar's budget would not have covered the copy. A hosted
  instruction that spends the bar's budget runs the interpreter's own code,
  so its error stands, with status 5, unless the program carries that
  earlier code, and then it is handed back as well.
- **History offsets.** A history offset computed at run time that passes
  `max_bars_back` stops the run with status 5. The module leaves the offset
  in scratch cell 1, and the error names it and the limit, as the
  interpreter's does. One offset is handed back with status 6 instead. The
  interpreter reads a fractional offset (the value of a kept `int / int`
  quotient, §3) as a number of bars up to 2^64, and the module's cell holds
  one below 2^63. An offset between the two is past every `max_bars_back`,
  and only the interpreter can name it.
- **String budget.** The interpreter reserves bytes of
  `maxStringBytesPerRun` at three instructions a module runs itself. A
  string constant reserves its length each time it is pushed, an
  `alertcondition` that fires reserves its message and the 29 bytes of its
  frequency name, and a copied object reserves its string fields. No host
  call reports these, so a host that counted alone would let a module
  finish runs the interpreter stops. A module with one of these
  instructions therefore keeps what is left of the budget in a private
  global. `run` takes the figure from the host, the module subtracts the
  same bytes at the same instructions, and each host call takes the figure
  in and returns what its own instruction left. The count is then the
  interpreter's at every instruction. A reservation that fits costs that
  subtraction and one test, on each pass of a loop that pushes a string
  constant. One that does not fit is made by the host, with the
  interpreter's own code, and the run stops with status 5 and the
  interpreter's `string_limit_exceeded`: the same bar, the same instruction
  and the same attempted total. The largest budget a request can name,
  every byte the host's `usize` counts, is no budget: the interpreter's sum
  saturates there, and so does the module's.

The VM memory budget is not kept this way. The two engines bound memory
separately (§6), so a module can stop on `maxVmBytesPerRun` on another bar
than the interpreter does, and a host that needs the interpreter's error
for such a stop reruns the request there.

### What stays the same

The header is the CSAW v6 header and carries version 6. A loader that does
not know the class cannot misread one: without `chartInit`, `run` returns
status 6.

The runs a module hands back are those of §5 and §13, and the instruction
budget stops above. A value that does not fit the cell its site was typed
with stops the run with status 6, and a spent host heap budget with status
7; the embedding host reruns the request on the interpreter. Realtime tick
replay and instruction profiling remain interpreter-only (§6).

A module's native heap takes its blocks from the linked host's allocator, as
a standalone strategy module's takes them from its broker's (§13). Those
blocks are not returned during a run, and neither are the entries of the
host's color table (24 bytes for each full color a bar derives, which its
color lanes refer to until the loader reads them). So a module that
allocates on every bar grows with the length of the chart, up to the 256 MiB
maximum, where it hands the run back like any memory failure (§6).

The linked host runs on a shadow stack of one megabyte that `prepare` places
above the runtime's data, where an overflow would meet that data rather than
a trap. A guard word at the stack's low end is checked each time a call into
the host returns; a run that reached it traps there.

### When a linked module pays

A linked module replaces an import with a call, not with compiled code: a
host site that does call its host still runs one instruction on the
interpreter. That call costs more than the interpreter's own dispatch of
the instruction, and every module carries the whole chart runtime, about
2.5 MiB that the compiler splices and audits and the engine compiles on
first use. So a linked module is faster than the interpreter where compiled
code carries the run, and slower where host instructions do or where the
run is too short to repay its setup. An embedding host chooses per program;
`hostSites`, `hostSitesInLoops` and `nativeHeapHot` on the compile result
are there for it. A host site outside every loop runs at most once a bar,
so a module with none in a loop calls its host at most `hostSites` times a
bar, whatever its loops do.

The first run of a module also pays for the splice, the audit, the engine's
compile of the runtime and the construction of the host, and a short run does
not earn that back. No timings are stated here. The repository has no
accepted benchmark result for this tier, and a performance claim follows the
[benchmark methodology](benchmarks.md).

`tests/chart_wasm.rs` runs every fixture and a set of targeted
programs as linked chart modules in an interpreter with no imports to offer,
and requires each run to equal `pinewasm_vm::run`'s: plot lanes bit for bit,
and color series, styles, alerts, logs, drawings and strategy results as the
same wire values. `cargo xtask chart-test` builds the chart runtime and runs
it. Of the 50 fixtures, all compile as linked chart modules; one
(`drawings.pine`) hands its run back with status 6, as it does when it
imports its host.

The same suite sweeps the limits. It runs ten programs under the string
budgets from none to one byte more than the run reserves (every one of them
where the run reserves at most 2,048 bytes, and a spread of them otherwise),
every fixture under three string budgets, two programs under a string budget
and an instruction budget together, and two programs under a range of
per-bar and per-run instruction budgets, with and without their unoptimized
form. Under each limit a run the interpreter finishes must finish with the
same results, and a run the interpreter stops must either be handed back or
stop on the same bar, at the same instruction, with the same error and the
same results before it. No run is handed back for its string budget.
`cargo xtask wasm-test` runs one more linked module from the built
JavaScript package in V8, against that package's interpreter, under no limit
and under string and instruction budgets that stop it.
