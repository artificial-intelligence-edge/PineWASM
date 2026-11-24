//! Checked arena arithmetic for the `standalone_wasm` allocator family.
//!
//! These helpers are pure, safe and target-independent, so they live apart
//! from that module's raw linear-memory access. The `aeneas` feature compiles
//! them without the rest of the standalone WebAssembly family so they can be
//! extracted for verification; production builds reach them only through
//! `standalone_wasm`.
//!
//! The bodies avoid `Result::ok`, `u32::is_power_of_two` and
//! `i32::try_from(u32)`, which the Aeneas standard library does not model.
//! Each form is equivalent to the one it replaces (see the tests).

/// Checked alignment and end-address arithmetic shared by both arena entry
/// points and the native safety harness.
pub(crate) fn allocation_bounds(cursor: u32, size: usize, align: usize) -> Option<(u32, u32)> {
    let align = align.max(1);
    if !align.is_power_of_two() {
        return None;
    }
    let Ok(align) = u32::try_from(align) else {
        return None;
    };
    let mask = align - 1;
    let base = cursor.checked_add(mask)? & !mask;
    let Ok(size) = u32::try_from(size) else {
        return None;
    };
    Some((base, base.checked_add(size)?))
}

/// Compute the non-negative relocation result accepted by the signed Wasm ABI.
/// A larger unsigned displacement must fail before memory growth or mutation.
pub(crate) fn relocation_delta(arena_lo: u32, target: u32) -> Option<i32> {
    let delta = target.checked_sub(arena_lo)?;
    if delta > i32::MAX as u32 {
        return None;
    }
    Some(delta as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// The pre-refactor body of [`allocation_bounds`], kept verbatim.
    fn old_allocation_bounds(cursor: u32, size: usize, align: usize) -> Option<(u32, u32)> {
        let align = u32::try_from(align.max(1)).ok()?;
        if !align.is_power_of_two() {
            return None;
        }
        let mask = align - 1;
        let base = cursor.checked_add(mask)? & !mask;
        let size = u32::try_from(size).ok()?;
        Some((base, base.checked_add(size)?))
    }

    /// The pre-refactor body of [`relocation_delta`], kept verbatim.
    fn old_relocation_delta(arena_lo: u32, target: u32) -> Option<i32> {
        i32::try_from(target.checked_sub(arena_lo)?).ok()
    }

    /// Deterministic 64-bit LCG (Knuth MMIX constants).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
    }

    fn u32_edges() -> Vec<u32> {
        let mut v = std::vec![0, 1, 2, 3, 7, 8, 9, 4095, 4096, 65_535, 65_536];
        for k in 0..32 {
            let p = 1u32 << k;
            v.extend([p - 1, p, p.wrapping_add(1)]);
        }
        let max = i32::MAX as u32;
        v.extend([max - 1, max, max + 1, max + 2, u32::MAX - 8, u32::MAX - 7]);
        v.extend([u32::MAX - 6, u32::MAX - 1, u32::MAX]);
        v
    }

    fn usize_edges() -> Vec<usize> {
        let mut v: Vec<usize> = u32_edges().into_iter().map(|x| x as usize).collect();
        for k in 0..usize::BITS {
            let p = 1usize << k;
            v.extend([p - 1, p, p.wrapping_add(1)]);
        }
        v.extend([usize::MAX - 1, usize::MAX]);
        if usize::BITS > u32::BITS {
            let over = u32::MAX as usize + 1;
            v.extend([over, over + 1, over + 7]);
        }
        v
    }

    #[test]
    fn allocation_bounds_matches_pre_refactor_body() {
        let cursors = u32_edges();
        let sizes = usize_edges();
        let aligns = usize_edges();
        // Miri runs a strided sample of the same families; native runs cover all of them.
        let (stride, dense, random) = if cfg!(miri) {
            (16, 8, 2_000)
        } else {
            (1, 70, 200_000)
        };
        let mut cases = 0u64;
        for &cursor in cursors.iter().step_by(stride) {
            for &size in sizes.iter().step_by(stride) {
                for &align in aligns.iter().step_by(stride) {
                    assert_eq!(
                        allocation_bounds(cursor, size, align),
                        old_allocation_bounds(cursor, size, align),
                        "cursor={cursor} size={size} align={align}"
                    );
                    cases += 1;
                }
            }
        }
        // Dense small window: every cursor, size and alignment below 70.
        for cursor in 0..dense {
            for size in 0..dense as usize {
                for align in 0..dense as usize {
                    assert_eq!(
                        allocation_bounds(cursor, size, align),
                        old_allocation_bounds(cursor, size, align)
                    );
                    cases += 1;
                }
            }
        }
        let mut rng = Lcg(0x5eed_a11c);
        for _ in 0..random {
            let cursor = rng.next() as u32;
            let size = match rng.next() % 3 {
                0 => rng.next() as usize,
                1 => (rng.next() % 0x1_0000) as usize,
                _ => rng.next() as u32 as usize,
            };
            let align = match rng.next() % 3 {
                0 => rng.next() as usize,
                1 => 1usize << (rng.next() % u64::from(usize::BITS)),
                _ => (rng.next() % 64) as usize,
            };
            assert_eq!(
                allocation_bounds(cursor, size, align),
                old_allocation_bounds(cursor, size, align),
                "cursor={cursor} size={size} align={align}"
            );
            cases += 1;
        }
        assert!(cfg!(miri) || cases > 1_000_000, "{cases} cases");
    }

    #[test]
    fn relocation_delta_matches_pre_refactor_body() {
        let edges = u32_edges();
        let (stride, random) = if cfg!(miri) {
            (4, 2_000)
        } else {
            (1, 1_000_000)
        };
        for &lo in edges.iter().step_by(stride) {
            for &target in edges.iter().step_by(stride) {
                assert_eq!(
                    relocation_delta(lo, target),
                    old_relocation_delta(lo, target),
                    "arena_lo={lo} target={target}"
                );
            }
        }
        let mut rng = Lcg(0xde17a);
        for _ in 0..random {
            let lo = rng.next() as u32;
            let target = if rng.next() % 2 == 0 {
                rng.next() as u32
            } else {
                lo.wrapping_add((rng.next() as u32) >> (rng.next() % 32))
            };
            assert_eq!(
                relocation_delta(lo, target),
                old_relocation_delta(lo, target),
                "arena_lo={lo} target={target}"
            );
        }
    }

    #[test]
    fn allocation_bounds_and_relocation_delta_fixed_cases() {
        assert_eq!(allocation_bounds(1, 7, 8), Some((8, 15)));
        assert_eq!(allocation_bounds(8, 0, 8), Some((8, 8)));
        assert_eq!(allocation_bounds(5, 1, 0), Some((5, 6)));
        assert_eq!(allocation_bounds(u32::MAX - 6, 0, 8), None);
        assert_eq!(allocation_bounds(1, 1, 3), None);
        assert_eq!(allocation_bounds(0, 0, 1 << 31), Some((0, 0)));
        if usize::BITS > u32::BITS {
            assert_eq!(allocation_bounds(8, u32::MAX as usize + 1, 8), None);
            assert_eq!(allocation_bounds(0, 0, u32::MAX as usize + 1), None);
        }
        assert_eq!(relocation_delta(8, 16), Some(8));
        assert_eq!(relocation_delta(16, 8), None);
        assert_eq!(relocation_delta(0, i32::MAX as u32), Some(i32::MAX));
        assert_eq!(relocation_delta(0, i32::MAX as u32 + 1), None);
        assert_eq!(relocation_delta(0, u32::MAX), None);
    }
}
