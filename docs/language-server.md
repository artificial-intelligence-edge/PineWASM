# PineWASM language server

`pinewasm` is the language-tooling host for PineWASM's implemented subset of
Pine Script® v6. Pine Script is a trademark of TradingView, Inc.; PineWASM is
not affiliated with it. One compiler-backed analysis core is exposed to editors
over the Language Server Protocol:

```text
editor / IDE ── LSP (Content-Length JSON-RPC) ── analysis snapshot ─ compiler
                                                                   ├─ diagnostics
                                                                   ├─ AST / HIR
                                                                   ├─ MIR / CSIL
                                                                   └─ timings
```

The [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
standardizes editor features such as diagnostics, completion, hover, signature
help, navigation, symbols, semantic tokens, and refactoring. The server
implements them directly against compiler state rather than a second,
editor-only model of the language.

This shared-analysis design is broadly comparable to language servers such as
Go's first-party [`gopls`](https://go.dev/gopls/features/diagnostics), which
presents compiler and analyzer findings through one diagnostics experience,
and [`rust-analyzer`](https://github.com/rust-lang/rust-analyzer), whose server
is built from reusable analysis libraries. PineWASM applies the same general
separation of protocol transport from compiler analysis; this is an
architectural comparison, not a claim of code lineage or feature parity.

## Build and run

Build the native binary:

```sh
cargo xtask build
```

Run it as an editor language server over stdio:

```sh
cargo xtask language-server
```

The xtask launches the server against this repository root. To serve another
Pine workspace after building, run
`target/debug/pinewasm lsp --workspace /absolute/path/to/pine/workspace`.
Repository commands use the toolchain selected by `rust-toolchain.toml` and
sanitize child compiler-tool overrides.

The default subcommand is `lsp`. The binary also provides scriptable native
commands:

```sh
pinewasm check script.pine --workspace .
pinewasm inspect script.pine ast --workspace .
pinewasm inspect script.pine hir --workspace .
pinewasm inspect script.pine mir --workspace .
pinewasm inspect script.pine csil --workspace .
pinewasm inspect script.pine all --workspace .
pinewasm migrate script.pine [--write] [--idiomatic] [--modernize] [--strict-indices] [--json]
```

`check` writes the structured `CompileResult` as JSON and exits nonzero when
the compiler reports an error. `inspect` writes the requested debug dump,
diagnostics, and per-phase timing object as JSON. `migrate` rewrites Pine v5
source as v6; see [migration.md](migration.md).

## LSP surface

The server advertises only implemented capabilities and uses full-document
text synchronization with immutable per-revision snapshots. Compiler byte
spans are converted to the LSP's UTF-16 positions, including non-BMP Unicode.
Each process has one deterministic workspace root; multi-root editors should
launch one server process per root.

| Capability | Behavior |
| --- | --- |
| Diagnostics | Parser, compiler, type/qualifier, and lint diagnostics; stable codes, severity, notes, labels, and raw structured data. A `//@version=5` document also reports the constructs the PineWASM v6 migration does not rewrite (source `pinewasm migrate`) |
| Completion | Compiler-registry keywords, series, constants, functions, namespaces, scoped document symbols, enum/type members, and mapped library exports |
| Hover | Compiler signatures/types plus local declaration metadata |
| Signature help | Compiler-registry parameters, types, qualifiers, optional/default arguments, and local callables |
| Definition | Local declarations and mapped imported-library members |
| References | Document references to a user-defined symbol |
| Rename | Scope-aware document edits for variables, parameters, functions, types, and import aliases; built-ins and members without typed receiver resolution are withheld |
| Code actions | Preferred quick fixes for compiler `did you mean` suggestions; for a `//@version=5` document, **Convert script to Pine v6** (whole file) and one **Migrate to v6** quick fix per rewrite in range (see [migration.md](migration.md)) |
| Symbols | Document symbols plus bounded workspace symbol search across Pine files |
| Semantic tokens | Namespace, type, function, variable, parameter, property, enum-member, and keyword classifications |
| Folding | Function, method, and user-type regions |
| Execute command | AST, HIR, MIR, CSIL, full-pipeline, and profiling inspection |

Standard commands exposed through `workspace/executeCommand` are:

- `pinewasm.inspectAst`
- `pinewasm.inspectHir`
- `pinewasm.inspectMir`
- `pinewasm.inspectCsil`
- `pinewasm.inspectPipeline`
- `pinewasm.profile`

The first command argument is the URI of an open document. Results are
structured JSON containing only the requested dumps, compiler diagnostics,
success state, and phase timings.

Any editor that can launch a stdio LSP can use the server. Register `.pine`
and `.pine.local` as the `pine` language ID and configure the command as:

```text
pinewasm lsp --workspace <workspace-root>
```

For development without installing the binary, use
`cargo run --locked -p pinewasm_lsp -- lsp --workspace <workspace-root>`.

## Libraries and workspaces

The server never downloads Pine libraries. It builds a local
catalog at workspace initialization and refreshes it after watched-file
changes. Import paths are discovered through either convention:

- `libraries/Publisher/Library/Version.pine` or `library/Publisher/Library/Version.pine`
- `Publisher_Library_Version.pine.local`

For names that cannot be expressed unambiguously by those conventions, add
`.pinewasm-lsp.json` at the workspace root:

```json
{
  "libraries": {
    "acme/Signals/6": "libs/acme_Signals_6.pine.local"
  }
}
```

Mapped paths and the configuration file itself must resolve inside the
canonical workspace root, including through symlinks. Special filesystem
entries are skipped. An open unsaved library buffer replaces its cataloged disk
revision before dependent documents are recompiled, so a library edit is
reflected in its importers before either file is saved.

The catalog admits at most 128 libraries, with 256 UTF-8 bytes per import path,
1 MiB per library source, and 8 MiB of aggregate library source. The config file
is also limited to 1 MiB. Valid explicit mappings take priority over discovered
entries. Discovery visits at most 50,000 directories to a depth of 12 and keeps
at most 10,000 Pine files, with a 750 ms soft scan budget; library count and
byte limits can stop admission earlier. Invalid configurations, unreadable
libraries, and rejected mappings produce stderr diagnostics. Reaching a
discovery ceiling can also leave files unvisited. The
remaining catalog is usable, so an omitted library can produce an unresolved
import diagnostic rather than aborting the server.

A client that resolves libraries itself can supply them with the
`pinewasm/setLibraries` notification:

```json
{ "libraries": [{ "importPath": "acme/Signals/6", "source": "//@version=6\nlibrary(\"Signals\")\n..." }] }
```

Each notification replaces the whole client-supplied set. A supplied library
replaces a cataloged one with the same import path, so the editor sees the same
sources the client compiles. The same limits apply, and invalid entries are
skipped with a stderr diagnostic.

## Pipeline introspection and profiling

Introspection uses the compiler's existing stage gates instead of duplicating
the pipeline:

```text
source → AST → typed HIR → control-flow MIR → validated CSIL
```

- AST is the faithful span-carrying syntax tree.
- HIR contains resolved symbols, types, qualifiers, and desugared Pine forms.
- MIR exposes evaluation order, basic blocks, virtual values, and lowering
  decisions useful for optimization work.
- CSIL is the validated stack bytecode consumed by the VM and AOT backend.
- Timings report parse, libraries, semantics, MIR, CSIL, validation, dump, and
  total wall time independently, so rendering a large dump does not look like
  a slow compiler stage.

Request a single stage by default. `all` is intended for pipeline debugging or
profiling because its output can be large.

## Security and boundaries

- Discovered files and configured libraries are confined to the canonical
  workspace root, including the scriptable `check` and `inspect` commands.
- The server never writes source files. Rename and quick-fix operations return
  workspace edits for the client to review and apply.
- Library resolution is local-only and performs no network access.
- The stdio process writes protocol messages only to stdout; everything else
  (skipped libraries, ignored config) goes to stderr.
- Source size and compiler resource limits are shared with the native and
  WebAssembly compiler APIs. Workspace-symbol disk reads are bounded to 1 MiB
  per file. An open buffer above 1 MiB reports `E0001` and suppresses parse-only
  symbols, semantic tokens, and v5 migration analysis. The editor buffer and its
  line index are still retained: these analysis bounds are not a transport or
  process-memory quota.

## Validation

The crate contains UTF-8/UTF-16 conversion tests, compiler diagnostic and
pipeline inspection tests, registry-surface tests, library-catalog tests, and
an in-memory LSP session that opens a document, receives diagnostics, performs
hover, and shuts down cleanly.

Run the locked workspace suite, which includes the focused language-server
tests:

```sh
cargo xtask test
```

The normal workspace quality gate also includes this crate:

```sh
cargo xtask check
```
