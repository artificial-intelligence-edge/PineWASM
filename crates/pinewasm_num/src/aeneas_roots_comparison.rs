//! Extraction roots for the generic numeric comparison helpers.

use crate::comparison::{generic, F64Model, FloatOps};

/// Include the concrete f64 model marker in the Aeneas extraction.
pub fn comparison_f64_model(_model: &F64Model) {}

/// Root the generic inclusive absolute-tolerance helper.
pub fn comparison_within_tolerance<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::within_tolerance(lhs, rhs)
}

/// Root the generic equality helper.
pub fn comparison_eq<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::eq(lhs, rhs)
}

/// Root the generic strict-less helper.
pub fn comparison_lt<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::lt(lhs, rhs)
}

/// Root the generic less-or-equal helper.
pub fn comparison_le<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::le(lhs, rhs)
}

/// Root the generic strict-greater helper.
pub fn comparison_gt<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::gt(lhs, rhs)
}

/// Root the generic greater-or-equal helper.
pub fn comparison_ge<F: FloatOps>(lhs: F, rhs: F) -> bool {
    generic::ge(lhs, rhs)
}

/// Instantiate the extracted generic helper at the concrete f64 operations.
pub fn comparison_within_tolerance_f64(lhs: f64, rhs: f64) -> bool {
    generic::within_tolerance(lhs, rhs)
}

/// Instantiate extracted generic equality at the concrete f64 operations.
pub fn comparison_eq_f64(lhs: f64, rhs: f64) -> bool {
    generic::eq(lhs, rhs)
}

/// Instantiate extracted generic strict-less at the concrete f64 operations.
pub fn comparison_lt_f64(lhs: f64, rhs: f64) -> bool {
    generic::lt(lhs, rhs)
}

/// Instantiate extracted generic less-or-equal at the concrete f64 operations.
pub fn comparison_le_f64(lhs: f64, rhs: f64) -> bool {
    generic::le(lhs, rhs)
}

/// Instantiate extracted generic strict-greater at the concrete f64 operations.
pub fn comparison_gt_f64(lhs: f64, rhs: f64) -> bool {
    generic::gt(lhs, rhs)
}

/// Instantiate extracted generic greater-or-equal at the concrete f64 operations.
pub fn comparison_ge_f64(lhs: f64, rhs: f64) -> bool {
    generic::ge(lhs, rhs)
}
