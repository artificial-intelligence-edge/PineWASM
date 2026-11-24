//! The numeric core shared by the reference VM and AOT artifacts.
//!
//! Every transcendental a compiled source program can reach lives here, and
//! nowhere else. Two consumers depend on that being literally one
//! implementation:
//!
//! * `pinewasm_vm`'s `call_pure` / `aot_host::pure_eval` — the reference
//!   semantics.
//! * `pinewasm_aot`'s linked runtime module — the same functions compiled
//!   to WebAssembly and spliced into every emitted artifact, so the artifact
//!   needs no `host.pure` import (docs/aot.md §5).
//!
//! # Why not `f64::exp`
//!
//! `std`'s float methods bind to the host platform's libm. `exp`, `log`,
//! `log10`, the trig family, and `pow` are not correctly rounded by any
//! mainstream libm, and the implementations disagree in the last ulp: macOS,
//! glibc, and the Rust `libm` that `compiler_builtins` supplies on
//! `wasm32-unknown-unknown` all give different answers for some inputs. That
//! made "the same script" mean three different things depending on where it
//! ran, and it made an in-wasm implementation impossible to verify against a
//! moving baseline. Pinning the `libm` crate collapses all three to one
//! answer. Natural log goes further and is correctly rounded, including inputs
//! where the pinned libm result differs by one ULP: a binary64 evaluation with
//! a proven error bound decides almost every input, and a shared integer
//! interval kernel decides the few that lie near a rounding boundary.
//!
//! The rest of the float surface needs no such pinning and deliberately does
//! not appear here: `sqrt`, `floor`, `ceil`, `round`, `trunc`, `abs`, and
//! `fmod` (Rust's `%`) are exact operations, so every conforming
//! implementation already returns identical bits.
//!
//! # Normalization
//!
//! The `pure_*` wrappers carry the interpreter's value-level policy, not just
//! the math: an na (NaN) argument short-circuits to na, and a non-finite
//! result normalizes to na the way `Value::from_f64` does. Keeping that in
//! this crate rather than in the emitter is what lets a compiled artifact
//! replace a `host.pure` call with a single direct call.

#![no_std]
#![forbid(unsafe_code)]

pub mod comparison;
mod ln;

/// Charon/Aeneas extraction roots for the comparison kernel. Production
/// builds never enable this feature.
#[cfg(feature = "aeneas")]
pub mod aeneas_roots_comparison;

/// Bare, unnormalized kernels — the interpreter's `call_pure` applies its own
/// `Value`-level `na` handling around these.
pub mod raw {
    pub fn exp(x: f64) -> f64 {
        libm::exp(x)
    }
    pub fn ln(x: f64) -> f64 {
        super::ln::rounded_log(x)
    }
    pub fn log10(x: f64) -> f64 {
        libm::log10(x)
    }
    pub fn sin(x: f64) -> f64 {
        libm::sin(x)
    }
    pub fn cos(x: f64) -> f64 {
        libm::cos(x)
    }
    pub fn tan(x: f64) -> f64 {
        libm::tan(x)
    }
    pub fn asin(x: f64) -> f64 {
        libm::asin(x)
    }
    pub fn acos(x: f64) -> f64 {
        libm::acos(x)
    }
    pub fn atan(x: f64) -> f64 {
        libm::atan(x)
    }
    pub fn powf(x: f64, y: f64) -> f64 {
        libm::pow(x, y)
    }

    /// Degrees from radians, as `x * 180 / pi` — two operations, not a
    /// multiply by a folded `180/pi`.
    ///
    /// The two spellings round at different points. The multiply-then-divide
    /// sequence is therefore part of the public numerical contract rather
    /// than an optimization opportunity. `f64::to_degrees` uses the folded
    /// form, which is why this is spelled out rather than delegated.
    pub fn to_degrees(x: f64) -> f64 {
        x * 180.0 / core::f64::consts::PI
    }

    /// Radians from degrees, as `x * pi / 180`. As with [`to_degrees`], the
    /// evaluation sequence is explicit so all execution targets round at the
    /// same point instead of using the folded `f64::to_radians` expression.
    pub fn to_radians(x: f64) -> f64 {
        x * core::f64::consts::PI / 180.0
    }

    /// `x` raised to an integer power by square-and-multiply.
    ///
    /// The explicit loop makes the multiplication order (and therefore the
    /// exact result) PineWASM's cross-target contract. It deliberately does
    /// not delegate to host `f64::powi`: that intrinsic is unavailable in
    /// `no_std` and its last bits can differ across native platforms for some
    /// inputs.
    pub fn powi(mut x: f64, mut n: i32) -> f64 {
        let recip = n < 0;
        let mut acc = 1.0f64;
        loop {
            if n & 1 != 0 {
                acc *= x;
            }
            n /= 2;
            if n == 0 {
                break;
            }
            x *= x;
        }
        if recip {
            1.0 / acc
        } else {
            acc
        }
    }
}

/// `Value::from_f64`'s rule: anything non-finite is na.
#[inline]
fn norm(r: f64) -> f64 {
    if r.is_finite() {
        r
    } else {
        f64::NAN
    }
}

/// The `host.pure` id space (docs/aot.md §5). The ids are part of the artifact
/// ABI, which is versioned against them, and the interpreter-hosted tier speaks
/// them too. Compiled artifacts reach these functions through a linked call.
pub mod id {
    pub const EXP: u32 = 0;
    pub const LOG: u32 = 1;
    pub const LOG10: u32 = 2;
    pub const SIN: u32 = 3;
    pub const COS: u32 = 4;
    pub const TAN: u32 = 5;
    pub const ASIN: u32 = 6;
    pub const ACOS: u32 = 7;
    pub const ATAN: u32 = 8;
    pub const POW: u32 = 9;
    pub const ROUND2: u32 = 10;
    pub const REM: u32 = 11;
    /// One past the last id.
    pub const COUNT: u32 = 12;
}

macro_rules! unary {
    ($(#[$m:meta])* $name:ident, $kernel:path) => {
        $(#[$m])*
        pub fn $name(a: f64) -> f64 {
            if a.is_nan() {
                f64::NAN
            } else {
                norm($kernel(a))
            }
        }
    };
}

unary!(
    /// `math.exp`, na-normalized.
    pure_exp,
    raw::exp
);
unary!(
    /// `math.log`, na-normalized.
    pure_log,
    raw::ln
);
unary!(
    /// `math.log10`, na-normalized.
    pure_log10,
    raw::log10
);
unary!(
    /// `math.sin`, na-normalized.
    pure_sin,
    raw::sin
);
unary!(
    /// `math.cos`, na-normalized.
    pure_cos,
    raw::cos
);
unary!(
    /// `math.tan`, na-normalized.
    pure_tan,
    raw::tan
);
unary!(
    /// `math.asin`, na-normalized.
    pure_asin,
    raw::asin
);
unary!(
    /// `math.acos`, na-normalized.
    pure_acos,
    raw::acos
);
unary!(
    /// `math.atan`, na-normalized.
    pure_atan,
    raw::atan
);

/// `math.pow(a, b)`, na-normalized in both arguments.
pub fn pure_pow(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        norm(raw::powf(a, b))
    }
}

/// `math.round(value, precision)`: rounded to `precision` decimal digits,
/// half away from zero. `precision` arrives as an f64 because that is the
/// shape of the compiled call; the interpreter has already typed it as an
/// int.
///
/// `precision` is clamped to the language contract's `[0, 100]` range: a
/// runtime value below 0 rounds to whole digits, and one above 100 keeps every
/// digit. A *statically-known* out-of-range precision never reaches here —
/// the semantic layer rejects it at compile time with E0248.
pub fn pure_round2(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    let p = clamp_precision(b as i64);
    let factor = raw::powi(10.0, p);
    let scaled = a * factor;
    if scaled.is_finite() {
        norm(libm::round(scaled) / factor)
    } else {
        f64::NAN
    }
}

/// The interpreter's `math.round` precision clamp, shared so the two sides
/// cannot drift. The valid range is `[0, 100]`; a runtime value outside it is
/// clamped (below 0 → 0, above 100 → 100), while a literal or otherwise
/// statically-known out-of-range precision is a compile error (E0248) rather
/// than reaching this clamp.
pub fn clamp_precision(p: i64) -> i32 {
    p.clamp(0, 100) as i32
}

/// Floating remainder, Rust's `%`. Exact in every conforming implementation,
/// but WebAssembly has no `f64.rem` instruction, so compiled artifacts call
/// this rather than expanding `a - trunc(a / b) * b` (which loses low bits
/// for large quotients).
pub fn pure_rem(a: f64, b: f64) -> f64 {
    norm(libm::fmod(a, b))
}

/// Evaluate by id — the shape the compiled artifact's dispatch and the
/// interpreter's host-side backing share. Unknown ids are na.
pub fn pure(func_id: u32, a: f64, b: f64) -> f64 {
    match func_id {
        id::EXP => pure_exp(a),
        id::LOG => pure_log(a),
        id::LOG10 => pure_log10(a),
        id::SIN => pure_sin(a),
        id::COS => pure_cos(a),
        id::TAN => pure_tan(a),
        id::ASIN => pure_asin(a),
        id::ACOS => pure_acos(a),
        id::ATAN => pure_atan(a),
        id::POW => pure_pow(a, b),
        id::ROUND2 => pure_round2(a, b),
        id::REM => pure_rem(a, b),
        _ => f64::NAN,
    }
}
