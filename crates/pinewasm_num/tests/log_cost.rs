//! A regression gate on what a correctly rounded `math.log` costs.
//!
//! `math.log` is correctly rounded. Deciding every input with the exact
//! integer interval kernel made it about ten times slower per call than the
//! pinned `libm` logarithm it replaced. Correct rounding does not need that
//! cost: a fast evaluation with a proven error bound decides almost every
//! input, and the interval kernel only has to settle the inputs whose
//! logarithm lies near a rounding boundary (see `src/ln.rs`).
//!
//! This test fails when the correctly rounded logarithm again costs much more
//! than the pinned `libm` logarithm on the same inputs. Both sides run on the
//! same inputs in alternating order, and the test compares medians of several
//! rounds, so machine load affects both alike. The limit is deliberately
//! loose. In an optimized build the correctly rounded logarithm costs about
//! two `libm` logarithms (about 3.5 without optimization), while deciding
//! every input with the interval kernel cost over 100.

use std::hint::black_box;
use std::time::{Duration, Instant};

/// The largest accepted median cost of `math.log` relative to `libm::log`.
/// Unoptimized builds roughly double the relative cost of the fast
/// evaluation's extra operations, so they get twice the limit.
const LIMIT: f64 = if cfg!(debug_assertions) { 8.0 } else { 4.0 };
const ROUNDS: usize = 7;
const REPEATS: usize = 8;

/// SplitMix64, so the inputs are the same on every run and platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / (1_u64 << 53) as f64)
    }
}

/// Input shapes: distance terms `1 + |a - b|` with `a` and `b` in [0, 1],
/// arguments just above one, and positive values across many binades.
fn inputs() -> Vec<f64> {
    let mut rng = Rng(0x6c6f_675f_636f_7374);
    let mut out = Vec::with_capacity(4096);
    for _ in 0..2048 {
        out.push(1.0 + (rng.unit() - rng.unit()).abs());
    }
    for _ in 0..1024 {
        out.push(1.0 + rng.unit() * 0.01);
    }
    for _ in 0..1024 {
        let exponent = (rng.next() % 121) as i32 - 60;
        out.push((1.0 + rng.unit()) * 2.0_f64.powi(exponent));
    }
    out
}

fn time(log: fn(f64) -> f64, inputs: &[f64]) -> Duration {
    let start = Instant::now();
    let mut acc = 0.0;
    for _ in 0..REPEATS {
        for &x in inputs {
            acc += log(black_box(x));
        }
    }
    black_box(acc);
    start.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
fn correctly_rounded_log_costs_about_as_much_as_libm_log() {
    let inputs = inputs();
    for &x in &inputs {
        assert!(x.is_finite() && x > 0.0);
    }
    // Warm both paths and the inputs into cache before measuring.
    time(pinewasm_num::raw::ln, &inputs);
    time(libm::log, &inputs);

    let mut rounded = Vec::with_capacity(ROUNDS);
    let mut reference = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            rounded.push(time(pinewasm_num::raw::ln, &inputs));
            reference.push(time(libm::log, &inputs));
        } else {
            reference.push(time(libm::log, &inputs));
            rounded.push(time(pinewasm_num::raw::ln, &inputs));
        }
    }
    let rounded = median(rounded);
    let reference = median(reference);
    let ratio = rounded.as_secs_f64() / reference.as_secs_f64();
    let calls = (inputs.len() * REPEATS) as f64;
    eprintln!(
        "math.log {:.1} ns/call, libm::log {:.1} ns/call, ratio {ratio:.2} (limit {LIMIT})",
        rounded.as_secs_f64() * 1e9 / calls,
        reference.as_secs_f64() * 1e9 / calls,
    );
    assert!(
        ratio <= LIMIT,
        "math.log costs {ratio:.2} times libm::log on the same inputs (limit {LIMIT}); \
         the exact interval kernel is deciding too many inputs"
    );
}
