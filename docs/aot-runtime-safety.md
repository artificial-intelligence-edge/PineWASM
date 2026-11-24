# AOT runtime memory-safety validation

The linked AOT runtime is intentionally excluded from the root Cargo workspace:
it is a `no_std` `wasm32-unknown-unknown` module whose checked-in binaries are
spliced into emitted artifacts. That target boundary determines which tools can
make an honest claim about its safety.

This document separates three complementary checks:

1. a syntax-tree inventory makes every explicit unsafe block, unsafe function,
   unsafe implementation, unsafe symbol/linkage attribute, mutable static, and
   foreign block reviewable;
2. Miri interprets the pure Rust numeric, technical-analysis, and broker code
   that the VM and linked runtime share verbatim, plus the native standalone WebAssembly harness;
   and
3. AddressSanitizer instruments native builds of that shared code plus a native
   harness around the standalone WebAssembly scanner, serializer state machine, and allocation/range
   arithmetic. The harness calls the production functions; it does not copy
   their algorithms into test-only code.

The inventory scans ordinary Rust modules under `runtime/src`. It rejects
`include!` and `#[path]` source redirection, including conditional attributes
and macro transcribers, rather than silently omitting an external source file.
A function-like macro name supplied by a metavariable is also rejected when
it could name `include!`; use a literal, reviewable macro name in this runtime.
Imports of `include!` (including aliases) and opaque macro-generated imports
are rejected for the same reason; keep runtime imports explicit and reviewable.
Raw identifier spellings retain the same source-inclusion and linkage-attribute
checks, including conditional attributes and macro transcribers.
Macro qualifier checks distinguish fragment kinds that can supply a bare
`unsafe` token from expression and visibility fragments; unknown shapes remain
conservative review sites. This is source-level inspection, not macro expansion.

The dedicated toolchain is pinned in
[`runtime-safety-toolchain.toml`](../.github/runtime-safety-toolchain.toml).
The entry points resolve that toolchain's exact Cargo binary and discard
inherited compiler flags and Miri sysroot/source overrides before applying the
reviewed Miri or sanitizer flags. In particular, an inherited
`CARGO_ENCODED_RUSTFLAGS` cannot silently replace AddressSanitizer
instrumentation. The native sanitizer lane also replaces any Cargo target
runner with the fixed `/usr/bin/env` identity runner, so a configured wrapper
cannot skip the test executables while returning success.
These commands are the local and CI entry points:

```console
cargo xtask aot-unsafe-inventory
cargo xtask runtime-safety-miri
cargo xtask runtime-safety-asan
```

Local Miri and sanitizer runs require `rustup` plus the channel and components
declared in the dedicated toolchain file; install that dated nightly with its
`miri` and `rust-src` components first. The hosted workflow performs this
installation explicitly on a clean runner.

The Miri lane uses `x86_64-unknown-linux-gnu` on every host and serializes its
tests. It runs the pinned numeric test suite, the complete shared TA contract
suite, and the broker's no-default-feature unit suite—the feature topology
used by the linked runtime—with Tree Borrows and symbolic alignment checks
under strict provenance. Because Miri cannot execute `libm`'s
target-specific inline assembly, these runs explicitly select that pinned
crate's `force-soft-floats` feature; normal native and Wasm release builds do
not enable it. Miri therefore covers the pure-Rust fallback rather than the
architecture-specific assembly path. It also exercises the standalone WebAssembly scanner,
serializer, checked arithmetic, and reusable raw-pointer primitives against
native allocations. This is deliberately bounded rather than interpreting the
full workspace. The setup follows the [Miri project's documented `cargo miri`
workflow](https://github.com/rust-lang/miri#using-miri).

The sanitizer lane uses the nightly compiler's AddressSanitizer with an
instrumented standard library on its supported Linux or macOS host target. It
runs the shared TA suite, the broker suite without its host-only default
feature, and the excluded runtime's single-threaded native standalone WebAssembly harness. The
flags follow the [Rust Unstable Book sanitizer
documentation](https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html).

## What the lanes do not cover

Miri does not execute `wasm32-unknown-unknown`, and Rust's native sanitizers do
not instrument instructions inside a WebAssembly binary interpreted by Wasmi.
Consequently, neither lane is evidence that the following target-specific
operations ran under Miri or AddressSanitizer:

- integer linear-memory addresses cast to raw pointers;
- `memory.size`, `memory.grow`, or Wasm trap behavior;
- the broker build's arena-backed global allocator;
- the TA and broker singleton `static mut` contexts under the Wasm calling
  convention; or
- raw lane/header/config reads in the checked-in runtime blobs.

Those paths remain covered by target-real Wasmi suites: `runtime_standalone_wasm_json`,
`runtime_ta`, `standalone_wasm`, `standalone_wasm_strategy`, and the AOT differential tests. They exercise
invalid host ranges, malformed declaration/config tables, allocation overflow,
failed-allocation atomicity, serializer failures, resident-instance resets,
and the actual checked-in binaries. Wasm bounds checks trap an out-of-memory
load or store; the native standalone WebAssembly harness intentionally refuses to emulate that
address space because a surrogate allocator would not validate the production
pointer provenance.

ThreadSanitizer is not used. The runtime contract is synchronous,
single-threaded, and callback-free, so a native multithreaded wrapper would test
a different program rather than the singleton-state contract. MemorySanitizer
is also outside this bounded lane; it requires complete instrumentation of all
code and dependencies. These omissions are limitations, not passing results.

## Safety-contract map

| Runtime area | Local contract | Executable evidence |
|---|---|---|
| `lib.rs` broker allocator | standalone WebAssembly initializes and relocates the arena before broker allocation; returned addresses satisfy `Layout`; failed growth traps; deallocation only rewinds the top allocation. | Target-real `standalone_wasm_strategy` resident-instance and broker-allocation tests. The native sanitizer build excludes the broker feature because installing its Wasm-address allocator on a native process would be invalid. |
| `standalone_wasm.rs` singleton and arena | Calls are synchronous and non-reentrant; no state-derived reference crosses another state access or allocation; checked alignment/end arithmetic commits the bump pointer only after successful growth. | Native ASan allocation/range tests plus target-real allocator overflow, invalid rewind, range, reset, and repeated-evaluation tests. |
| `standalone_wasm.rs` scanner and serializer | Host slices are range-validated before construction; number bytes are ASCII by tokenizer construction; output capacity is established before raw copy; serializer state and raw copy/read/write primitives are target-independent. | Native Miri/ASan raw-pointer, scanner-prefix/byte corpus, serializer transitions/failures, and cursor-atomicity test; target-real scanner differential and printer/OUT tests; target-real `standalone_wasm_chart_time` request-contract, binary and resident-packet tests of the timeframe fill. |
| `calendar.rs` timeframe and calendar arithmetic | Scalar integer arithmetic on its arguments and a borrowed request slice, with no memory access of its own; an intermediate that can leave `i64` is checked. Its two unsafe attributes export `standalone_wasm_calendar` and `standalone_wasm_timestamp` under fixed names. | Target-real `standalone_wasm_chart_time` differential against the interpreter over every calendar part and `timestamp` form at each supported timezone form, fields that carry or leave `i64`, every timeframe form, bar times across the time lane's exact range (±2^53 ms), literal times at the `i64` limits, and `na` times. |
| `ta.rs` raw regions | Initialization validates region sizes and writes declarations/states before publishing the context; advance reconstructs slices only from that context and writes two reserved extras cells. | Miri/ASan shared `pinewasm_ta` contract plus target-real `runtime_ta` initialization, corrupt-table, reset, and engine differential tests. |
| `broker.rs` context and reads | Initialization validates the config range before constructing a slice; compiler-authored string handles and CSAW header/lane addresses remain live for the synchronous call; no context borrow crosses allocation. | Miri/ASan shared `pinewasm_broker` suite plus target-real `standalone_wasm_strategy` and broker differential tests. |

Every unsafe expression remains subject to the runtime crate's
`undocumented_unsafe_blocks = "deny"` and `unsafe_op_in_unsafe_fn = "deny"`
gates. The table below is generated from parsed Rust syntax trees, including
the token trees that Rust macros otherwise leave opaque, not from a
hand-maintained count. Any source edit that adds, removes, or moves an unsafe
site makes `cargo xtask check` fail until a reviewer runs
`cargo xtask aot-unsafe-inventory --write` and examines the inventory diff.

## Generated unsafe-site inventory

<!-- BEGIN GENERATED UNSAFE INVENTORY -->
| Location | Construct | Source |
|---|---|---|
| [`crates/pinewasm_aot/runtime/src/broker.rs:252:8`](../crates/pinewasm_aot/runtime/src/broker.rs#L252) | mutable static | `static mut CTX: Ctx = Ctx {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:272:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L272) | unsafe function | `unsafe fn ctx() -> &'static mut Ctx {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:275:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L275) | unsafe block | `unsafe { &mut *ptr::addr_of_mut!(CTX) }` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:289:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L289) | unsafe block | `unsafe { ptr::read_unaligned(addr as *const i64) }` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:295:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L295) | unsafe block | `unsafe { ptr::read_unaligned(addr as *const u8) }` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:391:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L391) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:406:17`](../crates/pinewasm_aot/runtime/src/broker.rs#L406) | unsafe block | `let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:437:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L437) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:441:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L441) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:461:39`](../crates/pinewasm_aot/runtime/src/broker.rs#L461) | unsafe block | `standalone_wasm::arena_rewind(unsafe { standalone_wasm::arena_init() });` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:475:27`](../crates/pinewasm_aot/runtime/src/broker.rs#L475) | unsafe block | `let symbol_currency = unsafe { standalone_wasm::state() }.symbol_currency;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:515:37`](../crates/pinewasm_aot/runtime/src/broker.rs#L515) | unsafe block | `let (pointvalue, mincontract) = unsafe {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:575:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L575) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:579:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L579) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:613:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L613) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:617:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L617) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:641:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L641) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:647:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L647) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:670:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L670) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:685:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L685) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:712:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L712) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:734:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L734) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:748:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L748) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:761:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L761) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:772:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L772) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:784:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L784) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:789:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L789) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:799:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L799) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:803:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L803) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:811:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L811) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:816:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L816) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:825:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L825) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:841:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L841) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:882:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L882) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:890:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L890) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:898:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L898) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:906:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L906) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:915:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L915) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:921:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L921) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:929:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L929) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:935:25`](../crates/pinewasm_aot/runtime/src/broker.rs#L935) | unsafe block | `let Some(broker) = (unsafe { ctx() }).broker.as_mut() else {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:948:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L948) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:952:23`](../crates/pinewasm_aot/runtime/src/broker.rs#L952) | unsafe block | `let Some(snap) = (unsafe { ctx() })` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:992:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L992) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:996:23`](../crates/pinewasm_aot/runtime/src/broker.rs#L996) | unsafe block | `let Some(snap) = (unsafe { ctx() })` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1030:6`](../crates/pinewasm_aot/runtime/src/broker.rs#L1030) | unsafe block | `(unsafe { ctx() })` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1038:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L1038) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1064:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L1064) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1090:1`](../crates/pinewasm_aot/runtime/src/broker.rs#L1090) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1096:8`](../crates/pinewasm_aot/runtime/src/broker.rs#L1096) | unsafe block | `if unsafe { standalone_wasm::state() }.objective_results {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1100:8`](../crates/pinewasm_aot/runtime/src/broker.rs#L1100) | unsafe block | `if unsafe { standalone_wasm::state() }.trade_results {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1106:16`](../crates/pinewasm_aot/runtime/src/broker.rs#L1106) | unsafe block | `let full = unsafe { standalone_wasm::state() }.include_results;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1112:12`](../crates/pinewasm_aot/runtime/src/broker.rs#L1112) | unsafe block | `\|\| unsafe { ctx() }` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1121:9`](../crates/pinewasm_aot/runtime/src/broker.rs#L1121) | unsafe block | `unsafe { ctx() }.broker.take()` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1134:14`](../crates/pinewasm_aot/runtime/src/broker.rs#L1134) | unsafe block | `let st = unsafe { standalone_wasm::arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1138:18`](../crates/pinewasm_aot/runtime/src/broker.rs#L1138) | unsafe block | `let binary = unsafe { standalone_wasm::state() }.binary_results;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1148:22`](../crates/pinewasm_aot/runtime/src/broker.rs#L1148) | unsafe block | `let st = unsafe { standalone_wasm::state() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1158:18`](../crates/pinewasm_aot/runtime/src/broker.rs#L1158) | unsafe block | `let result = unsafe { ctx() }` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1167:18`](../crates/pinewasm_aot/runtime/src/broker.rs#L1167) | unsafe block | `let start = (unsafe { standalone_wasm::arena_init() }.arena_ptr + 7) & !7;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1203:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L1203) | unsafe block | `unsafe { standalone_wasm::state() }.arena_ptr = out.pos;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1220:18`](../crates/pinewasm_aot/runtime/src/broker.rs#L1220) | unsafe block | `let start = (unsafe { standalone_wasm::arena_init() }.arena_ptr + 7) & !7;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1233:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L1233) | unsafe block | `unsafe { standalone_wasm::state() }.arena_ptr = out.pos;` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1271:41`](../crates/pinewasm_aot/runtime/src/broker.rs#L1271) | unsafe block | `let (currency_text, currency_len) = unsafe {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1277:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L1277) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1422:13`](../crates/pinewasm_aot/runtime/src/broker.rs#L1422) | unsafe block | `let c = unsafe { ctx() };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1465:8`](../crates/pinewasm_aot/runtime/src/broker.rs#L1465) | unsafe block | `if unsafe { standalone_wasm::state() }.include_results {` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1589:5`](../crates/pinewasm_aot/runtime/src/broker.rs#L1589) | unsafe block | `unsafe { ptr::write_unaligned((out.start + 16) as *mut u32, metadata_len) };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1619:21`](../crates/pinewasm_aot/runtime/src/broker.rs#L1619) | unsafe block | `let bytes = unsafe { core::slice::from_raw_parts(offset as *const u8, lane_bytes) };` |
| [`crates/pinewasm_aot/runtime/src/broker.rs:1627:9`](../crates/pinewasm_aot/runtime/src/broker.rs#L1627) | unsafe block | `unsafe { core::slice::from_raw_parts(result.equity.as_ptr().cast::<u8>(), lane_bytes) };` |
| [`crates/pinewasm_aot/runtime/src/calendar.rs:193:1`](../crates/pinewasm_aot/runtime/src/calendar.rs#L193) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/calendar.rs:305:1`](../crates/pinewasm_aot/runtime/src/calendar.rs#L305) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:120:8`](../crates/pinewasm_aot/runtime/src/chart.rs#L120) | mutable static | `static mut STATE: Option<State> = None;` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:121:8`](../crates/pinewasm_aot/runtime/src/chart.rs#L121) | mutable static | `static mut REPLY: Vec<u8> = Vec::new();` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:123:8`](../crates/pinewasm_aot/runtime/src/chart.rs#L123) | mutable static | `static mut INIT_ERROR: Option<String> = None;` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:130:5`](../crates/pinewasm_aot/runtime/src/chart.rs#L130) | unsafe block | `unsafe { (*core::ptr::addr_of_mut!(STATE)).as_mut() }` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:143:5`](../crates/pinewasm_aot/runtime/src/chart.rs#L143) | extern block | `extern "C" {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:161:9`](../crates/pinewasm_aot/runtime/src/chart.rs#L161) | unsafe block | `unsafe { floor().write(WORD) }` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:166:12`](../crates/pinewasm_aot/runtime/src/chart.rs#L166) | unsafe block | `if unsafe { floor().read() } != WORD {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:201:8`](../crates/pinewasm_aot/runtime/src/chart.rs#L201) | unsafe block | `Ok(unsafe { core::slice::from_raw_parts(at as *const u8, len as usize) })` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:209:5`](../crates/pinewasm_aot/runtime/src/chart.rs#L209) | unsafe block | `unsafe { core::ptr::read_unaligned(offset as *const u32) }` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:246:5`](../crates/pinewasm_aot/runtime/src/chart.rs#L246) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:268:8`](../crates/pinewasm_aot/runtime/src/chart.rs#L268) | mutable static | `static mut SLAB: Slab = Slab {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:278:23`](../crates/pinewasm_aot/runtime/src/chart.rs#L278) | unsafe block | `Ok(layout) => unsafe { std::alloc::alloc(layout) as u32 },` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:287:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L287) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:302:16`](../crates/pinewasm_aot/runtime/src/chart.rs#L302) | unsafe block | `let slab = unsafe { &mut *core::ptr::addr_of_mut!(SLAB) };` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:390:13`](../crates/pinewasm_aot/runtime/src/chart.rs#L390) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:411:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L411) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:430:9`](../crates/pinewasm_aot/runtime/src/chart.rs#L430) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:441:13`](../crates/pinewasm_aot/runtime/src/chart.rs#L441) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:448:13`](../crates/pinewasm_aot/runtime/src/chart.rs#L448) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:462:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L462) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:484:5`](../crates/pinewasm_aot/runtime/src/chart.rs#L484) | unsafe block | `unsafe { core::slice::from_raw_parts_mut(state.scratch as *mut i64, state.scratch_cells) }` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:489:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L489) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:503:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L503) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:519:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L519) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:529:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L529) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:546:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L546) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:555:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L555) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:582:20`](../crates/pinewasm_aot/runtime/src/chart.rs#L582) | unsafe block | `let cell = unsafe {` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:651:9`](../crates/pinewasm_aot/runtime/src/chart.rs#L651) | unsafe block | `unsafe { core::slice::from_raw_parts(at as *const i32, visible) }` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:729:1`](../crates/pinewasm_aot/runtime/src/chart.rs#L729) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/chart.rs:742:28`](../crates/pinewasm_aot/runtime/src/chart.rs#L742) | unsafe block | `return encode(&unsafe { (*core::ptr::addr_of!(INIT_ERROR)).as_deref() });` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:97:5`](../crates/pinewasm_aot/runtime/src/lib.rs#L97) | unsafe impl | `unsafe impl GlobalAlloc for ArenaBump {` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:99:9`](../crates/pinewasm_aot/runtime/src/lib.rs#L99) | unsafe method | `unsafe fn alloc(&self, layout: Layout) -> *mut u8 {` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:104:9`](../crates/pinewasm_aot/runtime/src/lib.rs#L104) | unsafe method | `unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:115:9`](../crates/pinewasm_aot/runtime/src/lib.rs#L115) | unsafe method | `unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:122:17`](../crates/pinewasm_aot/runtime/src/lib.rs#L122) | unsafe block | `unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:125:13`](../crates/pinewasm_aot/runtime/src/lib.rs#L125) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:158:12`](../crates/pinewasm_aot/runtime/src/lib.rs#L158) | opaque attribute in macro | `$(#[$m])*` |
| [`crates/pinewasm_aot/runtime/src/lib.rs:159:11`](../crates/pinewasm_aot/runtime/src/lib.rs#L159) | unsafe attribute in macro | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:130:8`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L130) | mutable static | `static mut STATE: State = State {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:172:12`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L172) | unsafe function | `pub(crate) unsafe fn state() -> &'static mut State {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:175:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L175) | unsafe block | `unsafe { &mut *ptr::addr_of_mut!(STATE) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:179:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L179) | extern block | `extern "C" {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:210:12`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L210) | unsafe function | `pub(crate) unsafe fn arena_init() -> &'static mut State {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:213:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L213) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:280:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L280) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:320:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L320) | unsafe function | `unsafe fn read_unaligned<T: Copy>(source: *const u8) -> T {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:324:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L324) | unsafe block | `unsafe { ptr::read_unaligned(source.cast::<T>()) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:333:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L333) | unsafe function | `unsafe fn write_unaligned<T: Copy>(destination: *mut u8, value: T) {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:336:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L336) | unsafe block | `unsafe { ptr::write_unaligned(destination.cast::<T>(), value) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:345:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L345) | unsafe function | `unsafe fn copy_bytes(source: *const u8, destination: *mut u8, len: usize) {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:348:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L348) | unsafe block | `unsafe { ptr::copy_nonoverlapping(source, destination, len) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:356:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L356) | unsafe function | `unsafe fn move_bytes(source: *const u8, destination: *mut u8, len: usize) {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:359:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L359) | unsafe block | `unsafe { ptr::copy(source, destination, len) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:366:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L366) | unsafe block | `unsafe { read_unaligned(addr as *const u8) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:373:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L373) | unsafe block | `unsafe { read_unaligned(addr as *const u8) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:380:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L380) | unsafe block | `unsafe { read_unaligned(addr as *const u8) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:387:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L387) | unsafe block | `unsafe { write_unaligned(addr as *mut u8, v) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:393:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L393) | unsafe block | `unsafe { write_unaligned(addr as *mut u8, v) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:399:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L399) | unsafe block | `unsafe { write_unaligned(addr as *mut u8, v) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:405:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L405) | unsafe block | `unsafe { copy_bytes(cell.as_ptr(), addr as *mut u8, cell.len()) }` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:415:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L415) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:419:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L419) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:516:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L516) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:560:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L560) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:587:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L587) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:613:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L613) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:620:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L620) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:624:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L624) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:641:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L641) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:645:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L645) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:654:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L654) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:658:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L658) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:672:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L672) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:677:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L677) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:703:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L703) | unsafe block | `unsafe { move_bytes(st.arena_lo as *const u8, target as *mut u8, live as usize) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:714:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L714) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:758:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L758) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:968:20`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L968) | unsafe block | `let text = unsafe { core::str::from_utf8_unchecked(bytes) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1065:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1065) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1066:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1066) | unsafe function | `pub unsafe extern "C" fn standalone_wasm_scan(ptr_: *const u8, len: usize) -> i64 {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1070:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1070) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1086:16`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1086) | unsafe block | `let text = unsafe { core::slice::from_raw_parts(ptr_, len) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1176:22`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1176) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1426:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1426) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1427:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1427) | unsafe function | `pub unsafe extern "C" fn standalone_wasm_fill_lanes(` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1442:16`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1442) | unsafe block | `let text = unsafe { core::slice::from_raw_parts(json_ptr, json_len) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1445:15`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1445) | unsafe block | `let tab = unsafe { core::slice::from_raw_parts(keytab_ptr, keytab_len) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1455:26`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1455) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1477:26`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1477) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1504:18`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1504) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1564:30`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1564) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1597:38`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1597) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1628:30`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1628) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1656:18`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1656) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1775:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1775) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1776:5`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1776) | unsafe function | `pub unsafe extern "C" fn standalone_wasm_fill_timeframe(` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:1788:16`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L1788) | unsafe block | `let text = unsafe { core::slice::from_raw_parts(json_ptr, json_len) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:2791:9`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L2791) | unsafe block | `unsafe { copy_bytes(bytes.as_ptr(), self.pos as *mut u8, bytes.len()) };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:2853:1`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L2853) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:2861:14`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L2861) | unsafe block | `let st = unsafe { arena_init() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:2871:22`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L2871) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:3058:18`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L3058) | unsafe block | `let st = unsafe { state() };` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:3124:9`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L3124) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/standalone_wasm.rs:3134:9`](../crates/pinewasm_aot/runtime/src/standalone_wasm.rs#L3134) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:83:8`](../crates/pinewasm_aot/runtime/src/ta.rs#L83) | mutable static | `static mut CTX: Ctx = EMPTY;` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:107:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L107) | unsafe block | `unsafe { core::ptr::read_unaligned(offset as *const f64) }` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:156:1`](../crates/pinewasm_aot/runtime/src/ta.rs#L156) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:157:5`](../crates/pinewasm_aot/runtime/src/ta.rs#L157) | unsafe function | `pub unsafe extern "C" fn pinewasm_rt_ta_init(` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:172:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L172) | unsafe block | `unsafe { CTX = EMPTY };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:184:21`](../crates/pinewasm_aot/runtime/src/ta.rs#L184) | unsafe block | `let bytes = unsafe { slice::from_raw_parts(src, Decl::SIZE) };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:190:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L190) | unsafe block | `unsafe { core::ptr::write(decls_ptr.add(i as usize), decl) };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:195:17`](../crates/pinewasm_aot/runtime/src/ta.rs#L195) | unsafe block | `let decls = unsafe { slice::from_raw_parts(decls_ptr as *const Decl, count as usize) };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:206:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L206) | unsafe block | `unsafe { core::ptr::write(states_ptr.add(i as usize), State::Invalid) };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:213:5`](../crates/pinewasm_aot/runtime/src/ta.rs#L213) | unsafe block | `unsafe {` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:229:15`](../crates/pinewasm_aot/runtime/src/ta.rs#L229) | unsafe block | `let ctx = unsafe { CTX };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:231:5`](../crates/pinewasm_aot/runtime/src/ta.rs#L231) | unsafe block | `unsafe { engine(&ctx) }.reset();` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:240:1`](../crates/pinewasm_aot/runtime/src/ta.rs#L240) | unsafe function | `unsafe fn engine(ctx: &Ctx) -> Engine<'static> {` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:244:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L244) | unsafe block | `unsafe { slice::from_raw_parts(ctx.decls as *const Decl, ctx.count as usize) },` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:246:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L246) | unsafe block | `unsafe { slice::from_raw_parts_mut(ctx.states as *mut State, ctx.count as usize) },` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:249:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L249) | unsafe block | `unsafe { slice::from_raw_parts_mut(ctx.cells as *mut f64, ctx.cell_len as usize) },` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:251:9`](../crates/pinewasm_aot/runtime/src/ta.rs#L251) | unsafe block | `unsafe { slice::from_raw_parts_mut(ctx.scratch as *mut f64, ctx.scratch_len as usize) },` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:268:1`](../crates/pinewasm_aot/runtime/src/ta.rs#L268) | unsafe attribute | `#[no_mangle]` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:269:5`](../crates/pinewasm_aot/runtime/src/ta.rs#L269) | unsafe function | `pub unsafe extern "C" fn pinewasm_rt_ta_advance(` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:278:15`](../crates/pinewasm_aot/runtime/src/ta.rs#L278) | unsafe block | `let ctx = unsafe { CTX };` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:291:21`](../crates/pinewasm_aot/runtime/src/ta.rs#L291) | unsafe block | `let Some(out) = unsafe { engine(&ctx) }.advance(state, [a, b, c], &lanes, bar) else {` |
| [`crates/pinewasm_aot/runtime/src/ta.rs:299:5`](../crates/pinewasm_aot/runtime/src/ta.rs#L299) | unsafe block | `unsafe {` |
<!-- END GENERATED UNSAFE INVENTORY -->
