# PineWASM WebAssembly bindings

`pinewasm_bindings` packages the PineWASM compiler, virtual machine, language
services, search API, and ahead-of-time compiler for JavaScript hosts. It is a backend library: it does not include a user
interface, market-data client, broker connection, or artifact store.

## Install

PineWASM uses the same package name whether it comes from a local tarball or a
registry. To build a tarball from a trusted source checkout, run:

```sh
cargo xtask wasm
npm pack ./target/wasm-pkg --pack-destination /absolute/path/to/output-directory
```

Then install the resulting `ai-edge-pinewasm-<version>.tgz` in the consuming
project:

```sh
npm install /absolute/path/to/ai-edge-pinewasm-<version>.tgz
```

A source checkout does not establish that a registry release is available. If a
maintainer publishes a verified release, it uses the same package name:

```sh
npm install @ai-edge/pinewasm
```

Generated package metadata and artifact notices record the source repository
and build commit. Before distributing a package, verify that recipients can
retrieve that exact source. If the source repository changes, rebuild the
package and regenerate its notices; do not relabel an existing artifact. See
[artifact distribution](../../docs/artifact-distribution.md) for the corresponding-source requirements.

Application code should always use the package specifier rather than a build
directory. In a browser or bundler, the loader resolves its sibling Wasm file:

```js
import init, { compileAndRun } from '@ai-edge/pinewasm';

await init();
```

In Node.js, load the generated Wasm asset as bytes so initialization does not
depend on `fetch()` support for local files:

```js
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import init, { compileAndRun } from '@ai-edge/pinewasm';

const require = createRequire(import.meta.url);
const bytes = await readFile(require.resolve('@ai-edge/pinewasm/pinewasm_bg.wasm'));
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

The package contains the `@ai-edge/pinewasm` ES module, generated TypeScript
declarations, and WebAssembly binary. Initialize the default export once, then
use the named compile, run, library, incremental-session, search, or AOT
exports. The bundled `pinewasm.d.ts` is the checked contract for the package's
high-level exports and supported serialized data-transfer-object types. It
keeps a compiled `Program` opaque to TypeScript while exposing its input manifest,
preserves wide integer fields as `bigint`, and models run-time input overrides as native
`Map<number, InputValue>` values. Only wasm-bindgen's low-level `InitOutput`
ABI retains untyped externref slots; application code should not call those raw
functions. `docs/browser.md` in the matching source revision documents field
defaults, resource limits, and the few intentional snake_case wire keys.

For immediate VM or standalone preparation, `compileResident(source, libraries,
options)` keeps CSIL inside Wasm instead of exporting and rebuilding it. Its
`program` is an immutable handle with `inputs`, `plots`, and `metadata` getters,
`prepareVm()` and `compileStandaloneWasm(options)` consumers, and an explicit
`exportProgram()` for full inspection. Free the handle on eviction. Consumers
keep the same validation and fresh-state evaluation contracts; the serialized
compile APIs remain available. This is not an incremental source-edit cache.

In the declarations, a field typed as `T | null` is present with `null` when
its optional Rust value is empty. A field marked with `?` may instead be absent
when it is empty or unavailable. Pine `na` values inside series remain `null`.
Omit an unused optional object property instead of assigning JavaScript
`undefined`: the Rust boundary rejects a present property whose value cannot be
deserialized. Passing an entire optional function argument as `undefined`
remains supported.

For example, input overrides are externally tagged values in a native
JavaScript `Map` keyed by the numeric id from `program.inputs`:

```ts
import type { RunOptions } from '@ai-edge/pinewasm';

const options: RunOptions = {
  inputs: new Map([
    [0, { Int: 20 }],
    [1, { Source: 'Close' }],
  ]),
};
```

A plain object with numeric-looking keys, a raw scalar such as
`new Map([[0, 20]])`, or an input value containing more than one variant tag is
not the wire format and is rejected.

## Library boundary and validation

Library-aware exports accept an array of `[importPath, source]` string pairs.
The JavaScript boundary reads ordinary arrays through their own indexed data
properties. Each entry must be an array containing two string data properties;
sparse entries and accessors are rejected, and custom iterators are not used.
Failures while inspecting a proxy are rejected. Strings must contain
well-formed UTF-16. The boundary takes one immutable snapshot and checks UTF-8
lengths before copying sources into Wasm: at most 128 libraries, 256 bytes per
import path, 1 MiB per source, and 8 MiB of aggregate library source. A host must
provide `TextEncoder`; `String.prototype.isWellFormed` is optional because a
bounded fallback validates surrogate pairs.

After generating the package, run the JavaScript/Wasm boundary suite:

```sh
cargo xtask wasm
cargo xtask wasm-test
```

The suite requires Node.js with `TextEncoder` and the generated
`target/wasm-pkg` package. CI validates the package under Node.js 24 LTS.
The focused Rust test can also be invoked directly:

```sh
cargo test --locked -p pinewasm_bindings --test library_boundary -- --ignored
```

It is ignored by ordinary `cargo test` because it executes the generated package
in Node.js. The packaging CI and release gate explicitly run it.

PineWASM is an independent compiler and runtime for Pine Script® v6
source. It is developed by AI Edge LLC and is not affiliated with, authorized
by, sponsored by, or endorsed by TradingView®. Pine Script® and
TradingView® are trademarks of TradingView, Inc. Coverage is limited to
the subset documented in this repository's coverage matrix.

The package is licensed under AGPL-3.0-only. `LICENSE`, `NOTICE`, and
`THIRD_PARTY_LICENSES.txt` are included in every generated package.


## Versioned language queries

`languageCapabilities()` declares `pinewasm-language-v1`. `languageQuery()` accepts
only an explicit in-memory document and optional libraries with exact opaque IDs
and versions. UTF-16 positions are zero-based and must round-trip exactly. The
facade never reads files, discovers libraries, or runs workspace commands. Main
and library diagnostics preserve document identity; rename and quick fixes return
proposals and do not mutate buffers. Hosts must enforce approval and version CAS
before applying any proposal. Missing exports or a contract-version mismatch are
an unavailable capability, never a regex fallback. Single-buffer rename uses the
same binding-preservation checks as the native server, including shadowing,
reserved-name rejection, and ancestor/descendant capture rejection.

Limits: 256 KiB total source, 32 libraries, 4,096 result entries, 256 KiB
serialized result data, and 67,108,864 estimated symbol-resolution operations
for references and rename. When the deterministic work estimate exceeds that
limit, the query returns the typed `query_work_budget_exceeded` error; it never
returns a partial result. A host should execute queries in its existing
disposable worker to enforce a time/cancellation budget. The host supplies only documents
that the user disclosed and binds their IDs/versions to its snapshot digests.

Reproduce native/Wasm fixture parity with the pinned toolchain:

```sh
cargo test -p pinewasm_lsp
cargo test -p pinewasm_lsp --no-default-features
cargo test -p pinewasm_bindings
cargo xtask wasm
cargo xtask wasm-test
```

The fixtures cover Unicode, an attached library definition and diagnostic,
shadowed references, bounded edits, compiler quick fixes, invalid contract and
UTF-16 positions, and rejected binding capture. Native LSP tests additionally
exercise its workspace authorization independently of the shared query engine.

The existing generated-package gate includes the same language-query parity and
boundary checks. It binds the package to the current checkout revision, requires
the complete fixture roster and seven capabilities, and checks that capability
limits arrive as JavaScript numbers. This is bounded native/Wasm agreement;
it does not cover end-to-end host integration.
