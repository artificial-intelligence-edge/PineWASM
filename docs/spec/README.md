# The CSAW specification

CSAW—the **Compiled Series Artifact for WebAssembly** format, identified by
its `CSAW` header magic—is the public module ABI for per-script WebAssembly
artifacts emitted by PineWASM. It defines the standard exports, native header
and linear-memory layout, value encodings, the `host.vm` and hosted-interpreter
contracts, execution and error protocols, linked-runtime boundaries, fill
color lanes, and metadata custom sections.

The normative text is [`csaw-module-abi.md`](./csaw-module-abi.md).

**Current status:** ABI version 6, pre-1.0. Consumers must use exact version
matching. Native v6 modules export `memory`, `prepare`, and `run`; they import
at most `host.vm`. Numeric and technical-analysis runtimes are linked into
the module, alert conditions use an output lane when no VM host is present,
and color-lane order is plots, visuals, then fills. The legacy `host.ta`,
`host.pure`, and `host.alert` imports are not part of v6.

**Licensing:** this directory is Apache-2.0 (see [`LICENSE`](./LICENSE)) so
independent producers and consumers can implement the specification. The
PineWASM implementation in the rest of the repository is AGPL-3.0-only.

**Proposing changes:** open an issue or pull request describing the change,
whether it requires an ABI or artifact-envelope version bump, and the
accompanying PineWASM implementation change.
