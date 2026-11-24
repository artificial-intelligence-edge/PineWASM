// The WebAssembly feature set the hardened standalone WebAssembly host
// accepts, defined once and read from two places: `build.rs` validates every
// canonical runtime module under it while the compiler is built, and the
// producer audit in `lib.rs` validates every emitted artifact under it. The
// verified-copy audit rests on those two validations agreeing about which
// instructions are allowed, so the set lives in a file both `include!` rather
// than in two constants that could drift apart.
//
// Plain `//` comments only: `include!` splices this file into the build
// script's root, where an inner `//!` attribute is not permitted.

/// Everything wasmparser enables by default except the extensions the
/// hardened host disables. A module using any of them would fail to
/// instantiate there, so it must fail validation here first.
pub(crate) fn hardened_host_features() -> wasmparser::WasmFeatures {
    wasmparser::WasmFeatures::default()
        - wasmparser::WasmFeatures::SIMD
        - wasmparser::WasmFeatures::RELAXED_SIMD
        - wasmparser::WasmFeatures::THREADS
        - wasmparser::WasmFeatures::MEMORY64
        - wasmparser::WasmFeatures::MULTI_MEMORY
}
