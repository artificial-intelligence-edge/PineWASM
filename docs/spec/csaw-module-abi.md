# CSAW Module ABI — version 6

**Status:** draft, tracking ABI version 6. This is a pre-1.0
specification; it may change together with the PineWASM implementation until
it is declared stable (§13).

**License:** this specification is licensed under the Apache License,
Version 2.0 (see [`LICENSE`](./LICENSE)). The PineWASM implementation — the
rest of this repository — is licensed under the GNU AGPL-3.0-only;
nothing in this document grants rights to that implementation.

**PineWASM implementation:** `crates/pinewasm_aot` is the producer;
`crates/pinewasm_bindings` exposes produced bytes and the residual-operation
`AotHost` to JavaScript consumers. This repository does not bundle an
application loader or define a distributable file envelope. Where this
document and the producer disagree within the same release, that is a
defect to resolve rather than permission for consumers to guess. Pre-1.0
development may change both together; consumers must match the declared
ABI version and the documentation for the exact release they embed.

## 1. Scope and execution tiers

CSAW—the **Compiled Series Artifact for WebAssembly** format, identified by
the ASCII header magic `CSAW`—is the binary contract for per-script
WebAssembly modules emitted by PineWASM from validated CSIL programs. A
standard CSAW module is an ordinary 32-bit WebAssembly module with one bounded
linear memory and the three exports in §3.

There are two execution tiers:

- A **native module** runs the complete bar loop in WebAssembly. Most native
  modules have no imports. A module that still needs `pinewasm_vm` behavior
  may import the single function `host.vm` (§7.1).
- An **interpreter-hosted module** is a WebAssembly packaging wrapper. Its
  `run` export calls `host.interpret`, and the host runs the embedded program
  (or the exact corresponding program supplied out of band) with a compatible
  `pinewasm_vm`. Its linear memory and native header are unused (§7.3).

The tiers have the same standard export names but different driver
protocols. Consumers MUST select a tier from trusted build metadata before
instantiation. They MUST NOT infer the tier merely from whether an import
section exists: a native module can import `host.vm`, while a hosted module
always imports `host.interpret`.

A native module with no imports is **standalone**. A native module importing
`host.vm`, and every hosted module, requires a compatible CSAW host.

This specification covers the standard CSAW `memory` / `prepare` / `run`
profile. A different adapter may reuse the internal CSAW layout while
publishing another export surface; such an adapter is not a standard CSAW
module unless it also satisfies §3.

## 2. Terminology and conventions

The producer's `styleSlots` build metadata describes compiler-owned
fixed rendering scalars: `{slot, tag, offset, mask}`. `offset` is relative to
the header's `slots_base`, and the last completed bar selects the ring cell
with `(bars_executed - 1) & mask`. Every cell has an 8-byte stride; i32-backed
tags use only the low four bytes, which the consumer must sign extend rather
than read as an i64. These cells are read-only consumer data. The matching
`AotHost.resolveDisplayStyles` converts them through the engine's typed
decoder and executed string table; no second evaluation of the script is performed.
An import-free module can still require this host for fixed style decoding.
The metadata and host method are additive to ABI v6 and do not change the
header or execution exports. A loader must fail or select a classified
compatible execution path when required style metadata or decoding is absent.

The key words MUST, MUST NOT, REQUIRED, SHALL, SHOULD, SHOULD NOT,
RECOMMENDED, MAY, and OPTIONAL are to be interpreted as described in BCP 14
(RFC 2119 and RFC 8174) when they appear in all capitals.

- **Producer** — software that emits a CSAW module.
- **Consumer** (also **host**, **loader**, or **driver**) — software that
  instantiates and drives a module.
- **Artifact reader** — software that parses a distributable `.wasm` file
  without instantiating it.
- **Bar** — one row of the market-data feed. Bars and instruction offsets
  are zero-based.
- **na** — the language's “not available” value. Each typed domain has a
  designated encoding (§6).
- **Linked runtime** — producer-supplied WebAssembly code and data spliced
  into a native module. It is neither imported nor exported (§7.2).
- **Build metadata** — emit-time facts that are not repeated in the native
  header. At minimum, a driver needs the execution tier, ABI version, input
  declarations, source-input lane order, slot and plot counts, color-role
  counts, alert-site count, scratch-cell count, and import requirements. A
  `host.vm` implementation additionally needs the host-site plan (§7.1).

All multi-byte values in linear memory are little-endian. `u32`, `u64`,
`i32`, `i64`, and `f64` have their usual fixed-width meanings. Byte offsets
are hexadecimal. `align8(x)` means `(x + 7) & ~7`.

Consumers MUST treat the module and all artifact metadata as untrusted
input. Build metadata used to drive a module MUST describe those exact
module bytes; metadata from a different compilation is not interchangeable.

## 3. Standard module contract

A standard CSAW v6 module MUST export exactly these names:

| export    | kind     | signature                                   | meaning                         |
| --------- | -------- | ------------------------------------------- | ------------------------------- |
| `memory`  | memory   | wasm32, minimum 1 page, maximum 4,096 pages | the module's sole linear memory |
| `prepare` | function | `(i32 bars) -> i32 status`                  | initialize a run                |
| `run`     | function | `() -> i32 status`                          | execute the run                 |

The memory maximum is 256 MiB (4,096 × 64 KiB). A host MAY enforce a lower
runtime limit, in which case `memory.grow` can fail cleanly with status 2.
The module MUST NOT import memory, a table, or a global.

A native module MAY import exactly one function:

```wat
(import "host" "vm" (func (param i32 i32) (result i32)))
```

An interpreter-hosted module MUST instead import exactly:

```wat
(import "host" "interpret" (func (result i32)))
```

The obsolete `host.ta`, `host.pure`, and `host.alert` imports are not part of
ABI v6. A module declaring any of them does not conform to v6.

Custom sections do not alter the export contract (§11).

## 4. Native header

The first `0x60` (96) bytes of native-module memory are the header. A
successful native `prepare` fills it; the consumer writes the option cells
identified below; `run` reports through it.

| offset | size | type | field            | writer                          | meaning                                               |
| -----: | ---: | ---- | ---------------- | ------------------------------- | ----------------------------------------------------- |
| `0x00` |    4 | u32  | `magic`          | `prepare`                       | `0x57415343`, the bytes `CSAW` read little-endian     |
| `0x04` |    4 | u32  | `abi_version`    | `prepare`                       | `6`                                                   |
| `0x08` |    4 | u32  | `bars`           | `prepare`                       | the unsigned bar count                                |
| `0x0C` |    4 | u32  | `ring_mask`      | `prepare`                       | deepest slot ring `- 1` (§5.4)                        |
| `0x10` |    4 | u32  | `builtin_base`   | `prepare`                       | first builtin f64 lane                                |
| `0x14` |    4 | u32  | `slots_base`     | `prepare`                       | first user-slot ring                                  |
| `0x18` |    4 | u32  | `plots_base`     | `prepare`                       | first plot f64 lane                                   |
| `0x1C` |    4 | u32  | `heap_end`       | `prepare`                       | first byte after the fixed run layout (§5.10)         |
| `0x20` |    4 | u32  | `err_code`       | `prepare`, `run`                | last recorded run status; `prepare` writes 0          |
| `0x24` |    4 | u32  | `err_bar`        | failing `run`                   | failing bar, when defined (§9)                        |
| `0x28` |    4 | u32  | `err_pc`         | failing `run`                   | failing program instruction offset, when defined (§9) |
| `0x2C` |    4 | u32  | `colors_base`    | `prepare`                       | first packed-color lane                               |
| `0x30` |    8 | u64  | `budget_per_bar` | `prepare`, consumer             | per-bar instruction budget; default 64,000,000        |
| `0x38` |    8 | f64  | `mintick`        | `prepare`, consumer             | minimum price increment; default `0.01`               |
| `0x40` |    4 | u32  | `scratch_base`   | `prepare`                       | first scratch cell (§5.8)                             |
| `0x44` |    4 | —    | reserved         | —                               | consumers MUST ignore                                 |
| `0x48` |    8 | u64  | `max_elements`   | native-heap `prepare`, consumer | native-heap live-element budget; default 100,000      |
| `0x50` |    8 | u64  | `max_objects`    | native-heap `prepare`, consumer | native-heap object budget; default 1,000,000          |
| `0x58` |    4 | u32  | `alerts_base`    | `prepare`                       | first alert-lane entry (§5.9)                         |
| `0x5C` |    4 | u32  | `alert_count`    | `prepare`, `run`                | entries committed by the current run                  |

Rules:

- After native `prepare` succeeds, a consumer MUST verify both `magic` and
  `abi_version` and refuse an exact-version mismatch.
- Consumers MUST read every published base from the header. They MUST NOT
  assume that `builtin_base` immediately follows the input table; linked
  runtime data can occupy the intervening address range (§5.1).
- The consumer MAY overwrite `budget_per_bar`, `mintick`, `max_elements`,
  and `max_objects` after `prepare` and before `run`. It MUST NOT modify any
  other header field.
- A zero budget is a real limit. The first charged instruction on bar 0
  fails; zero does not mean “unlimited”.
- `err_bar` and `err_pc` are not initialized by `prepare`. They are valid
  only for statuses that §9 says record the error triple.
- This header does not apply to the hosted tier. Hosted `prepare` does not
  write magic or version, and a hosted driver MUST NOT inspect this header.

## 5. Native linear-memory layout

Let:

- `n_inputs` be the number of input declarations;
- `n_source` be the number of `input.source` declarations;
- `n_slots` and `n_plots` be the user-slot and plot counts;
- `slot_bytes` be the producer-internal size of the slot-ring region (§5.4);
- `n_color` be the sum of every color role on every plot, visual, and fill,
  in the order specified in §5.7;
- `scratch_cells` be the emit-time scratch count, at least 4;
- `alert_sites` be the native alert-lane site count; and
- `ta_bytes` be the linked technical-analysis engine's fixed state-region
  size, or zero when no such engine is linked.

The fixed layout is:

```text
header          0x00 .. 0x60
input table     0x60 .. 0x60 + 8*n_inputs
runtime prefix  producer-internal, if present; never overlaps header/table

builtin_base    max(0x60 + 8*n_inputs, lane_floor)
builtin lanes   (6 + n_source) * 8*bars bytes

slots_base      builtin_base + (6 + n_source)*8*bars
slot rings      slot_bytes bytes

written_base    slots_base + slot_bytes
written flags   n_slots bytes

plots_base      align8(written_base + n_slots)
plot lanes      n_plots * 8*bars bytes

colors_base     plots_base + n_plots*8*bars
color lanes     n_color * 4*bars bytes

scratch_base    align8(colors_base + n_color*4*bars)
scratch         scratch_cells * 8 bytes

alerts_base     scratch_base + scratch_cells*8
alert lane      alert_sites * bars * 8 bytes

ta_base         alerts_base + alert_sites*bars*8
TA state        ta_bytes bytes

heap_end        ta_base + ta_bytes
```

`lane_floor` is zero when no linked runtime exists. With a linked runtime it
is the first byte after the runtime's fixed low-memory reservation and any
producer-linked declaration tables, rounded as required by those tables.
The reservation can contain read-only numeric data, a shadow stack, and
technical-analysis declarations. It is module-internal. The consumer MUST
use `builtin_base`, not attempt to discover or reproduce `lane_floor`.

All published 8-byte regions are 8-aligned and color lanes are 4-aligned.
`written_base` and `ta_base` are intentionally not published; they are
module-internal facts included here to define the formula chain.

### 5.1 Input table and runtime prefix

The input table always begins at `0x60`, even when a linked runtime moves the
lanes upward. Each declaration gets one 8-byte cell (§6.1). A producer that
links the numeric runtime MUST place its fixed runtime data high
enough that the supported input table cannot overlap it; the current
producer rejects more than 500 inputs for such a module.

Only the input-table bytes in this low-memory area are consumer-writable.
Everything between the end of the table and `builtin_base` is
producer-internal and MUST NOT be modified.

### 5.2 Builtin lanes

Six f64 lanes contain `bars` samples each, in this fixed order:

| lane | series |
| ---: | ------ |
|    0 | open   |
|    1 | high   |
|    2 | low    |
|    3 | close  |
|    4 | volume |
|    5 | time   |

Lane `k` begins at `builtin_base + k*8*bars`. Price and volume samples are
finite f64 values or NaN for na. Consumers MUST normalize non-finite source
values to NaN.

Time is epoch milliseconds represented as f64. If a feed has no time lane,
the consumer synthesizes daily bars beginning at
`1,577,836,800,000` (2020-01-01T00:00:00Z):

```text
time[i] = 1,577,836,800,000 + 86,400,000*i
```

### 5.3 Source-input lanes

One additional f64 lane follows the six builtins for each `input.source`
declaration. Source lane `j` begins at
`builtin_base + (6 + j)*8*bars`. The source-input order is emit-time build
metadata; it is not derivable from the header alone.

The consumer resolves each source selector to a concrete series and writes
the complete lane. The standard native loader supports open, high, low,
close, hl2, hlc3, ohlc4, and hlcc4. A cross-script external source is outside
the native contract and requires the interpreter.

### 5.4 User-slot rings and written flags

Each user series slot has one ring of `ring(s)` 8-byte cells. `ring(s)` is a
power of two large enough for the program's declared history requirement and
for every history access and window over slot `s`; a slot read by an
input-backed window is sized for that input's largest legal value. Rings are
laid out in slot order, so the cell for slot `s` and bar `b` is:

```text
slots_base + 8*(ring(0) + ... + ring(s-1)) + (b & (ring(s) - 1))*8
```

`slot_bytes` is the sum of `8*ring(s)` over every slot. The header publishes
`ring_mask` as the deepest ring minus one; it is informational and does not
locate any other slot's cells.

Float and integer values occupy all 8 bytes. Bool3, color, string, and
reference values occupy the low 4 bytes; the high 4 bytes are padding and
MUST NOT be interpreted by a consumer.

One byte per slot at `written_base + s` records whether the program has
written that slot during the run. `prepare` zeroes these bytes. Rings and
flags are internal working state; consumers MUST NOT write or read them as
output.

### 5.5 Plot lanes

Plot `p` has one f64 output lane beginning at
`plots_base + p*8*bars`. A cell is at `+ 8*b`. NaN means na. At the start of
each bar, `run` initializes that bar's cell in every plot lane to NaN.

The plot count and declaration metadata are build metadata, not header
fields.

### 5.6 Visual outputs

Visual declarations do not have separate numeric lanes. Their observable
per-bar data in this ABI consists of their color roles in the color region.
Their titles, styles, and other declaration properties come from build
metadata or the embedded program.

### 5.7 Color lanes, including fills

Each declared color role gets one i32 lane of `bars` cells. Color lane `k`
begins at `colors_base + k*4*bars`. The exact flat order in ABI v6 is:

1. every plot in plot-slot order, with that plot's roles in declaration
   order;
2. every visual in visual-slot order, with that visual's roles in
   declaration order; then
3. every `fill(...)` band in fill-slot order, with that fill's roles in
   declaration order.

Fills form the third family. A fill has no numeric output lane;
the consumer combines its declaration's two plot endpoints with its color
lanes. Each color cell uses §6.4. `run` initializes every current-bar color
cell to `-1` (na) before executing the script body.

### 5.8 Scratch region

The scratch region has `scratch_cells` 8-byte cells and is at least 32 bytes
(4 cells). Its size is build metadata. It is shared, at non-overlapping
times, by the cumulative-budget handoff, linked technical-analysis tuple
returns, and `host.vm` calls.

`prepare` writes the default cumulative run budget (1,000,000,000) as a u64
in cell 0. The consumer MAY overwrite it after `prepare` and before `run`.
A standalone WebAssembly artifact has no consumer that can reach the cell, so
its `prepare` writes the larger of that default and 1,000,000 times its bar
count.
`run` copies it into an internal local before any other scratch use. This is
an additive convention: it consumes no header bytes, and a consumer that does
not override the cell receives the producer's safe default.

During `host.vm`, cell 0 carries the remaining per-bar u64 budget into and out
of the call. Cells 1 onward carry operands in argument order, deepest stack
operand first. On success, a single result replaces cell 1. Section 7.1
defines the static cell tags and host protocol.

The linked technical-analysis engine may put tuple elements after the first
result in consecutive 8-byte scratch cells beginning at `scratch_base`; the
module consumes them immediately. Consumers MUST otherwise treat scratch
contents as transient.

### 5.9 Alert lane

Each alert-lane entry is 8 bytes:

| entry offset | type | meaning                                |
| -----------: | ---- | -------------------------------------- |
|         `+0` | u32  | bar index                              |
|         `+4` | i32  | message string id; `-1` means na/empty |

Entries are appended in execution order. `alert_count` is the number of
committed entries. Capacity is exactly `bars * alert_sites`; emission only
uses the lane when each declared site can fire at most once per bar.

When a native module has any `host.vm` sites, `alert_sites` is zero and the
native lane remains empty. Alert conditions then join the host-site plan so
that alert conditions, `alert()`, logs, drawings, and other effects retain a
single execution order in host state. The consumer obtains that combined
event stream from the compatible VM host, not from linear memory.

### 5.10 Linked TA state and native heap

A module using natively lowered `ta.*` calls links a stateful engine. Its
fixed declaration and state regions are internal; only their size affects
`heap_end`. `run` reinitializes linked TA state on every invocation.

For a module with a native collection/object heap, `heap_end` is also the
heap's initial bump-allocation address. The heap may call `memory.grow`
during `run`, so `heap_end` remains the end of the **fixed** layout rather
than the final memory high-water mark. Array records and object fields are
internal and are not a consumer-facing interchange format.

Because `memory.grow` can replace a JavaScript `WebAssembly.Memory` buffer,
a JavaScript consumer MUST reacquire its `DataView` and typed-array views
after `run` before reading any result.

## 6. Value and input encodings

Native lanes and scratch cells are statically typed. There is no dynamic tag
stored beside ordinary lane values.

### 6.1 Input-table cells

Cells are ordered by input declaration index:

| input kind   | 8-byte cell encoding                                                          |
| ------------ | ----------------------------------------------------------------------------- |
| Float, Price | f64; NaN = na                                                                 |
| Int, Time    | exact signed i64                                                              |
| Enum         | exact signed i64 member index                                                 |
| Bool         | f64 exactly `0.0` for false or `1.0` for true                                 |
| Source       | unused; the value comes from a source lane; the consumer writes NaN |

String and Color inputs are not natively representable in v6; a program that
needs them uses the hosted tier.

Bool encoding is strict. Native code decodes the cell as `cell != 0.0`, so a
NaN cell would decode as true. A conforming consumer MUST write exactly
`0.0` or `1.0`.

### 6.2 Float

Float values are f64. Any NaN bit pattern means na. Native arithmetic
normalizes non-finite results to NaN where the language's value conversion
does so; consumers MUST use `isNaN`, not a single canonical NaN bit pattern,
when decoding output.

### 6.3 Integer

Integer values are signed i64. `i64::MIN`
(`0x8000_0000_0000_0000`) is the na sentinel. Native checked arithmetic
produces na instead of trapping on overflow, division by zero, and the
`i64::MIN / -1` overflow case. The sentinel means the exact mathematical
value `i64::MIN` cannot be distinguished from na in an ABI cell.

### 6.4 Bool3

Boolean series use i32 Kleene encoding:

| value | encoding |
| ----- | -------: |
| false |        0 |
| na    |        1 |
| true  |        2 |

Values outside this set are invalid.

### 6.5 Color

Colors use packed `rgbt` in one i32/u32 cell:

```text
(r << 24) | (g << 16) | (b << 8) | t
```

`r`, `g`, and `b` are 0–255. `t` is transparency from 0 through 100.
`0xFFFF_FFFF` (i32 `-1`) is na.

### 6.6 String and reference handles

String and reference domains use i32 handles; `-1` is na.

For a `host.vm` scratch cell, string handles index the host's run string
table. Constant-pool strings occupy their original ids and dynamically
created strings are appended. Host-managed reference handles identify the
compatible VM host's arena. Native-heap references instead identify
module-internal linear-memory records. A producer's static plan MUST prevent
handles from one ownership domain from being interpreted as the other.

## 7. Imports and runtime linking

### 7.1 `host.vm` — native VM sites

The only native import is:

```wat
(func $host.vm (param $site_id i32) (param $bar i32) (result i32))
```

`site_id` indexes an emit-time host-site plan in ascending program-instruction
order. Each plan entry contains:

- the program instruction offset;
- operand cell tags, in deepest-first argument order; and
- an optional result cell tag.

Cell tags are:

| tag | domain    | cell representation                                    |
| --: | --------- | ------------------------------------------------------ |
|   0 | f64       | raw IEEE-754 bits; NaN = na                            |
|   1 | i64       | signed integer; `i64::MIN` = na                        |
|   2 | bool3     | 0 / 1 / 2                                              |
|   3 | color     | packed rgbt in low 32 bits; `-1` = na                  |
|   4 | string    | run string-table handle; `-1` = na                     |
|   5 | reference | host arena handle; `-1` = na                           |
|   6 | discard   | result only; the result is dead and the host writes na |

The tags are not stored in the scratch region or header. A compatible host
MUST obtain the exact site plan from trusted build metadata or reproduce the
producer's analysis of the exact embedded program.

For a call, the module writes remaining per-bar budget to scratch cell 0 and operands
to cells 1 onward, then invokes `host.vm(site_id, bar)`. On success the host
writes updated remaining budget to cell 0 and, when present, the result to
cell 1, then returns 0. The module charges the dispatch before calling; the
host charges only handler-specific extra work. The module derives that extra
from the returned per-bar delta and subtracts it from the cumulative local.
If the cumulative counter is smaller than the per-bar counter at a host site,
the module returns status 10 before effects because one v6 scratch counter
cannot communicate both ceilings without ambiguity.

The defined host failure statuses are:

- 5 `HOST_ERROR`: the host has a structured runtime error available out of
  band; and
- 6 `ABI_MISMATCH`: a dynamic runtime value did not fit the static cell plan,
  so the request must be rerun by the full interpreter.

The module records any nonzero host return as the run's error status, bar,
and instruction offset, then returns it unchanged.

Two `site_id` values are reserved for a module's heap collection (§9): they
index no plan entry, and neither reads or writes cell 0.

- `0x7FFF_FFFE` (roots): cell 1 holds a count `n`, and cells `2 .. 2 + n`
  hold host references the module keeps, as unsigned 32-bit values. The host
  keeps them as roots of the next collection.
- `0x7FFF_FFFF` (collect): the host collects its heap from those roots plus
  its own drawing registry and table cells, forgets the roots, and writes its
  live element count to cell 1 and its live object count to cell 2.

A module calls them only at a bar boundary, before the bar's first site. A
host without them answers 6, so the embedding consumer MUST rerun the
original request on the interpreter.

The compatible host also owns strings, requests, logs, drawings, host-routed
collections/objects, and the combined alert stream used by such a module.
Those results are transferred out of band; CSAW v6 does not serialize them
into linear memory.

A `strategy()` module on this tier (`host_broker` in its build metadata) also
leaves its broker to the host: orders, `strategy.*` reads, trade accessors and
currency conversions are `host.vm` sites. Before such a site runs, the host
brings the `pinewasm_vm` broker up to `bar` over the bars the module has
finished, in the `pinewasm_vm` order for a close-only strategy. After a
successful run the host finishes the broker over the executed bars and returns
the `pinewasm_vm` strategy results out of band. A host that cannot reproduce
a request this way (recalculation after fills or on every past tick, a
`calc_bars_count` window, market data `pinewasm_vm` rejects as malformed,
or a bar among those the host was built for whose closing tick would not show
the supplied prices) returns 6 from every call and refuses to return strategy
results, so the consumer reruns the request on the interpreter.

Consequently, a module importing `host.vm` is not independently portable
despite doing most of its work natively.

### 7.2 Linked numeric and technical-analysis runtimes

Transcendental math, two-argument rounding, floating remainder, and any
required support routines are linked into the module. Stateful `ta.*`
functions are likewise linked with their state and declaration tables. These
functions are internal implementation details: they are not imports, not
standard exports, and their function ids and internal memory records are not
part of CSAW v6.

Linking can reserve low linear-memory addresses, which is why consumers must
honor `builtin_base` (§5.1). A conforming producer MUST keep the header and
input table disjoint from linked data and MUST include linked state in the
fixed-layout calculation.

### 7.3 `host.interpret` — hosted tier

The hosted import is:

```wat
(func $host.interpret (result i32))
```

A hosted module has one-page initial memory with the same 4,096-page declared
maximum, but does not use the native header or lane layout. Its
`prepare(bars)` returns 0 without inspecting `bars` or writing memory. Its
`run()` calls `host.interpret` exactly once and returns that call's status.

The host validates and executes the exact corresponding program with the
requested market data and options, returning the complete result out of band.
The compiler API binds that program separately; a host that persists a hosted
module MUST retain a cryptographically bound, validated CSIL program alongside
it. A completed `pinewasm_vm` invocation returns status 0; `run`
propagates any nonzero host status unchanged. A host without a compatible
interpreter MUST treat the module as a data wrapper and MUST NOT claim it is
independently executable.

Program-specific custom sections mean complete hosted module bytes need not
be identical across programs even though the executable wrapper body is
program-independent.

## 8. Driver protocol

### 8.1 Native preflight

Before selecting native execution, the consumer MUST validate the request
constraints that the module does not reproduce. These include consistent
market-data lengths, finite positive symbol increments, a valid time axis,
the requested operand-stack limit, supported source selections, and any
execution mode the native result shape cannot represent (including realtime
replay, and strategy broker results from a module without a host broker).

When exact structured validation errors are required, a request that fails
preflight MUST be routed to the interpreter rather than partially
driven through native memory.

### 8.2 Native `prepare`

Call `prepare(bars)` before any input or lane write. The parameter is compared
as an unsigned i32 against a per-module maximum derived during emission from
the exact layout: input table and runtime lane floor, builtin/source lanes,
slot history, written flags, plot and color lanes, scratch, alert entries, and
the linked TA region. The compiler admits the largest bar count whose exact
region ends fit the module's declared 4096-page memory32 limit, including
page-rounding arithmetic. Standalone modules also require the arena relocation
target (fixed regions, per-bar regions, alignment slack, and its safety pad) to
fit that limit. A negative i32 is interpreted as an unsigned value and, like
any count above the derived maximum, returns status 3 before layout arithmetic
or memory growth.

On success, `prepare`:

1. computes the §5 fixed layout;
2. grows memory to cover `heap_end`;
3. writes the header and initializes linked-runtime pointers;
4. defaults budget and mintick and, when a native heap exists, its limits;
5. zeroes the user-slot written flags; and
6. returns 0.

The consumer MUST stop on a nonzero status. Header contents are not valid
after a failed `prepare`.

### 8.3 Native host writes

After a successful `prepare` and header verification, the consumer writes:

1. every scalar input-table cell;
2. all six builtin lanes;
3. every resolved source-input lane; and
4. the requested budget, mintick, maximum live-element count, and maximum
   object count.

The defaults are 64,000,000 instructions per bar, mintick `0.01`,
100,000 live collection elements, and 1,000,000 heap objects. A consumer MAY
retain a default by writing that value explicitly.

### 8.4 Native `run`

`run()` executes `header.bars` bars. At each bar it reloads the budget,
initializes the current plot and color output cells to na, carries persistent
slots from the prior bar (na on bar 0), initializes non-persistent slots to
na, and executes the program.

Linked TA state, native heap state, and alert count are rearmed for the run.
Success returns 0 and writes `err_code = 0`. The first failure stops execution
and returns its status (§9).

### 8.5 Native read-back

After `run`, reacquire memory views if memory could have grown. Read bases
again from the header. Let:

```text
bars_executed = bars       when status == 0
bars_executed = err_bar    for a recorded run failure
```

Only values at bars `< bars_executed` are observable output. Decode plot NaN
as null/na, color `-1` as null/na, and reconstruct plot, visual, and fill
records from build metadata in the exact lane order of §5.7.

If `host.vm` is absent, read `alert_count` entries from the alert lane. If it
is present, use the host's combined alert/log/drawing streams instead. A
host-broker strategy module's results come from the host too: take them first,
with the `run` status and `bars_executed`, since finishing the broker can add
fill alerts to that stream. A run that returned `TA_INIT_FAILED` (8) or
`INVALID_TA_LENGTH` (12) stopped before its first bar and has no strategy
results, as the interpreter reports none for a run it rejects before its first
bar; any other failure leaves the broker on its error bar.

### 8.6 Instance reuse

A consumer MAY call `prepare` and `run` again on the same native instance.
`prepare` recomputes bases and clears written flags; `run` resets the alert
count, linked TA state, and native heap pointer. The consumer MUST rewrite all
inputs and data lanes and MUST reset any companion `host.vm` state before the
next run.

A consumer may cache the compiled `WebAssembly.Module`; creating a fresh
instance per request is the simplest way to isolate memory and host state.

## 9. Status and error protocol

The complete v6 status namespace is:

| code | name                  | source           | meaning                                                                   |
| ---: | --------------------- | ---------------- | ------------------------------------------------------------------------- |
|    0 | `OK`                  | both tiers       | success                                                                   |
|    1 | `BUDGET_EXCEEDED`     | native `run`     | insufficient per-bar instruction budget                                   |
|    2 | `ALLOC_FAILED`        | native `prepare` | fixed-layout `memory.grow` failed                                         |
|    3 | `TOO_MANY_BARS`       | native `prepare` | unsigned `bars` exceeded this module's exact memory32 layout bound         |
|    4 | `HISTORY_LIMIT`       | native `run`     | dynamic history offset exceeded `max_bars_back`                           |
|    5 | `HOST_ERROR`          | `host.vm`        | host produced a structured runtime error out of band                      |
|    6 | `ABI_MISMATCH`        | `host.vm`, native `run` | dynamic value did not fit its static cell: a `host.vm` scratch cell, or a native integer cell that would hold a present `i64::MIN` |
|    7 | `HEAP_REROUTE`        | native `run`, `host.vm` | native heap hit a condition whose exact error requires interpreter replay, or a heap budget was spent on the module's counters or on the host's |
|    8 | `TA_INIT_FAILED`      | native `run`     | linked TA declaration/state initialization failed                         |
|    9 | `RUN_BUDGET_EXCEEDED` | native `run`     | insufficient cumulative instruction budget                                |
|   10 | `RUN_BUDGET_REROUTE`  | native `run`     | host site needs interpreter replay to enforce both counters exactly       |
|   11 | `BROKER_FAILED`       | native standalone WebAssembly `run`  | linked broker reported a named configuration, risk, or capacity failure   |
|   12 | `INVALID_TA_LENGTH`   | native `run`     | an input-backed TA parameter is outside its legal range                   |
|   13 | `SECURITY_TIMEFRAME_UNAVAILABLE` | native standalone WebAssembly `run` | an executed `request.security` selected a timeframe the module cannot resample |
|   14 | `SECURITY_LENGTH_OUT_OF_RANGE`   | native standalone WebAssembly `run` | an executed compound `request.security` has a `ta.*` length outside 1..=100000 |
|   15 | `INT_FRACTION_RANGE`  | native standalone WebAssembly `run`  | a whole number `f64` cannot hold exactly met a kept `int / int` quotient   |

For statuses 1, 4, 5, 6, 7, 9, and 10, `run` writes the returned status to
`err_code`, the failing bar to `err_bar`, and the failing program instruction
offset to `err_pc`. The instruction offset is opaque to a generic consumer;
a compatible host may map it through the embedded program's source map.

Status 8 is a module-integrity failure: a correctly emitted module's linked
engine validates the declaration table produced alongside it. The current v6
module returns 8 before writing the error triple, so consumers MUST NOT read
`err_bar` or `err_pc` for this status.

Status 11 is used only inside a standalone WebAssembly strategy artifact. The linked broker writes
its precise message to the standalone WebAssembly error slot before `run` returns; the standalone WebAssembly entry
point preserves that message. Generic CSAW chart-tier consumers do not receive
this status.

Status 12 is returned by `run` before the bar loop when an input-backed TA
parameter (for example a length supplied through `input.int`) is outside the
range the validated binding declares. `run` writes 12 to `err_code` only; it
does not write `err_bar` or `err_pc`, so consumers MUST NOT read them for this
status. It corresponds to the interpreter's `RuntimeErrorKind::InvalidTaLength`;
the standalone WebAssembly glue reports it as "Indicator failure: TA input
length out of range".

Inside a standalone WebAssembly artifact, `run` also returns 12 from the bar
loop, when a `ta.ema` call whose length is computed at run time (for example
`ta.ema(close, n + 1)`) executes with a length outside 1..=100000. The glue
reports it with the same message. Generic CSAW chart-tier consumers receive 12
only before the bar loop: a chart-tier module does not lower that call
natively.

Statuses 13, 14, and 15 are used only inside a standalone WebAssembly
artifact; generic CSAW chart-tier consumers do not receive them. Each writes
the status, bar, and instruction offset like status 1, and the standalone
WebAssembly glue reports a fixed message.

- 13: the artifact serves a same-symbol `request.security` from its own bars.
  A request whose selected timeframe it cannot resample (a calendar week or
  month chosen through an input, or one beyond the interpreter's ceiling)
  stops the run when it first executes: "Indicator failure: this
  request.security timeframe is not available in standalone WebAssembly".
- 14: a compound request's input-bound `ta.*` length is out of range when the
  request first executes: "Indicator failure: request.security length is out
  of range".
- 15: the artifact carries a kept `int / int` quotient, and the integers that
  meet it, as `f64`. A whole value that `f64` cannot hold exactly (possible
  only from 2^53 up) stops the run rather than return a number the
  interpreter would not: "Indicator failure: a whole number too large to hold
  exactly met the result of dividing two whole numbers".

Statuses 6, 7, and 10 are request reroute signals, not final user errors. The
embedding consumer MUST discard the native result and rerun the original
request from fresh state on the interpreter. The exported
`AotHost.vm` adapter returns the status; it does not perform that replay. For status 5,
it takes the host's structured error. For statuses 1 and 4, it constructs the
corresponding source-mapped runtime error.

A spent heap budget is always status 7 and never status 5. The interpreter
has one heap, collects unreachable objects at bar boundaries, and budgets the
live ones. The module's element and object counters are charged for native
allocations and for each hosted object allocation (a drawing constructor or
copy, and `*.all`), so they cover both heaps. A module whose producer planned
a collector collects both heaps where and when the interpreter does (the
`host.vm` half through the reserved sites of §7.1), so its counters equal the
interpreter's live counts and status 7 marks the bar and instruction where
the interpreter stops. Any other module counts every object it ever
allocated, so its counters are never behind the interpreter's; when a counter
or the host's own budget runs out, the interpreter may still have room, so the
request is replayed there and the interpreter decides.

Native code returns status 6 from an integer minimum guard. The native integer
cell spells `na` as `i64::MIN`, so it cannot hold that value as a present
integer. Where the producer's static analysis cannot rule the value out, the
emitted operation checks its own result: if the result is `i64::MIN` and is
not the operation's `na` (an `na` operand, a checked overflow, or a float
outside the integer range), `run` stops with status 6 before the value is
stored or observed. A module whose integer operations are all statically
cleared contains no such check. A chart-tier native module that runs a kept
`int / int` quotient as `f64` code also returns status 6, when a whole number
that code would have to hold has no exact `f64` image (possible from 2^53
up); a standalone WebAssembly artifact stops with status 15 in the same
case. A standalone WebAssembly artifact has no
interpreter to reroute to; its entry point reports the status as the
evaluation error `unexpected status 6`.

Effects already committed before a failure remain in memory or host state.
The `bars_executed` truncation excludes every output cell on the failing bar,
while already committed host-side effects keep their language semantics.

The hosted tier returns whatever `host.interpret` returns. Its error details
are entirely out of band and its native header is undefined.

## 10. Metering

A native run starts with the u64 cumulative budget from scratch cell 0 and
starts every bar with the u64 `budget_per_bar`. Charges occur immediately
before effects. If the remaining per-bar budget is less than a charge, the
instruction does not execute and status 1 records its program offset; if only
the cumulative counter is short, status 9 records the same source location.

A producer MAY check a straight-line sequence of instructions once: when
both counters cover the sequence's summed charge, it subtracts the sum and
runs the sequence without further checks. No instruction in the sequence may
read or change either counter, and the sequence may be left only by falling
through or by `run` returning a nonzero status. When either counter is short, the
producer charges each instruction as above. Neither counter is observable
once `run` returns, so statuses, the error triple, `bars_executed` and output
are those of charging each instruction. The rule holds for both artifact
classes: the PineWASM producer applies it to chart modules and to standalone
WebAssembly modules, whose per-instruction path may keep the two counters
in private globals while it runs.

The base cost is one unit per program instruction. Window mean charges
`max(length, 1)`. A linked TA call charges that function's declared engine
cost, at least one. The module charges a `host.vm` dispatch against both
counters, then the host charges additional work through the per-bar scratch
counter and the module mirrors its delta into the cumulative counter. Native collection operations can charge
runtime linear work; when reproducing the exact mid-operation error requires
the interpreter, insufficient budget returns status 7 for full replay.

The hosted wrapper does no metering because the interpreter host meters the
actual execution.

## 11. Custom sections and distribution

Custom sections do not participate in WebAssembly linking and do not change
the §3 execution contract.

The producer appends UTF-8 JSON custom sections named `parameter_schema` and
`module_metadata` to both tiers. They describe the program's searchable scalar
parameters and proposed module identity. A CSAW consumer that does not
implement those adjunct schemas MAY ignore them.

This specification does not define a UI, file envelope, persistence format, or
artifact registry. A distributor SHOULD bind module bytes to compiler and
source provenance, record a SHA-256 digest, and retain the exact validated CSIL
program whenever the hosted tier requires it. A hosted-module hash attests to
the emitted wrapper and metadata sections, not to native lowering of the
corresponding program.

Artifact files are untrusted. Consumers SHOULD validate Wasm, enforce the
import/export allow-list and bounded-memory declaration, verify trusted
metadata, and apply execution resource limits before instantiation. Re-emitting
bytes from validated CSIL with a trusted compiler is the strongest supported
path when provenance is uncertain.

## 12. Conformance

### 12.1 Producer

A conforming standard producer MUST:

1. emit valid wasm32 with the exact exports and bounded memory in §3;
2. choose one tier and declare only that tier's allowed import;
3. for native modules, write the §4 header, reproduce the §5 region order,
   and use the §6 encodings;
4. make required emit-time metadata available with the exact emitted bytes;
5. implement the driver, status, and metering behavior in §§8–10 without
   trapping on inputs admitted by the protocol; and
6. label the module ABI version as 6 in trusted build metadata and, for
   native modules, in the prepared header.

A producer that cannot lower a valid program natively MAY emit the hosted
wrapper, but MUST package enough program information for its compatible host
to execute it.

### 12.2 Consumer

A conforming consumer MUST:

1. validate the module and enforce the exact import/export allow-list;
2. select the tier from trusted metadata and provide its required host;
3. for native modules, call `prepare`, stop on error, verify magic and exact
   ABI version, then perform only the writes allowed by §§4 and 8;
4. use header bases and trusted build metadata rather than infer
   producer-internal runtime placement;
5. honor status-specific reroute/error behavior and `bars_executed` output
   truncation;
6. refresh memory views after possible growth; and
7. enforce market-data, run-option, memory, and execution resource limits
   before accepting untrusted work.

## 13. Versioning and stability

ABI versions are exact, not ranges. A consumer MUST NOT drive a native module
whose prepared `abi_version` differs from the version it implements.

The current ABI version is 6. This document freezes the complete ABI 6
contract; any further breaking execution-contract change requires a new ABI
number.

This specification is pre-1.0. Until declared stable, a breaking change to
the header, memory layout, value encodings, imports, exports, statuses, or
driver protocol MUST receive a new ABI number. Once a version is declared
stable, its contract is frozen.

The embedded CSIL program representation is not specified here. Its
`csil_version` (serialized as `il_version` in CSIL v2), validation rules,
instruction set, source map, and constant pool are a separate, evolving
contract. Generic consumers MUST treat that payload as opaque. In particular,
`err_pc`, host-site plans, and string ids do not turn the program
representation into part of the public CSAW instruction set.

To propose a change, open an issue or pull request describing the change, its
compatibility impact, the required ABI version bump, and the corresponding
implementation update.
