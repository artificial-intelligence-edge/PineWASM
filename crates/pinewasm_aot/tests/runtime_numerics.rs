//! The linked runtime module must be bit-identical to [`pinewasm_num`].
//!
//! This test checks the linked numeric runtime against `pinewasm_num`.
//! Emitted artifacts no longer call back into the host for `math.exp` and
//! friends; they call a WebAssembly
//! build of the same crate the interpreter uses, spliced into the artifact.
//! That is only sound if the two builds agree in every bit, so this sweeps
//! both and compares `to_bits()` — never an epsilon.
//!
//! Two distinct things are being checked, and the second is the subtle one:
//!
//! 1. The checked-in `runtime/pinewasm_aot_runtime.wasm` is a build of the
//!    *current* `pinewasm_num`. A stale blob fails here.
//! 2. Compiling the same Rust source for `wasm32-unknown-unknown` and for the
//!    host produces identical float results. It does, because Rust never
//!    contracts or reassociates float arithmetic and `libm` is plain IEEE
//!    double code — but that is a property worth failing loudly on rather
//!    than assuming.

use pinewasm_aot::runtime;

/// Every `pinewasm_num` id, with the exported name that implements it.
const CASES: &[(u32, &str)] = &[
    (pinewasm_num::id::EXP, "pinewasm_rt_exp"),
    (pinewasm_num::id::LOG, "pinewasm_rt_log"),
    (pinewasm_num::id::LOG10, "pinewasm_rt_log10"),
    (pinewasm_num::id::SIN, "pinewasm_rt_sin"),
    (pinewasm_num::id::COS, "pinewasm_rt_cos"),
    (pinewasm_num::id::TAN, "pinewasm_rt_tan"),
    (pinewasm_num::id::ASIN, "pinewasm_rt_asin"),
    (pinewasm_num::id::ACOS, "pinewasm_rt_acos"),
    (pinewasm_num::id::ATAN, "pinewasm_rt_atan"),
    (pinewasm_num::id::POW, "pinewasm_rt_pow"),
    (pinewasm_num::id::ROUND2, "pinewasm_rt_round2"),
    (pinewasm_num::id::REM, "pinewasm_rt_rem"),
];

struct Runtime {
    store: wasmi::Store<()>,
    instance: wasmi::Instance,
}

impl Runtime {
    fn load(blob: &[u8]) -> Runtime {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, blob).expect("the runtime module is valid wasm");
        let mut store = wasmi::Store::new(&engine, ());
        // No linker entries at all: the runtime module imports nothing, which
        // is the whole point of it.
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("runtime module instantiates");
        Runtime { store, instance }
    }

    fn call(&mut self, name: &str, a: f64, b: f64) -> f64 {
        let f = self
            .instance
            .get_typed_func::<(f64, f64), f64>(&self.store, name)
            .unwrap_or_else(|e| panic!("runtime exports {name}: {e}"));
        f.call(&mut self.store, (a, b))
            .unwrap_or_else(|e| panic!("{name} trapped: {e}"))
    }
}

/// A deterministic bit-pattern generator. SplitMix64's finalizer: no
/// dependency, and reproducible across platforms and runs, which matters for
/// a test whose failures are last-ulp differences.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Inputs chosen to reach every branch a libm kernel has: the na/infinite
/// short-circuits, the argument-reduction thresholds, the subnormal tails,
/// and (for trig) the huge-argument path through `rem_pio2_large`.
fn corpus() -> Vec<f64> {
    let mut values = vec![
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        2.0,
        10.0,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::from_bits(0x000F_FFFF_FFFF_FFFF),
        std::f64::consts::PI,
        std::f64::consts::E,
        std::f64::consts::LN_2,
        std::f64::consts::SQRT_2,
        // The trig reduction thresholds: below 2^20·π/2 the medium path
        // runs, above it `rem_pio2_large` does.
        1.0e6,
        1_647_099.0,
        1.0e10,
        1.0e30,
        1.0e300,
        6.123233995736766e16,
    ];
    // Multiples and near-multiples of π/2, where the trig kernels are most
    // sensitive to how the reduction rounds.
    for k in 0..64 {
        let x = std::f64::consts::FRAC_PI_2 * f64::from(k);
        values.push(x);
        values.push(f64::from_bits(x.to_bits() + 1));
        values.push(f64::from_bits(x.to_bits().wrapping_sub(1)));
        values.push(-x);
    }
    // A log sweep across the whole exponent range, both signs.
    for e in -320i32..=308 {
        let x = pinewasm_num::raw::powi(10.0, e);
        values.push(x);
        values.push(-x);
        values.push(x * 1.5);
    }
    // Fine sweeps over the domains of the inverse-trig kernels, which only
    // accept [-1, 1] and change branch at |x| = 0.5.
    for i in -2000i32..=2000 {
        values.push(f64::from(i) / 2000.0);
    }
    // Raw bit patterns: the strongest search for a kernel branch nobody
    // thought to enumerate. Many land on NaN/infinity, which exercises the
    // na policy instead.
    let mut state = 0x0DDB_A11C_0FFE_E123u64;
    for _ in 0..20_000 {
        values.push(f64::from_bits(splitmix(&mut state)));
        // Bias toward the ordinary magnitudes indicator math actually uses.
        let scaled = (splitmix(&mut state) % 2_000_000) as f64 / 1000.0 - 1000.0;
        values.push(scaled);
    }
    values
}

fn assert_same(name: &str, id: u32, a: f64, b: f64, wasm: f64) {
    let native = pinewasm_num::pure(id, a, b);
    assert_eq!(
        wasm.to_bits(),
        native.to_bits(),
        "{name}({a:?}, {b:?}): wasm {wasm:?} ({:#018x}) != native {native:?} ({:#018x})",
        wasm.to_bits(),
        native.to_bits(),
    );
}

#[test]
fn unary_kernels_match_the_interpreter_bit_for_bit() {
    let mut rt = Runtime::load(runtime::MODULE_WASM);
    let values = corpus();
    for &(id, name) in CASES {
        if id == pinewasm_num::id::POW
            || id == pinewasm_num::id::ROUND2
            || id == pinewasm_num::id::REM
        {
            continue;
        }
        for &a in &values {
            let wasm = rt.call(name, a, f64::NAN);
            assert_same(name, id, a, f64::NAN, wasm);
        }
    }
}

#[test]
fn pow_matches_the_interpreter_bit_for_bit() {
    let mut rt = Runtime::load(runtime::MODULE_WASM);
    let bases = corpus();
    // `pow`'s special-case tree keys off whether the exponent is an integer,
    // an odd integer, or a half-integer, so the exponent grid is explicit
    // rather than sampled.
    let mut exponents: Vec<f64> = vec![
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        0.5,
        -0.5,
        1.5,
        1023.0,
        -1074.0,
        f64::MAX,
    ];
    for k in -40i32..=40 {
        exponents.push(f64::from(k));
        exponents.push(f64::from(k) + 0.5);
        exponents.push(f64::from(k) / 3.0);
    }
    let mut state = 0xC0FF_EE00_1234_5678u64;
    for &a in &bases {
        for &b in &exponents {
            let wasm = rt.call("pinewasm_rt_pow", a, b);
            assert_same("pinewasm_rt_pow", pinewasm_num::id::POW, a, b, wasm);
        }
        // Plus a couple of unstructured exponents per base, so the grid is
        // not the only thing under test.
        for _ in 0..2 {
            let b = f64::from_bits(splitmix(&mut state));
            let wasm = rt.call("pinewasm_rt_pow", a, b);
            assert_same("pinewasm_rt_pow", pinewasm_num::id::POW, a, b, wasm);
        }
    }
}

#[test]
fn round2_and_rem_match_the_interpreter_bit_for_bit() {
    let mut rt = Runtime::load(runtime::MODULE_WASM);
    let values = corpus();
    let mut precisions: Vec<f64> = (-20i32..=20).map(f64::from).collect();
    // Past the `[0, 100]` clamp in both directions, plus the boundaries. The
    // wasm and interpreter sides share `clamp_precision`, so this pins that
    // they agree on the clamped result, not any particular value.
    precisions.extend([-1.0, 0.0, 100.0, 101.0, 400.0, 1e18, -1e18, f64::NAN]);
    for &a in &values {
        for &p in &precisions {
            let wasm = rt.call("pinewasm_rt_round2", a, p);
            assert_same("pinewasm_rt_round2", pinewasm_num::id::ROUND2, a, p, wasm);
        }
    }
    let mut state = 0x5EED_1234_ABCD_0001u64;
    for &a in &values {
        for &b in &[1.0, -1.0, 0.5, 3.0, 1e-300, 1e300, 0.0, f64::NAN] {
            let wasm = rt.call("pinewasm_rt_rem", a, b);
            assert_same("pinewasm_rt_rem", pinewasm_num::id::REM, a, b, wasm);
        }
        let b = f64::from_bits(splitmix(&mut state));
        let wasm = rt.call("pinewasm_rt_rem", a, b);
        assert_same("pinewasm_rt_rem", pinewasm_num::id::REM, a, b, wasm);
    }
}

/// The standalone WebAssembly blob (`MODULE_WASM_STANDALONE_WASM`) is the same crate built with
/// `--features standalone_wasm`, so its numeric exports must agree with the base blob's
/// — and with the interpreter — bit for bit. The full sweeps above pin the
/// base blob; this compact one catches a standalone WebAssembly blob rebuilt against a stale
/// `pinewasm_num` without repeating the whole corpus.
#[test]
fn the_standalone_wasm_blob_shares_the_numeric_kernels_bit_for_bit() {
    let mut rt = Runtime::load(runtime::MODULE_WASM_STANDALONE_WASM);
    let values = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        2.0,
        10.0,
        f64::MAX,
        f64::MIN_POSITIVE,
        std::f64::consts::PI,
        std::f64::consts::E,
        1.0e6,
        1.0e300,
        6.123233995736766e16,
    ];
    let seconds = [f64::NAN, 0.0, 1.0, -1.0, 0.5, 2.0, 3.0, 100.0];
    for &(id, name) in CASES {
        for &a in &values {
            for &b in &seconds {
                let wasm = rt.call(name, a, b);
                assert_same(name, id, a, b, wasm);
            }
        }
    }
}

/// The base blob must not carry the standalone WebAssembly family: its stable bytes keep
/// previously exported chart-interface artifacts reproducible,
/// and an `standalone_wasm_*` export here would mean its ~25 KiB of rodata is being
/// spliced into every artifact again. `tests/differential.rs`'s size-bound test
/// checks the same regression from the artifact side.
#[test]
fn the_base_blob_has_no_standalone_wasm_export_family() {
    let mut export_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(runtime::MODULE_WASM) {
        if let wasmparser::Payload::ExportSection(reader) = payload.expect("base blob parses") {
            for export in reader {
                let export = export.expect("export entry");
                assert_ne!(
                    export.name, "standalone_wasm_scan",
                    "the base blob must not export standalone_wasm_scan"
                );
                assert!(
                    !export.name.starts_with("standalone_wasm_"),
                    "the base blob must not export the standalone WebAssembly family, found {}",
                    export.name
                );
                export_count += 1;
            }
        }
    }
    assert!(
        export_count > 0,
        "the base blob exports its numeric surface"
    );
}

/// `pinewasm_num::raw::powi` is the platform-independent implementation
/// used by `math.round(x, p)`. Host `f64::powi` is not bit-stable across targets:
/// supported native targets can differ in the last bits for the same input.
/// Pin representative values and a broad-corpus fingerprint instead.
#[test]
fn powi_follows_the_pinned_square_and_multiply_contract() {
    for (base, power, expected) in [
        (1.0000001, -400, 0x3fef_ffac_1d98_1531),
        (10.0, 100, 0x54b2_49ad_2594_c37e),
        (10.0, -20, 0x3bc7_9ca1_0c92_4223),
        (2.0, -400, 0x26f0_0000_0000_0000),
        (-3.0, 5, 0xc06e_6000_0000_0000),
        (-3.0, -5, 0xbf70_db20_a88f_4696),
        (0.0, 0, 0x3ff0_0000_0000_0000),
        (0.0, -1, 0x7ff0_0000_0000_0000),
    ] {
        assert_eq!(
            pinewasm_num::raw::powi(base, power).to_bits(),
            expected,
            "powi({base}, {power})"
        );
    }

    // FNV-1a compresses the full sweep into one stable regression
    // value without introducing a second floating-point implementation.
    let mut fingerprint = 0xcbf2_9ce4_8422_2325u64;
    for p in -400i32..=400 {
        for base in [10.0f64, 2.0, 0.5, 1.0000001, -3.0] {
            fingerprint ^= pinewasm_num::raw::powi(base, p).to_bits();
            fingerprint = fingerprint.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    assert_eq!(fingerprint, 0xf958_36fd_b198_0f72);
}
