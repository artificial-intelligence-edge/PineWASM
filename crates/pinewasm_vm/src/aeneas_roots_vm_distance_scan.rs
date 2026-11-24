//! Aeneas start items for the operation-dictionary indexed-distance kernels.
//!
//! These wrappers expose the generic twins as free functions rather than
//! relying on roots for private inherent methods. The f64 roots make the
//! concrete operation dictionary reachable to extraction.

use crate::distance_scan::{
    self, DistanceFeatures, DistanceMergeOps, DistanceNode, DistanceRejectOps,
};

#[allow(private_bounds, private_interfaces)]
pub fn root_node_merge<F: DistanceMergeOps>(
    left: &DistanceNode<F>,
    right: &DistanceNode<F>,
) -> DistanceNode<F> {
    distance_scan::node_merge_generic(left, right)
}

#[allow(private_bounds, private_interfaces)]
pub fn root_node_rejects<F: DistanceRejectOps>(
    node: &DistanceNode<F>,
    query: &DistanceFeatures<F>,
    count: usize,
    threshold: F,
) -> bool {
    node.rejects_features(query, count, threshold)
}

#[allow(private_interfaces)]
pub fn root_node_merge_f64(
    left: &DistanceNode<f64>,
    right: &DistanceNode<f64>,
) -> DistanceNode<f64> {
    distance_scan::node_merge_generic(left, right)
}

#[allow(private_bounds, private_interfaces)]
pub fn root_node_rejects_f64(
    node: &DistanceNode<f64>,
    query: &DistanceFeatures<f64>,
    count: usize,
    threshold: f64,
) -> bool {
    distance_scan::node_rejects_generic(node, query, count, threshold)
}
