//! Checked arithmetic for the VM's pre-allocation resample-cache reservation.
//!
//! This kernel projects the requested accounting bytes. It does not measure
//! allocator usage or claim that the per-bar accounting constant is a physical
//! upper bound. The caller retains the runtime error and reservation policy.

/// Project `bars * per_bar + fixed`, rejecting either arithmetic overflow.
#[inline]
pub(crate) fn project(bars: usize, per_bar: usize, fixed: usize) -> Option<usize> {
    match bars.checked_mul(per_bar) {
        Some(bytes) => bytes.checked_add(fixed),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::project;

    /// Baseline model of the `Vm::reserve_resample_cache` projection.
    fn reference(bars: usize, per_bar: usize, fixed: usize) -> Option<usize> {
        bars.checked_mul(per_bar)
            .and_then(|bytes| bytes.checked_add(fixed))
    }

    const EDGES: [usize; 16] = [
        0,
        1,
        2,
        7,
        127,
        128,
        129,
        511,
        512,
        513,
        usize::MAX / 512,
        usize::MAX / 128,
        usize::MAX / 2,
        usize::MAX / 2 + 1,
        usize::MAX - 1,
        usize::MAX,
    ];

    #[test]
    fn projection_matches_frozen_reference_on_boundary_grid() {
        for bars in EDGES {
            for per_bar in EDGES {
                for fixed in EDGES {
                    assert_eq!(
                        project(bars, per_bar, fixed),
                        reference(bars, per_bar, fixed),
                        "bars={bars}, per_bar={per_bar}, fixed={fixed}"
                    );
                }
            }
        }
    }

    #[test]
    fn multiplication_overflow_is_rejected_before_addition() {
        assert_eq!(project(usize::MAX, 2, 0), None);
        assert_eq!(project(usize::MAX / 2 + 1, 2, 0), None);
        assert_eq!(project(usize::MAX, usize::MAX, 0), None);
    }

    #[test]
    fn addition_overflow_is_rejected() {
        assert_eq!(project(usize::MAX, 1, 1), None);
        assert_eq!(project(usize::MAX - 1, 1, 2), None);
        assert_eq!(project(1, 1, usize::MAX), None);
    }

    #[test]
    fn exact_maximum_and_zero_products_are_admitted() {
        assert_eq!(project(usize::MAX, 1, 0), Some(usize::MAX));
        assert_eq!(project(usize::MAX - 1, 1, 1), Some(usize::MAX));
        assert_eq!(project(0, usize::MAX, usize::MAX), Some(usize::MAX));
        assert_eq!(project(usize::MAX, 0, usize::MAX), Some(usize::MAX));
        assert_eq!(project(0, 0, 0), Some(0));
        assert_eq!(project(3, 128, 512), Some(896));
    }

    #[test]
    fn production_constants_have_an_exact_acceptance_boundary() {
        assert_eq!(crate::RESAMPLE_CACHE_RESERVATION_BYTES_PER_BAR, 128);
        assert_eq!(crate::RESAMPLE_CACHE_ENTRY_RESERVATION_BYTES, 512);
        let last = (usize::MAX - 512) / 128;
        assert_eq!(project(last, 128, 512), Some(last * 128 + 512));
        assert_eq!(project(last + 1, 128, 512), None);
    }
}
