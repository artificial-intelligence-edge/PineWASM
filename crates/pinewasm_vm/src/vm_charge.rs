//! Integer kernels behind instruction charging and the slot-history
//! ring: the two instruction counters, the per-run ring size, and the ring
//! read index.
//!
//! These are free functions over plain integers so Charon/Aeneas can extract
//! them (inherent methods cannot be named as extraction roots, and the
//! surrounding `Vm` methods carry `f64`, `HashMap` and `String` state). The
//! `Vm` methods delegate here and keep building the `RuntimeError`s, so the
//! observable behavior is unchanged. The AOT-path ring size (saturating add,
//! unchecked `next_power_of_two`) lives next to its only definition,
//! `pinewasm_csil::history::history_ring_aot`.

/// Which instruction ceiling a charge would have crossed. The caller turns
/// this into the `RuntimeError` (`InstructionLimitExceeded` or
/// `RunInstructionLimitExceeded`).
///
/// The `charon::rename` attribute only affects the Charon/Aeneas extraction
/// output; ordinary builds never set `cfg(charon)`.
#[cfg_attr(charon, charon::rename("DispatchChargeLimit"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChargeLimit {
    /// The per-bar budget is short.
    Bar,
    /// The cumulative per-run counter is short.
    Run,
}

/// Charge `cost` against both independent instruction ceilings. Per-bar
/// exhaustion wins when both counters are short. On error neither counter
/// changes; on success both drop by exactly `cost`.
#[inline]
pub(crate) fn charge_counters(
    budget: &mut u64,
    remaining: &mut u64,
    cost: u64,
) -> Result<(), ChargeLimit> {
    if *budget < cost {
        return Err(ChargeLimit::Bar);
    }
    if *remaining < cost {
        return Err(ChargeLimit::Run);
    }
    *budget -= cost;
    *remaining -= cost;
    Ok(())
}

/// Mirror an extra charge into one instruction's profiled `units` total.
#[inline]
pub(crate) fn profile_extra_units(units: &mut [u64], pc: usize, extra: u64) {
    units[pc] += extra;
}

/// Ring size for a fresh run's slot history: the smallest power of two
/// strictly greater than `depth`, or `None` when that does not fit in `u32`.
// `let ... else` instead of `?`: Aeneas's model of `?` on `Option` (the
// `Try`/`FromResidual` impls) is unverified, so the item avoids it.
#[allow(clippy::question_mark)]
#[inline]
pub(crate) fn ring_size(depth: u32) -> Option<u32> {
    let Some(cells) = depth.checked_add(1) else {
        return None;
    };
    cells.checked_next_power_of_two()
}

/// Ring cell holding the value `offset` bars before `bar`, or `None` when
/// that bar precedes the series start.
#[allow(clippy::question_mark)]
#[inline]
pub(crate) fn ring_read(bar: usize, offset: usize, mask: usize) -> Option<usize> {
    let Some(index) = bar.checked_sub(offset) else {
        return None;
    };
    Some(index & mask)
}

#[cfg(test)]
mod tests {
    use super::{charge_counters, profile_extra_units, ring_read, ring_size, ChargeLimit};

    /// Direct form of `Vm::charge_instructions`, used as the test expectation,
    /// with the error construction reduced to its kind.
    fn charge_reference(
        budget: &mut u64,
        remaining: &mut u64,
        cost: u64,
    ) -> Result<(), ChargeLimit> {
        if *budget < cost {
            return Err(ChargeLimit::Bar);
        }
        if *remaining < cost {
            return Err(ChargeLimit::Run);
        }
        *budget -= cost;
        *remaining -= cost;
        Ok(())
    }

    /// Direct ring sizing used by `Vm::new_owned`, as the test expectation.
    fn ring_size_reference(depth: u32) -> Option<usize> {
        depth
            .checked_add(1)
            .and_then(u32::checked_next_power_of_two)
            .map(|ring| ring as usize)
    }

    /// Direct `ReadUserSlot` index computation, as the test expectation.
    fn ring_read_reference(bar: usize, offset: u32, mask: usize) -> Option<usize> {
        bar.checked_sub(offset as usize).map(|index| index & mask)
    }

    const U64_EDGES: [u64; 10] = [
        0,
        1,
        2,
        3,
        7,
        1 << 20,
        u64::MAX / 2,
        u64::MAX - 1,
        u64::MAX,
        1 << 63,
    ];

    #[test]
    fn charge_counters_matches_reference_on_edges() {
        for &budget in &U64_EDGES {
            for &remaining in &U64_EDGES {
                for &cost in &U64_EDGES {
                    let (mut b_new, mut r_new) = (budget, remaining);
                    let (mut b_old, mut r_old) = (budget, remaining);
                    let new = charge_counters(&mut b_new, &mut r_new, cost);
                    let old = charge_reference(&mut b_old, &mut r_old, cost);
                    assert_eq!(
                        new, old,
                        "budget={budget} remaining={remaining} cost={cost}"
                    );
                    assert_eq!((b_new, r_new), (b_old, r_old));
                }
            }
        }
    }

    #[test]
    fn charge_counters_bar_wins_and_errors_do_not_mutate() {
        let (mut b, mut r) = (1, 1);
        assert_eq!(charge_counters(&mut b, &mut r, 2), Err(ChargeLimit::Bar));
        assert_eq!((b, r), (1, 1));
        let (mut b, mut r) = (5, 1);
        assert_eq!(charge_counters(&mut b, &mut r, 2), Err(ChargeLimit::Run));
        assert_eq!((b, r), (5, 1));
        let (mut b, mut r) = (5, 9);
        assert_eq!(charge_counters(&mut b, &mut r, 5), Ok(()));
        assert_eq!((b, r), (0, 4));
    }

    #[test]
    fn profile_extra_units_adds_in_place() {
        let mut units = vec![0, 3, 9];
        profile_extra_units(&mut units, 1, 4);
        assert_eq!(units, [0, 7, 9]);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn profile_extra_units_panics_out_of_range_like_the_original() {
        let mut units = vec![0_u64; 2];
        profile_extra_units(&mut units, 2, 1);
    }

    #[test]
    fn ring_size_matches_reference() {
        let mut depths: Vec<u32> = (0..=4096).collect();
        for shift in 0..32 {
            let p = 1_u32 << shift;
            depths.extend([p - 1, p, p.saturating_add(1)]);
        }
        depths.extend([
            u32::MAX - 1,
            u32::MAX,
            (1 << 31) - 1,
            1 << 31,
            (1 << 31) + 1,
        ]);
        for depth in depths {
            assert_eq!(
                ring_size(depth).map(|ring| ring as usize),
                ring_size_reference(depth),
                "depth={depth}"
            );
        }
        assert_eq!(ring_size(0), Some(1));
        assert_eq!(ring_size((1 << 31) - 1), Some(1 << 31));
        assert_eq!(ring_size(1 << 31), None);
        assert_eq!(ring_size(u32::MAX), None);
    }

    /// The fresh-run ring (`ring_size`) and the AOT-host ring
    /// (`CsilProgram::history_ring`: saturating add, unchecked
    /// `next_power_of_two`) agree wherever the checked path succeeds.
    #[test]
    fn ring_size_agrees_with_aot_ring_below_2_pow_31() {
        let depths = (0..=4096_u32).chain([(1 << 30) - 1, 1 << 30, (1 << 31) - 1]);
        for depth in depths {
            assert_eq!(
                ring_size(depth),
                Some(depth.saturating_add(1).next_power_of_two()),
                "depth={depth}"
            );
        }
    }

    #[test]
    fn ring_read_matches_reference() {
        let bars = [
            0_usize,
            1,
            2,
            5,
            63,
            64,
            65,
            1000,
            usize::MAX - 1,
            usize::MAX,
        ];
        let offsets = [0_u32, 1, 2, 5, 64, 1000, u32::MAX];
        let masks = [0_usize, 1, 3, 63, 1023, usize::MAX];
        for &bar in &bars {
            for &offset in &offsets {
                for &mask in &masks {
                    assert_eq!(
                        ring_read(bar, offset as usize, mask),
                        ring_read_reference(bar, offset, mask),
                        "bar={bar} offset={offset} mask={mask}"
                    );
                }
            }
        }
        assert_eq!(ring_read(3, 4, 7), None);
        assert_eq!(ring_read(10, 1, 7), Some(1));
    }
}
