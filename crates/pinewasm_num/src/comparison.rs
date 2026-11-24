//! PineWASM float comparisons, separate from the VM's raw IEEE primitives.
//!
//! PineWASM float comparisons treat finite operands whose absolute difference
//! is at most EPSILON (1e-10) as equal, including the boundary at zero.
//! Preserve IEEE equality for identical infinities and signed zero. Callers
//! retain their own missing-value policy; two integers must never enter here.

/// Inclusive absolute tolerance for PineWASM float comparisons.
pub const EPSILON: f64 = 1e-10;

// Keep the shipped expansion a sequence of primitive f64 operations. The
// same helper bodies expand against FloatOps only for tests and extraction.
macro_rules! f64_op {
    (epsilon) => {
        EPSILON
    };
    (sub $lhs:expr, $rhs:expr) => {
        $lhs - $rhs
    };
    (abs $value:expr) => {
        $value.abs()
    };
    (eq $lhs:expr, $rhs:expr) => {
        $lhs == $rhs
    };
    (lt $lhs:expr, $rhs:expr) => {
        $lhs < $rhs
    };
    (le $lhs:expr, $rhs:expr) => {
        $lhs <= $rhs
    };
    (gt $lhs:expr, $rhs:expr) => {
        $lhs > $rhs
    };
}

/// The operations needed by the generic comparison twins. This trait exists
/// only for unit tests and Charon/Aeneas extraction; default builds expose no
/// additional API.
#[cfg(any(test, feature = "aeneas"))]
pub trait FloatOps: Copy {
    fn epsilon() -> Self;
    fn sub(lhs: Self, rhs: Self) -> Self;
    fn abs(value: Self) -> Self;
    fn equal(lhs: Self, rhs: Self) -> bool;
    fn less(lhs: Self, rhs: Self) -> bool;
    fn less_equal(lhs: Self, rhs: Self) -> bool;
    fn greater(lhs: Self, rhs: Self) -> bool;
}

/// The generic operation dictionary; each operation is abstract while the
/// function body and operation order stay shared with the shipped f64 code.
#[cfg(any(test, feature = "aeneas"))]
macro_rules! generic_op {
    (epsilon) => {
        F::epsilon()
    };
    (sub $lhs:expr, $rhs:expr) => {
        F::sub($lhs, $rhs)
    };
    (abs $value:expr) => {
        F::abs($value)
    };
    (eq $lhs:expr, $rhs:expr) => {
        F::equal($lhs, $rhs)
    };
    (lt $lhs:expr, $rhs:expr) => {
        F::less($lhs, $rhs)
    };
    (le $lhs:expr, $rhs:expr) => {
        F::less_equal($lhs, $rhs)
    };
    (gt $lhs:expr, $rhs:expr) => {
        F::greater($lhs, $rhs)
    };
}

/// Define the six helpers once over an operation dictionary. For the default
/// f64 expansion, macro expansion reduces to the original primitive bodies.
macro_rules! comparison_helpers {
    ($ops:ident, $vis:vis, $ty:ty, [$($generic:tt)*]) => {
        #[inline]
        $vis fn within_tolerance $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(le $ops!(abs $ops!(sub lhs, rhs)), $ops!(epsilon))
        }

        #[inline]
        $vis fn eq $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(eq lhs, rhs) || within_tolerance(lhs, rhs)
        }

        #[inline]
        $vis fn lt $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(lt lhs, rhs) && !eq(lhs, rhs)
        }

        #[inline]
        $vis fn le $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(lt lhs, rhs) || eq(lhs, rhs)
        }

        #[inline]
        $vis fn gt $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(gt lhs, rhs) && !eq(lhs, rhs)
        }

        #[inline]
        $vis fn ge $($generic)* (lhs: $ty, rhs: $ty) -> bool {
            $ops!(gt lhs, rhs) || eq(lhs, rhs)
        }
    };
}

comparison_helpers!(f64_op, pub, f64, []);

/// Small opaque boundary for the concrete binary64 operations. Charon/Aeneas
/// extraction models these functions; the shipped helpers use the same
/// `f64_op` dictionary directly, without calls through this module.
#[cfg(any(test, feature = "aeneas"))]
mod f64_ops {
    pub(super) fn epsilon() -> f64 {
        super::EPSILON
    }
    pub(super) fn sub(lhs: f64, rhs: f64) -> f64 {
        f64_op!(sub lhs, rhs)
    }
    pub(super) fn abs(value: f64) -> f64 {
        f64_op!(abs value)
    }
    pub(super) fn equal(lhs: f64, rhs: f64) -> bool {
        f64_op!(eq lhs, rhs)
    }
    pub(super) fn less(lhs: f64, rhs: f64) -> bool {
        f64_op!(lt lhs, rhs)
    }
    pub(super) fn less_equal(lhs: f64, rhs: f64) -> bool {
        f64_op!(le lhs, rhs)
    }
    pub(super) fn greater(lhs: f64, rhs: f64) -> bool {
        f64_op!(gt lhs, rhs)
    }
}

#[cfg(any(test, feature = "aeneas"))]
impl FloatOps for f64 {
    fn epsilon() -> Self {
        f64_ops::epsilon()
    }
    fn sub(lhs: Self, rhs: Self) -> Self {
        f64_ops::sub(lhs, rhs)
    }
    fn abs(value: Self) -> Self {
        f64_ops::abs(value)
    }
    fn equal(lhs: Self, rhs: Self) -> bool {
        f64_ops::equal(lhs, rhs)
    }
    fn less(lhs: Self, rhs: Self) -> bool {
        f64_ops::less(lhs, rhs)
    }
    fn less_equal(lhs: Self, rhs: Self) -> bool {
        f64_ops::less_equal(lhs, rhs)
    }
    fn greater(lhs: Self, rhs: Self) -> bool {
        f64_ops::greater(lhs, rhs)
    }
}

#[cfg(any(test, feature = "aeneas"))]
pub(crate) mod generic {
    use super::FloatOps;

    comparison_helpers!(generic_op, pub(crate), F, [<F: FloatOps>]);
}

/// Extraction-only marker for the binary64 representation used by the extraction model.
#[cfg(feature = "aeneas")]
#[allow(dead_code)]
pub struct F64Model;

#[cfg(test)]
mod tests {
    use super::*;

    fn original_within_tolerance(lhs: f64, rhs: f64) -> bool {
        (lhs - rhs).abs() <= EPSILON
    }

    fn original_eq(lhs: f64, rhs: f64) -> bool {
        lhs == rhs || original_within_tolerance(lhs, rhs)
    }

    fn original_lt(lhs: f64, rhs: f64) -> bool {
        lhs < rhs && !original_eq(lhs, rhs)
    }

    fn original_le(lhs: f64, rhs: f64) -> bool {
        lhs < rhs || original_eq(lhs, rhs)
    }

    fn original_gt(lhs: f64, rhs: f64) -> bool {
        lhs > rhs && !original_eq(lhs, rhs)
    }

    fn original_ge(lhs: f64, rhs: f64) -> bool {
        lhs > rhs || original_eq(lhs, rhs)
    }

    fn original_results(lhs: f64, rhs: f64) -> [bool; 6] {
        [
            original_within_tolerance(lhs, rhs),
            original_eq(lhs, rhs),
            original_lt(lhs, rhs),
            original_le(lhs, rhs),
            original_gt(lhs, rhs),
            original_ge(lhs, rhs),
        ]
    }

    fn shipped_results(lhs: f64, rhs: f64) -> [bool; 6] {
        [
            within_tolerance(lhs, rhs),
            eq(lhs, rhs),
            lt(lhs, rhs),
            le(lhs, rhs),
            gt(lhs, rhs),
            ge(lhs, rhs),
        ]
    }

    fn generic_results(lhs: f64, rhs: f64) -> [bool; 6] {
        [
            generic::within_tolerance(lhs, rhs),
            generic::eq(lhs, rhs),
            generic::lt(lhs, rhs),
            generic::le(lhs, rhs),
            generic::gt(lhs, rhs),
            generic::ge(lhs, rhs),
        ]
    }

    fn assert_f64_twin(lhs: f64, rhs: f64) {
        let expected = original_results(lhs, rhs);
        assert_eq!(
            shipped_results(lhs, rhs),
            expected,
            "shipped ({lhs:?}, {rhs:?})"
        );
        assert_eq!(
            generic_results(lhs, rhs),
            expected,
            "generic ({lhs:?}, {rhs:?})"
        );

        assert_eq!(f64_ops::epsilon().to_bits(), EPSILON.to_bits());
        assert_eq!(f64_ops::sub(lhs, rhs).to_bits(), (lhs - rhs).to_bits());
        assert_eq!(f64_ops::abs(lhs).to_bits(), lhs.abs().to_bits());
        assert_eq!(f64_ops::equal(lhs, rhs), lhs == rhs);
        assert_eq!(f64_ops::less(lhs, rhs), lhs < rhs);
        assert_eq!(f64_ops::less_equal(lhs, rhs), lhs <= rhs);
        assert_eq!(f64_ops::greater(lhs, rhs), lhs > rhs);
    }

    #[test]
    fn generic_twin_matches_original_at_both_signed_tolerance_boundaries() {
        let below = f64::from_bits(EPSILON.to_bits() - 1);
        let at = EPSILON;
        let above = f64::from_bits(EPSILON.to_bits() + 1);
        for value in [below, at, above, -below, -at, -above] {
            assert_f64_twin(value, 0.0);
            assert_f64_twin(0.0, value);
        }

        assert!(within_tolerance(below, 0.0));
        assert!(within_tolerance(at, 0.0));
        assert!(!within_tolerance(above, 0.0));
        assert!(within_tolerance(-below, 0.0));
        assert!(within_tolerance(-at, 0.0));
        assert!(!within_tolerance(-above, 0.0));
    }

    #[test]
    fn generic_twin_matches_original_for_ieee_special_values_and_operand_orders() {
        let quiet_nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let negative_nan = f64::from_bits(0xfff8_1234_5678_9abc);
        let values = [
            0.0,
            -0.0,
            EPSILON,
            -EPSILON,
            f64::from_bits(EPSILON.to_bits() - 1),
            f64::from_bits(EPSILON.to_bits() + 1),
            -f64::from_bits(EPSILON.to_bits() - 1),
            -f64::from_bits(EPSILON.to_bits() + 1),
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            quiet_nan,
            negative_nan,
        ];

        for lhs in values {
            for rhs in values {
                assert_f64_twin(lhs, rhs);
            }
        }
    }

    #[test]
    fn generic_tolerance_is_nontransitive() {
        let a = 0.0;
        let b = f64::from_bits(0x3dd0_0000_0000_0000); // 2^-34
        let c = f64::from_bits(0x3de0_0000_0000_0000); // 2^-33

        assert!(eq(a, b));
        assert!(eq(b, c));
        assert!(!eq(a, c));
        assert!(generic::eq(a, b));
        assert!(generic::eq(b, c));
        assert!(!generic::eq(a, c));
        assert_f64_twin(a, b);
        assert_f64_twin(b, c);
        assert_f64_twin(a, c);
    }

    #[test]
    fn comparison_lattice_matches_expected_outcomes() {
        // Expected outcomes for each (bar, base, delta) case.
        // Order: eq, ne, lt, le, gt, ge; base arrays shift by bar parity.
        let expectations = [
            ["011100", "100101", "100101", "100101", "100101", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
            ["011100", "100101", "100101", "100101", "100101", "010011"],
            ["011100", "011100", "100101", "100101", "010011", "010011"],
        ];
        for bar in 0..2 {
            for (base_index, base) in [0.0, 1.0, -1.0, 0.3].into_iter().enumerate() {
                let rhs = base + bar as f64;
                for (delta_index, delta) in [-1.5e-10, -1e-10, -5e-11, 5e-11, 1e-10, 1.5e-10]
                    .into_iter()
                    .enumerate()
                {
                    let lhs = rhs + delta;
                    let actual = [
                        eq(lhs, rhs),
                        !eq(lhs, rhs),
                        lt(lhs, rhs),
                        le(lhs, rhs),
                        gt(lhs, rhs),
                        ge(lhs, rhs),
                    ];
                    for (index, expected) in expectations[bar * 4 + base_index][delta_index]
                        .bytes()
                        .enumerate()
                    {
                        assert_eq!(
                            actual[index],
                            expected == b'1',
                            "bar={bar} base={base} delta={delta} operator={index}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn nonfinite_and_signed_zero_preserve_ieee_boundary_behavior() {
        // Boundary behaviour for IEEE special values and signed zero.
        for value in [0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(eq(value, value));
            assert!(le(value, value));
            assert!(ge(value, value));
            assert!(!lt(value, value));
            assert!(!gt(value, value));
        }
        assert!(eq(0.0, -0.0));
        assert!(!eq(f64::INFINITY, f64::NEG_INFINITY));
        assert!(!eq(f64::MAX, -f64::MAX));
        assert!(!eq(f64::NAN, 0.0));
        assert!(!le(f64::NAN, 0.0));
    }
}
