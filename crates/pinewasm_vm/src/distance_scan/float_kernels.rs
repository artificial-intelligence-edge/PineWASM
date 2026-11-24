use std::fmt;

pub(crate) trait DistanceMergeOps: Copy {
    fn min(left: Self, right: Self) -> Self;
    fn max(left: Self, right: Self) -> Self;
}

pub(crate) trait DistanceRejectOps: DistanceMergeOps {
    fn zero() -> Self;
    fn one() -> Self;
    fn margin() -> Self;
    fn sub(left: Self, right: Self) -> Self;
    fn abs(value: Self) -> Self;
    fn add(left: Self, right: Self) -> Self;
    fn add_assign(left: &mut Self, right: Self);
    fn mul(left: Self, right: Self) -> Self;
    fn gt(left: Self, right: Self) -> bool;
    fn le(left: Self, right: Self) -> bool;
    fn lt(left: Self, right: Self) -> bool;
    fn is_finite(value: Self) -> bool;
    fn log(value: Self) -> Self;
}

/// Five scalar coordinates in the operation-dictionary shape used by the
/// extracted kernel and by the VM's array adapter.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DistanceFeatures<F> {
    pub(crate) f0: F,
    pub(crate) f1: F,
    pub(crate) f2: F,
    pub(crate) f3: F,
    pub(crate) f4: F,
}

impl<F: Copy> DistanceFeatures<F> {
    pub(crate) fn splat(value: F) -> Self {
        Self {
            f0: value,
            f1: value,
            f2: value,
            f3: value,
            f4: value,
        }
    }

    pub(crate) fn get(&self, index: usize) -> F {
        match index {
            0 => self.f0,
            1 => self.f1,
            2 => self.f2,
            3 => self.f3,
            4 => self.f4,
            _ => panic!("distance feature index out of bounds"),
        }
    }

    pub(crate) fn set(&mut self, index: usize, value: F) {
        match index {
            0 => self.f0 = value,
            1 => self.f1 = value,
            2 => self.f2 = value,
            3 => self.f3 = value,
            4 => self.f4 = value,
            _ => panic!("distance feature index out of bounds"),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DistanceNode<F> {
    pub(crate) low: DistanceFeatures<F>,
    pub(crate) high: DistanceFeatures<F>,
    pub(crate) bounded: bool,
}

impl<F: fmt::Debug + Copy> fmt::Debug for DistanceNode<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let low = [
            self.low.f0,
            self.low.f1,
            self.low.f2,
            self.low.f3,
            self.low.f4,
        ];
        let high = [
            self.high.f0,
            self.high.f1,
            self.high.f2,
            self.high.f3,
            self.high.f4,
        ];
        f.debug_struct("Node")
            .field("min", &low)
            .field("max", &high)
            .field("bounded", &self.bounded)
            .finish()
    }
}

impl Default for DistanceNode<f64> {
    fn default() -> Self {
        Self {
            low: DistanceFeatures::splat(f64::INFINITY),
            high: DistanceFeatures::splat(f64::NEG_INFINITY),
            bounded: true,
        }
    }
}

impl DistanceNode<f64> {
    pub(crate) fn merge(left: &Self, right: &Self) -> Self {
        node_merge_generic(left, right)
    }

    pub(super) fn rejects(&self, spec: &super::Spec, threshold: f64) -> bool {
        self.rejects_features(&spec.query_fields, spec.count, threshold)
    }
}

impl<F: DistanceRejectOps> DistanceNode<F> {
    pub(crate) fn rejects_features(
        &self,
        query: &DistanceFeatures<F>,
        count: usize,
        threshold: F,
    ) -> bool {
        node_rejects_generic(self, query, count, threshold)
    }
}

pub(crate) fn node_merge_generic<F: DistanceMergeOps>(
    a: &DistanceNode<F>,
    b: &DistanceNode<F>,
) -> DistanceNode<F> {
    DistanceNode {
        low: DistanceFeatures {
            f0: F::min(a.low.f0, b.low.f0),
            f1: F::min(a.low.f1, b.low.f1),
            f2: F::min(a.low.f2, b.low.f2),
            f3: F::min(a.low.f3, b.low.f3),
            f4: F::min(a.low.f4, b.low.f4),
        },
        high: DistanceFeatures {
            f0: F::max(a.high.f0, b.high.f0),
            f1: F::max(a.high.f1, b.high.f1),
            f2: F::max(a.high.f2, b.high.f2),
            f3: F::max(a.high.f3, b.high.f3),
            f4: F::max(a.high.f4, b.high.f4),
        },
        bounded: a.bounded && b.bounded,
    }
}

/// The caller must keep `count <= 5`; the production `Spec` admits 2 through 5.
#[inline(always)]
pub(crate) fn node_rejects_generic<F: DistanceRejectOps>(
    node: &DistanceNode<F>,
    query: &DistanceFeatures<F>,
    count: usize,
    threshold: F,
) -> bool {
    if !node.bounded || !F::is_finite(threshold) || F::le(threshold, F::zero()) {
        return false;
    }
    let mut bound = F::zero();

    if count > 0 {
        if !(F::le(F::zero(), query.f0) && F::le(query.f0, F::one())) {
            return false;
        }
        if F::gt(node.low.f0, node.high.f0) {
            return true;
        }
        let delta = F::max(
            F::abs(F::sub(query.f0, node.low.f0)),
            F::abs(F::sub(query.f0, node.high.f0)),
        );
        F::add_assign(&mut bound, F::log(F::add(F::one(), delta)));
    }

    if count > 1 {
        if !(F::le(F::zero(), query.f1) && F::le(query.f1, F::one())) {
            return false;
        }
        if F::gt(node.low.f1, node.high.f1) {
            return true;
        }
        let delta = F::max(
            F::abs(F::sub(query.f1, node.low.f1)),
            F::abs(F::sub(query.f1, node.high.f1)),
        );
        F::add_assign(&mut bound, F::log(F::add(F::one(), delta)));
    }

    if count > 2 {
        if !(F::le(F::zero(), query.f2) && F::le(query.f2, F::one())) {
            return false;
        }
        if F::gt(node.low.f2, node.high.f2) {
            return true;
        }
        let delta = F::max(
            F::abs(F::sub(query.f2, node.low.f2)),
            F::abs(F::sub(query.f2, node.high.f2)),
        );
        F::add_assign(&mut bound, F::log(F::add(F::one(), delta)));
    }

    if count > 3 {
        if !(F::le(F::zero(), query.f3) && F::le(query.f3, F::one())) {
            return false;
        }
        if F::gt(node.low.f3, node.high.f3) {
            return true;
        }
        let delta = F::max(
            F::abs(F::sub(query.f3, node.low.f3)),
            F::abs(F::sub(query.f3, node.high.f3)),
        );
        F::add_assign(&mut bound, F::log(F::add(F::one(), delta)));
    }

    if count > 4 {
        if !(F::le(F::zero(), query.f4) && F::le(query.f4, F::one())) {
            return false;
        }
        if F::gt(node.low.f4, node.high.f4) {
            return true;
        }
        let delta = F::max(
            F::abs(F::sub(query.f4, node.low.f4)),
            F::abs(F::sub(query.f4, node.high.f4)),
        );
        F::add_assign(&mut bound, F::log(F::add(F::one(), delta)));
    }

    F::is_finite(bound)
        && F::lt(
            F::add(bound, F::mul(F::margin(), F::add(F::one(), F::abs(bound)))),
            threshold,
        )
}

mod distance_scan_f64_ops {
    pub(crate) fn zero() -> f64 {
        0.0
    }
    pub(crate) fn one() -> f64 {
        1.0
    }
    pub(crate) fn margin() -> f64 {
        1e-10
    }
    pub(crate) fn min(left: f64, right: f64) -> f64 {
        left.min(right)
    }
    pub(crate) fn max(left: f64, right: f64) -> f64 {
        left.max(right)
    }
    pub(crate) fn sub(left: f64, right: f64) -> f64 {
        left - right
    }
    pub(crate) fn abs(value: f64) -> f64 {
        value.abs()
    }
    pub(crate) fn add(left: f64, right: f64) -> f64 {
        left + right
    }
    pub(crate) fn add_assign(left: &mut f64, right: f64) {
        *left += right;
    }
    pub(crate) fn mul(left: f64, right: f64) -> f64 {
        left * right
    }
    pub(crate) fn gt(left: f64, right: f64) -> bool {
        left > right
    }
    pub(crate) fn le(left: f64, right: f64) -> bool {
        left <= right
    }
    pub(crate) fn lt(left: f64, right: f64) -> bool {
        left < right
    }
    pub(crate) fn is_finite(value: f64) -> bool {
        value.is_finite()
    }
    pub(crate) fn log(value: f64) -> f64 {
        pinewasm_num::pure_log(value)
    }
}

impl DistanceMergeOps for f64 {
    fn min(left: Self, right: Self) -> Self {
        distance_scan_f64_ops::min(left, right)
    }
    fn max(left: Self, right: Self) -> Self {
        distance_scan_f64_ops::max(left, right)
    }
}

impl DistanceRejectOps for f64 {
    fn zero() -> Self {
        distance_scan_f64_ops::zero()
    }
    fn one() -> Self {
        distance_scan_f64_ops::one()
    }
    fn margin() -> Self {
        distance_scan_f64_ops::margin()
    }
    fn sub(left: Self, right: Self) -> Self {
        distance_scan_f64_ops::sub(left, right)
    }
    fn abs(value: Self) -> Self {
        distance_scan_f64_ops::abs(value)
    }
    fn add(left: Self, right: Self) -> Self {
        distance_scan_f64_ops::add(left, right)
    }
    fn add_assign(left: &mut Self, right: Self) {
        distance_scan_f64_ops::add_assign(left, right);
    }
    fn mul(left: Self, right: Self) -> Self {
        distance_scan_f64_ops::mul(left, right)
    }
    fn gt(left: Self, right: Self) -> bool {
        distance_scan_f64_ops::gt(left, right)
    }
    fn le(left: Self, right: Self) -> bool {
        distance_scan_f64_ops::le(left, right)
    }
    fn lt(left: Self, right: Self) -> bool {
        distance_scan_f64_ops::lt(left, right)
    }
    fn is_finite(value: Self) -> bool {
        distance_scan_f64_ops::is_finite(value)
    }
    fn log(value: Self) -> Self {
        distance_scan_f64_ops::log(value)
    }
}
