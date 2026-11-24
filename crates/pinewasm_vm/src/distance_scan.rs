//! Shared interpreter/AOT implementation of the compiler's guarded scan.
//! Index nodes cover closed groups of 32 already-available feature rows.
//! Only proven-rejected groups are omitted; surviving leaves retain input
//! order and the original threshold/queue update order.

use crate::{RuntimeError, RuntimeErrorKind, Vm};
use pinewasm_hir::CollectionFunc as C;
use pinewasm_mir::{
    heap::{ElemType, Heap, HeapObj},
    HandleId, Value,
};

const GROUP: usize = 32;
const MAX_LEAVES: usize = 32_768;

fn numeric(value: &Value) -> f64 {
    match crate::value_as_f64(value) {
        Some(value) if value.is_finite() => value,
        _ => f64::NAN,
    }
}
fn numeric_query(value: &Value) -> Option<f64> {
    match value {
        Value::Float(_) | Value::Na => Some(numeric(value)),
        _ => None,
    }
}
fn normalized(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        f64::NAN
    }
}
fn read_threshold(value: &Value) -> Option<f64> {
    match value {
        Value::Int(_) | Value::Na => Some(numeric(value)),
        Value::Float(x) if x.is_finite() => Some(*x),
        _ => None,
    }
}
fn array(heap: &Heap, h: HandleId, ty: ElemType) -> Option<&[Value]> {
    match heap.get(h)? {
        HeapObj::Array { elem_ty, items } if *elem_ty == ty => Some(items),
        _ => None,
    }
}
fn spec_array(heap: &Heap, handle: HandleId, expected: ElemType) -> Option<&[Value]> {
    match heap.get(handle)? {
        HeapObj::Array { elem_ty, items } => match (elem_ty, expected) {
            (ElemType::Float, ElemType::Float) | (ElemType::Int, ElemType::Int) => Some(items),
            _ => None,
        },
        _ => None,
    }
}
fn object<'a>(heap: &'a Heap, value: &Value) -> Option<&'a [Value]> {
    match heap.get(value.as_ref()?)? {
        HeapObj::Object { fields, .. } => Some(fields),
        _ => None,
    }
}

struct SpecArgs<'a> {
    start: i64,
    end: i64,
    count: i64,
    query: &'a Value,
    arrays: &'a Value,
    labels: &'a Value,
    distances: &'a Value,
    predictions: &'a Value,
    neighbors: i64,
    threshold: &'a Value,
}

// Keep `get(0)` instead of `first()` because the extraction stdlib does not
// yet support `Slice::first`; the focused lint exception is extraction-only.
#[allow(clippy::get_first)]
fn read_spec_args(args: &[Value]) -> Option<SpecArgs<'_>> {
    if args.len() != 10 {
        return None;
    }
    let Value::Int(start) = args.get(0)? else {
        return None;
    };
    let Value::Int(end) = args.get(1)? else {
        return None;
    };
    let Value::Int(count) = args.get(2)? else {
        return None;
    };
    let query = args.get(3)?;
    let arrays = args.get(4)?;
    let labels = args.get(5)?;
    let distances = args.get(6)?;
    let predictions = args.get(7)?;
    let Value::Int(neighbors) = args.get(8)? else {
        return None;
    };
    let threshold = args.get(9)?;
    Some(SpecArgs {
        start: *start,
        end: *end,
        count: *count,
        query,
        arrays,
        labels,
        distances,
        predictions,
        neighbors: *neighbors,
        threshold,
    })
}

/// Normalize the internal 12-value guard ABI into the borrowed 10-value spec
/// arguments. Descriptors at positions 10 and 11 either preserve the raw
/// count/neighbors (`-1`) or read an integer from the corresponding object
/// field. This deliberately checks the original first ten values for strings
/// before applying substitutions, matching the VM's guard.
// Keep `get(0)` like `read_spec_args`; the extraction stdlib does not yet
// support `Slice::first` and the clippy exception is extraction-only.
#[allow(clippy::get_first)]
fn read_guard_spec_args<'a>(heap: &Heap, args: &'a [Value]) -> Option<SpecArgs<'a>> {
    if args.len() != 12
        || args[..10]
            .iter()
            .any(|value| matches!(value, Value::String(_)))
    {
        return None;
    }

    let count = read_guard_integer(heap, args.get(2)?, args.get(10)?)?;
    let neighbors = read_guard_integer(heap, args.get(8)?, args.get(11)?)?;
    let Value::Int(start) = args.get(0)? else {
        return None;
    };
    let Value::Int(end) = args.get(1)? else {
        return None;
    };

    Some(SpecArgs {
        start: *start,
        end: *end,
        count,
        query: args.get(3)?,
        arrays: args.get(4)?,
        labels: args.get(5)?,
        distances: args.get(6)?,
        predictions: args.get(7)?,
        neighbors,
        threshold: args.get(9)?,
    })
}

fn read_guard_integer(heap: &Heap, value: &Value, descriptor: &Value) -> Option<i64> {
    let Value::Int(field) = descriptor else {
        return None;
    };
    if *field == -1 {
        let Value::Int(value) = value else {
            return None;
        };
        return Some(*value);
    }

    let field = usize::try_from(*field).ok()?;
    let Some(Value::Int(value)) = object(heap, value)?.get(field) else {
        return None;
    };
    Some(*value)
}

#[derive(Clone, Copy)]
struct SpecBase<'a> {
    start: usize,
    end: usize,
    count: usize,
    query: &'a [Value],
    arrays: &'a [Value],
    labels: HandleId,
    distances: HandleId,
    predictions: HandleId,
    neighbors: usize,
    threshold: f64,
}

fn read_spec_base<'a>(heap: &'a Heap, args: SpecArgs<'_>) -> Option<SpecBase<'a>> {
    if !(2..=5).contains(&args.count)
        || !(1..=100).contains(&args.neighbors)
        || args.start < 0
        || args.end < 0
    {
        return None;
    }
    let threshold = read_threshold(args.threshold)?;
    let start = usize::try_from(args.start).ok()?;
    let end = usize::try_from(args.end).ok()?;
    end.max(start).checked_add(1)?;
    let count = args.count as usize;
    let neighbors = args.neighbors as usize;
    let query = object(heap, args.query)?;
    let arrays = object(heap, args.arrays)?;
    if query.len() < count || arrays.len() < count {
        return None;
    }
    let labels = args.labels.as_ref()?;
    let distances = args.distances.as_ref()?;
    let predictions = args.predictions.as_ref()?;
    let d = spec_array(heap, distances, ElemType::Float)?;
    let p = spec_array(heap, predictions, ElemType::Float)?;
    if d.len() != p.len()
        || d.len() > neighbors
        || distances == predictions
        || labels == distances
        || labels == predictions
    {
        return None;
    }
    if spec_array(heap, labels, ElemType::Int)?.len() <= end.max(start) {
        return None;
    }
    Some(SpecBase {
        start,
        end,
        count,
        query,
        arrays,
        labels,
        distances,
        predictions,
        neighbors,
        threshold,
    })
}

#[derive(Clone)]
struct Spec {
    start: usize,
    end: usize,
    count: usize,
    query: [f64; 5],
    query_fields: DistanceFeatures<f64>,
    arrays: [HandleId; 5],
    labels: HandleId,
    distances: HandleId,
    predictions: HandleId,
    neighbors: usize,
    threshold: f64,
}

impl Spec {
    fn read_feature(
        heap: &Heap,
        query: &Value,
        feature: &Value,
        last_index: usize,
        distances: HandleId,
        predictions: HandleId,
    ) -> Option<(f64, HandleId)> {
        let query = numeric_query(query)?;
        let feature = feature.as_ref()?;
        if feature == distances || feature == predictions {
            return None;
        }
        if spec_array(heap, feature, ElemType::Float)?.len() <= last_index {
            return None;
        }
        Some((query, feature))
    }

    fn read(heap: &Heap, args: &[Value]) -> Option<Self> {
        let args = read_spec_args(args)?;
        Self::read_decoded(heap, args)
    }

    fn read_guard(heap: &Heap, args: &[Value]) -> Option<Self> {
        let args = read_guard_spec_args(heap, args)?;
        Self::read_decoded(heap, args)
    }

    fn read_decoded(heap: &Heap, args: SpecArgs<'_>) -> Option<Self> {
        let base = read_spec_base(heap, args)?;
        Self::read_fields(heap, base)
    }

    fn read_fields(heap: &Heap, base: SpecBase<'_>) -> Option<Self> {
        let (query_fields, array_fields) = Self::read_feature_fields(heap, base)?;
        Some(Self::from_base(base, query_fields, array_fields))
    }

    fn read_feature_fields(
        heap: &Heap,
        base: SpecBase<'_>,
    ) -> Option<(DistanceFeatures<f64>, DistanceFeatures<HandleId>)> {
        let mut query_fields = DistanceFeatures::splat(f64::NAN);
        let mut array_fields = DistanceFeatures::splat(base.labels);
        let last_index = base.end.max(base.start);
        if base.count > 0 {
            let (query, feature) = Self::read_feature(
                heap,
                &base.query[0],
                &base.arrays[0],
                last_index,
                base.distances,
                base.predictions,
            )?;
            query_fields.f0 = query;
            array_fields.f0 = feature;
        }
        if base.count > 1 {
            let (query, feature) = Self::read_feature(
                heap,
                &base.query[1],
                &base.arrays[1],
                last_index,
                base.distances,
                base.predictions,
            )?;
            query_fields.f1 = query;
            array_fields.f1 = feature;
        }
        if base.count > 2 {
            let (query, feature) = Self::read_feature(
                heap,
                &base.query[2],
                &base.arrays[2],
                last_index,
                base.distances,
                base.predictions,
            )?;
            query_fields.f2 = query;
            array_fields.f2 = feature;
        }
        if base.count > 3 {
            let (query, feature) = Self::read_feature(
                heap,
                &base.query[3],
                &base.arrays[3],
                last_index,
                base.distances,
                base.predictions,
            )?;
            query_fields.f3 = query;
            array_fields.f3 = feature;
        }
        if base.count > 4 {
            let (query, feature) = Self::read_feature(
                heap,
                &base.query[4],
                &base.arrays[4],
                last_index,
                base.distances,
                base.predictions,
            )?;
            query_fields.f4 = query;
            array_fields.f4 = feature;
        }
        Some((query_fields, array_fields))
    }

    fn from_base(
        base: SpecBase<'_>,
        query_fields: DistanceFeatures<f64>,
        array_fields: DistanceFeatures<HandleId>,
    ) -> Self {
        let query = [
            query_fields.f0,
            query_fields.f1,
            query_fields.f2,
            query_fields.f3,
            query_fields.f4,
        ];
        let arrays = [
            array_fields.f0,
            array_fields.f1,
            array_fields.f2,
            array_fields.f3,
            array_fields.f4,
        ];
        Self {
            start: base.start,
            end: base.end,
            count: base.count,
            query,
            query_fields,
            arrays,
            labels: base.labels,
            distances: base.distances,
            predictions: base.predictions,
            neighbors: base.neighbors,
            threshold: base.threshold,
        }
    }
}

/// Extraction entry point for the exact Spec-to-rejection call used by the
/// indexed scan. This is compiled only for extraction and does not alter
/// shipped behavior.
#[cfg(feature = "aeneas")]
// The extraction tool starts from this item; Rust has no call site.
#[allow(dead_code, private_interfaces)]
pub fn root_node_rejects_spec_f64(node: &DistanceNode<f64>, spec: &Spec, threshold: f64) -> bool {
    node.rejects(spec, threshold)
}

/// Extraction entry point for the exact VM-side validation/normalization path that admits
/// indexed-distance scan operands. This is compiled only for extraction and
/// has no normal-build call site.
#[cfg(feature = "aeneas")]
#[allow(dead_code, private_interfaces)]
pub fn root_spec_read_current(heap: &Heap, args: &[Value]) -> Option<Spec> {
    Spec::read(heap, args)
}

/// Extraction entry point for the VM's 12-to-10 guard normalization followed by the same
/// decoded Spec construction used by `Spec::read`.
#[cfg(feature = "aeneas")]
#[allow(dead_code, private_interfaces)]
pub fn root_spec_read_guard_current(heap: &Heap, args: &[Value]) -> Option<Spec> {
    Spec::read_guard(heap, args)
}

mod float_kernels;
#[cfg(feature = "aeneas")]
pub(crate) use float_kernels::DistanceMergeOps;
#[cfg(feature = "aeneas")]
pub(crate) use float_kernels::DistanceRejectOps;
#[cfg(any(test, feature = "aeneas"))]
pub(crate) use float_kernels::{node_merge_generic, node_rejects_generic};
pub(crate) use float_kernels::{DistanceFeatures, DistanceNode};
pub(crate) type Node<F = f64> = DistanceNode<F>;

#[derive(Default, Debug)]
pub(crate) struct Cache {
    index: Option<Index>,
}
#[derive(Debug)]
struct Index {
    arrays: [HandleId; 5],
    revisions: [u64; 5],
    count: usize,
    groups: usize,
    capacity: usize,
    nodes: Vec<Node>,
}

impl Vm {
    fn distance_cache_room(&self, bytes: usize) -> bool {
        crate::vm_memory_reservation::project(
            self.bytes_reserved,
            bytes.saturating_sub(self.distance_cache_reserved_bytes),
            self.transient_vm_bytes,
            self.options.max_vm_bytes_per_run,
        )
        .is_ok()
    }

    fn reserve_distance_cache(&mut self, bytes: usize, pc: usize) -> Result<(), RuntimeError> {
        let extra = bytes.saturating_sub(self.distance_cache_reserved_bytes);
        self.reserve_vm_bytes(extra, pc)?;
        self.distance_cache_reserved_bytes = self.distance_cache_reserved_bytes.max(bytes);
        Ok(())
    }

    pub(crate) fn distance_scan_eligible(&self, args: &[Value]) -> bool {
        Spec::read_guard(&self.heap, args).is_some()
    }

    /// Meter a bound/candidate by its actual feature work. Queue operations
    /// additionally pay their actual push/shift work. This contract is shared
    /// by interpreter, profiling, realtime, and hosted/linked AOT execution.
    pub(crate) fn exec_distance_scan(
        &mut self,
        args: &[Value],
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        let spec = Spec::read(&self.heap, args).ok_or_else(|| {
            self.fail(
                RuntimeErrorKind::InvalidProgram,
                "distance scan operands did not pass the guard".into(),
                pc,
            )
        })?;
        if spec.query[..spec.count].iter().any(|x| !x.is_finite()) || !spec.threshold.is_finite() {
            return Ok(Value::from_f64(spec.threshold));
        }
        let mut cache = std::mem::take(&mut self.distance_cache);
        let result = self.scan_with_cache(&spec, &mut cache, pc, budget);
        self.distance_cache = cache;
        result
    }

    fn scan_with_cache(
        &mut self,
        spec: &Spec,
        cache: &mut Cache,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Value, RuntimeError> {
        if spec.start > spec.end {
            let mut threshold = spec.threshold;
            for i in (spec.end..=spec.start).rev() {
                self.scan_candidate(spec, i, &mut threshold, pc, budget)?;
            }
            return Ok(Value::from_f64(threshold));
        }
        let groups = (spec.end + 1) / GROUP;
        let mut indexed = groups <= MAX_LEAVES
            && spec.query[..spec.count]
                .iter()
                .all(|x| (0.0..=1.0).contains(x));
        if indexed {
            if self.distance_cache_room(4096) {
                self.reserve_distance_cache(4096, pc)?;
            } else {
                indexed = false;
            }
        }
        let mut revisions = [0; 5];
        if indexed {
            for (j, revision) in revisions.iter_mut().enumerate().take(spec.count) {
                match self.heap.watch_array_prefix(spec.arrays[j]) {
                    Some(r) => *revision = r,
                    None => indexed = false,
                }
            }
        }
        if indexed {
            if cache.index.as_ref().is_none_or(|i| {
                i.arrays != spec.arrays
                    || i.revisions != revisions
                    || i.count != spec.count
                    || i.groups > groups
            }) {
                cache.index = Some(Index {
                    arrays: spec.arrays,
                    revisions,
                    count: spec.count,
                    groups: 0,
                    capacity: 0,
                    nodes: Vec::new(),
                });
            }
            if let Some(index) = cache.index.as_mut() {
                let capacity = groups.max(1).next_power_of_two();
                if capacity > index.capacity
                    && !self.distance_cache_room(
                        4096 + (2 * capacity + index.nodes.len()) * std::mem::size_of::<Node>(),
                    )
                {
                    // Optional cache growth cannot be admitted at this limit.
                    // Use the ordered, uncached kernel instead.
                    indexed = false;
                } else {
                    self.extend_index(index, spec, groups, pc, budget)?;
                }
            }
        }
        let mut threshold = spec.threshold;
        if indexed {
            if let Some(index) = cache.index.as_ref() {
                if index.groups > 0 {
                    self.visit_index(
                        index,
                        1,
                        0,
                        index.capacity,
                        spec,
                        &mut threshold,
                        pc,
                        budget,
                    )?;
                }
            }
        }
        let first = if indexed {
            spec.start.max(groups * GROUP)
        } else {
            spec.start
        };
        for i in first..=spec.end {
            self.scan_candidate(spec, i, &mut threshold, pc, budget)?;
        }
        Ok(Value::from_f64(threshold))
    }

    fn extend_index(
        &mut self,
        index: &mut Index,
        spec: &Spec,
        groups: usize,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let capacity = groups.max(1).next_power_of_two();
        if capacity > index.capacity {
            let cells = 2 * capacity;
            // The old and new vectors coexist during growth. Reserve their
            // peak once; cache resets/rebuilds reuse that high-water allowance.
            self.reserve_distance_cache(
                4096 + (cells + index.nodes.len()) * std::mem::size_of::<Node>(),
                pc,
            )?;
            self.charge_extra_work(budget, cells as u64, pc)?;
            let mut nodes = vec![Node::default(); cells];
            if index.capacity > 0 {
                for j in 0..index.groups {
                    nodes[capacity + j] = index.nodes[index.capacity + j];
                }
                for j in (1..capacity).rev() {
                    nodes[j] = Node::merge(&nodes[2 * j], &nodes[2 * j + 1]);
                }
            }
            index.nodes = nodes;
            index.capacity = capacity;
        }
        for group in index.groups..groups {
            self.charge_extra_work(
                budget,
                (GROUP * spec.count + index.capacity.ilog2() as usize) as u64,
                pc,
            )?;
            let mut node = Node::default();
            for j in 0..spec.count {
                if let Some(items) = array(&self.heap, spec.arrays[j], ElemType::Float) {
                    for value in &items[group * GROUP..(group + 1) * GROUP] {
                        let x = numeric(value);
                        if x.is_finite() {
                            node.bounded &= (0.0..=1.0).contains(&x);
                            node.low.set(j, node.low.get(j).min(x));
                            node.high.set(j, node.high.get(j).max(x));
                        }
                    }
                }
            }
            let mut position = index.capacity + group;
            index.nodes[position] = node;
            while position > 1 {
                position /= 2;
                index.nodes[position] =
                    Node::merge(&index.nodes[2 * position], &index.nodes[2 * position + 1]);
            }
        }
        index.groups = groups;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_index(
        &mut self,
        index: &Index,
        node: usize,
        lo: usize,
        hi: usize,
        spec: &Spec,
        threshold: &mut f64,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        if lo >= index.groups || hi * GROUP <= spec.start || lo * GROUP > spec.end {
            return Ok(());
        }
        self.charge_extra_work(budget, (spec.count * 9 + 1) as u64, pc)?;
        if lo * GROUP >= spec.start
            && hi <= index.groups
            && hi * GROUP <= spec.end + 1
            && index.nodes[node].rejects(spec, *threshold)
        {
            return Ok(());
        }
        if hi - lo == 1 {
            for i in (lo * GROUP).max(spec.start)..(hi * GROUP).min(spec.end + 1) {
                self.scan_candidate(spec, i, threshold, pc, budget)?;
            }
        } else {
            let mid = lo.midpoint(hi);
            self.visit_index(index, node * 2, lo, mid, spec, threshold, pc, budget)?;
            self.visit_index(index, node * 2 + 1, mid, hi, spec, threshold, pc, budget)?;
        }
        Ok(())
    }

    fn scan_candidate(
        &mut self,
        spec: &Spec,
        i: usize,
        threshold: &mut f64,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        self.charge_extra_work(budget, 1, pc)?;
        if i % 4 == 0 {
            return Ok(());
        }
        self.charge_extra_work(budget, (9 * spec.count) as u64, pc)?;
        let mut distance = 0.0;
        for j in 0..spec.count {
            let x = array(&self.heap, spec.arrays[j], ElemType::Float)
                .and_then(|a| a.get(i))
                .map_or(f64::NAN, numeric);
            let term =
                pinewasm_num::pure_log(normalized(1.0 + normalized(spec.query[j] - x).abs()));
            distance = if j == 0 {
                term
            } else {
                normalized(distance + term)
            };
        }
        if distance >= *threshold {
            self.charge_extra_work(budget, 8, pc)?;
            *threshold = distance;
            let label = array(&self.heap, spec.labels, ElemType::Int)
                .and_then(|a| a.get(i))
                .cloned()
                .unwrap_or(Value::Na);
            self.call_array_op(
                C::ArrayPush,
                &[Value::Ref(spec.distances), Value::from_f64(distance)],
                pc,
            )?;
            self.call_array_op(
                C::ArrayPush,
                &[
                    Value::Ref(spec.predictions),
                    Value::from_f64(numeric(&label)),
                ],
                pc,
            )?;
            let len =
                array(&self.heap, spec.predictions, ElemType::Float).map_or(0, <[Value]>::len);
            if len > spec.neighbors {
                self.charge_extra_work(budget, (2 * len + 4) as u64, pc)?;
                // Compute the index as round(3*k/4), rounding positive ties
                // upward. Read the threshold before either shift.
                *threshold = array(&self.heap, spec.distances, ElemType::Float)
                    .and_then(|a| a.get(((spec.neighbors * 3) as f64 / 4.0).round() as usize))
                    .map_or(f64::NAN, numeric);
                self.call_array_op(C::ArrayShift, &[Value::Ref(spec.distances)], pc)?;
                self.call_array_op(C::ArrayShift, &[Value::Ref(spec.predictions)], pc)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod distance_scan_float_ops_tests {
    use super::{
        node_merge_generic, node_rejects_generic, DistanceFeatures, DistanceNode, Node, Spec,
    };
    use pinewasm_mir::HandleId;

    fn assert_same_float(left: f64, right: f64) {
        assert_eq!(left.to_bits(), right.to_bits());
    }

    fn assert_same_node(left: &Node, right: &Node) {
        assert_eq!(left.bounded, right.bounded);
        for j in 0..5 {
            assert_same_float(left.low.get(j), right.low.get(j));
            assert_same_float(left.high.get(j), right.high.get(j));
        }
    }

    fn assert_same_summary(node: &Node, summary: &DistanceNode<f64>) {
        assert_eq!(node.bounded, summary.bounded);
        for (left, right) in [
            (node.low.f0, summary.low.f0),
            (node.low.f1, summary.low.f1),
            (node.low.f2, summary.low.f2),
            (node.low.f3, summary.low.f3),
            (node.low.f4, summary.low.f4),
            (node.high.f0, summary.high.f0),
            (node.high.f1, summary.high.f1),
            (node.high.f2, summary.high.f2),
            (node.high.f3, summary.high.f3),
            (node.high.f4, summary.high.f4),
        ] {
            assert_same_float(left, right);
        }
    }

    fn distance_features(values: &[f64; 5]) -> DistanceFeatures<f64> {
        DistanceFeatures {
            f0: values[0],
            f1: values[1],
            f2: values[2],
            f3: values[3],
            f4: values[4],
        }
    }

    fn distance_node(node: &Node) -> DistanceNode<f64> {
        *node
    }

    fn node(low: [f64; 5], high: [f64; 5], bounded: bool) -> Node {
        Node {
            low: distance_features(&low),
            high: distance_features(&high),
            bounded,
        }
    }

    // Plain model of the control flow and primitive operations, with the low/high field names.
    fn reference_merge(left: &Node, right: &Node) -> Node {
        let mut result = Node {
            bounded: left.bounded && right.bounded,
            ..Node::default()
        };
        for j in 0..5 {
            result.low.set(j, left.low.get(j).min(right.low.get(j)));
            result.high.set(j, left.high.get(j).max(right.high.get(j)));
        }
        result
    }

    fn reference_rejects_arrays(
        low: &[f64; 5],
        high: &[f64; 5],
        bounded: bool,
        query: &[f64; 5],
        count: usize,
        threshold: f64,
    ) -> bool {
        if !bounded || !threshold.is_finite() || threshold <= 0.0 {
            return false;
        }
        let mut bound = 0.0;
        let mut j = 0;
        while j < count {
            let q = query[j];
            if !(0.0..=1.0).contains(&q) {
                return false;
            }
            if low[j] > high[j] {
                return true;
            }
            let delta = (q - low[j]).abs().max((q - high[j]).abs());
            bound += pinewasm_num::pure_log(1.0 + delta);
            j += 1;
        }
        bound.is_finite() && bound + 1e-10 * (1.0 + bound.abs()) < threshold
    }

    fn reference_rejects(node: &Node, query: &[f64; 5], count: usize, threshold: f64) -> bool {
        let low = [
            node.low.f0,
            node.low.f1,
            node.low.f2,
            node.low.f3,
            node.low.f4,
        ];
        let high = [
            node.high.f0,
            node.high.f1,
            node.high.f2,
            node.high.f3,
            node.high.f4,
        ];
        reference_rejects_arrays(&low, &high, node.bounded, query, count, threshold)
    }

    fn time_reference_kernel(
        low: &[f64; 5],
        high: &[f64; 5],
        specs: &[Spec; 4],
        thresholds: &[f64; 5],
        iterations: usize,
    ) -> std::time::Duration {
        let start = std::time::Instant::now();
        for i in 0..iterations {
            let spec = &specs[i % specs.len()];
            let result = reference_rejects_arrays(
                low,
                high,
                true,
                &spec.query,
                spec.count,
                thresholds[i % thresholds.len()],
            );
            std::hint::black_box(result);
        }
        start.elapsed()
    }

    fn time_shared_kernel(
        node: &Node,
        specs: &[Spec; 4],
        thresholds: &[f64; 5],
        iterations: usize,
    ) -> std::time::Duration {
        let start = std::time::Instant::now();
        for i in 0..iterations {
            let spec = &specs[i % specs.len()];
            let result = node.rejects(spec, thresholds[i % thresholds.len()]);
            std::hint::black_box(result);
        }
        start.elapsed()
    }

    fn spec(query: [f64; 5], count: usize) -> Spec {
        Spec {
            start: 0,
            end: 0,
            count,
            query,
            query_fields: distance_features(&query),
            arrays: [HandleId(0); 5],
            labels: HandleId(0),
            distances: HandleId(0),
            predictions: HandleId(0),
            neighbors: 1,
            threshold: 0.0,
        }
    }

    #[test]
    fn spec_rejects_observes_query_fields_and_count() {
        let flat_node = node([0.0; 5], [0.0; 5], true);
        let at_zero = spec([0.0; 5], 2);
        assert!(flat_node.rejects(&at_zero, 0.5));

        let mut fields_at_one = spec([0.0; 5], 2);
        fields_at_one.query_fields.f0 = 1.0;
        assert!(!flat_node.rejects(&fields_at_one, 0.5));

        let mut raw_query_at_one = spec([0.0; 5], 2);
        raw_query_at_one.query[0] = 1.0;
        assert!(flat_node.rejects(&raw_query_at_one, 0.5));

        let invalid_third = node([0.0, 0.0, 1.0, 0.0, 0.0], [0.0; 5], true);
        let two_features = spec([0.0; 5], 2);
        let three_features = spec([0.0; 5], 3);
        assert!(!invalid_third.rejects(&two_features, 1e-10));
        assert!(invalid_third.rejects(&three_features, 1e-10));
    }

    #[test]
    fn generic_merge_matches_primitive_bits_for_float_boundaries() {
        let values = [
            f64::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            1.0,
            f64::INFINITY,
            f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0042),
        ];

        for (i, left_low) in values.iter().copied().enumerate() {
            for (j, right_low) in values.iter().copied().enumerate() {
                let left = node(
                    [
                        left_low,
                        values[(i + 1) % values.len()],
                        values[(i + 2) % values.len()],
                        values[(i + 3) % values.len()],
                        values[(i + 4) % values.len()],
                    ],
                    [
                        values[(i + 5) % values.len()],
                        values[(i + 6) % values.len()],
                        values[(i + 7) % values.len()],
                        values[(i + 8) % values.len()],
                        values[(i + 9) % values.len()],
                    ],
                    i % 2 == 0,
                );
                let right = node(
                    [
                        right_low,
                        values[(j + 1) % values.len()],
                        values[(j + 2) % values.len()],
                        values[(j + 3) % values.len()],
                        values[(j + 4) % values.len()],
                    ],
                    [
                        values[(j + 5) % values.len()],
                        values[(j + 6) % values.len()],
                        values[(j + 7) % values.len()],
                        values[(j + 8) % values.len()],
                        values[(j + 9) % values.len()],
                    ],
                    j % 3 != 0,
                );
                let primitive = Node::merge(&left, &right);
                let reference = reference_merge(&left, &right);
                let left_summary = distance_node(&left);
                let right_summary = distance_node(&right);
                let generic = node_merge_generic(&left_summary, &right_summary);
                assert_same_node(&reference, &primitive);
                assert_same_summary(&primitive, &generic);
            }
        }
    }

    #[test]
    fn generic_rejects_preserves_distinct_feature_positions() {
        let nodes = [
            node([0.0, 0.1, 0.2, 0.3, 0.4], [0.6, 0.7, 0.8, 0.9, 1.0], true),
            node([0.0, 0.1, 0.2, 1.0, 0.4], [0.6, 0.7, 0.8, 0.9, 1.0], true),
        ];
        let query = [0.05, 0.15, 0.25, 0.95, 0.75];

        for node in &nodes {
            for count in 2..=5 {
                for threshold in [0.0, 0.1, 1.0, 5.0] {
                    let primitive = node.rejects(&spec(query, count), threshold);
                    let reference = reference_rejects(node, &query, count, threshold);
                    let summary = distance_node(node);
                    let features = distance_features(&query);
                    let generic = node_rejects_generic(&summary, &features, count, threshold);
                    assert_eq!(reference, primitive);
                    assert_eq!(primitive, generic);
                }
            }
        }
    }

    #[test]
    fn generic_rejects_matches_primitive_for_nan_signed_zero_and_empty_features() {
        let nodes = [
            node([0.0; 5], [1.0; 5], true),
            node([1.0; 5], [0.0; 5], true),
            node([-0.0; 5], [0.0; 5], true),
            node([f64::NAN; 5], [f64::NAN; 5], true),
            node([0.0; 5], [1.0; 5], false),
        ];
        let queries = [
            [0.0; 5],
            [-0.0; 5],
            [0.5; 5],
            [1.0; 5],
            [f64::from_bits(1.0_f64.to_bits() + 1); 5],
            [f64::from_bits((-f64::MIN_POSITIVE).to_bits()); 5],
            [f64::NAN; 5],
            [f64::INFINITY; 5],
        ];
        let thresholds = [
            f64::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
            f64::from_bits(1),
            1e-10,
            1.0,
            f64::INFINITY,
            f64::NAN,
        ];

        for node in &nodes {
            for query in queries {
                for count in [0, 1, 2, 3, 4, 5] {
                    let input = spec(query, count);
                    for threshold in thresholds {
                        let primitive = node.rejects(&input, threshold);
                        let reference = reference_rejects(node, &query, count, threshold);
                        let summary = distance_node(node);
                        let features = distance_features(&query);
                        let generic = node_rejects_generic(&summary, &features, count, threshold);
                        assert_eq!(reference, primitive);
                        assert_eq!(primitive, generic);
                    }
                }
            }
        }
    }

    #[test]
    fn generic_rejects_preserves_strict_threshold_boundary() {
        let node = node([0.0; 5], [1.0; 5], true);
        let query = [0.5; 5];
        let bound = 2.0 * pinewasm_num::pure_log(1.5);
        let cutoff = bound + 1e-10 * (1.0 + bound.abs());
        let below = f64::from_bits(cutoff.to_bits() - 1);
        let above = f64::from_bits(cutoff.to_bits() + 1);

        for threshold in [below, cutoff, above] {
            let primitive = node.rejects(&spec(query, 2), threshold);
            let reference = reference_rejects(&node, &query, 2, threshold);
            let summary = distance_node(&node);
            let features = distance_features(&query);
            let generic = node_rejects_generic(&summary, &features, 2, threshold);
            assert_eq!(reference, primitive);
            assert_eq!(primitive, generic);
        }
        assert!(!node.rejects(&spec(query, 2), cutoff));
        assert!(!node.rejects(&spec(query, 2), below));
        assert!(node.rejects(&spec(query, 2), above));
    }

    #[test]
    #[ignore = "manual matched kernel timing; run in release mode with --ignored --nocapture"]
    fn manual_matched_direct_vs_shared_reject_kernel_timing() {
        const ITERATIONS: usize = 2_000_000;
        let low = [0.03, 0.11, 0.19, 0.27, 0.35];
        let high = [0.61, 0.69, 0.77, 0.85, 0.93];
        let query = [0.15, 0.26, 0.38, 0.56, 0.74];
        let node = node(low, high, true);
        let specs = [
            spec(query, 2),
            spec(query, 3),
            spec(query, 4),
            spec(query, 5),
        ];
        let thresholds = [0.25, 0.5, 1.0, 2.0, 8.0];
        let mut direct = Vec::new();
        let mut shared = Vec::new();

        for round in 0..9 {
            if round % 2 == 0 {
                direct.push(time_reference_kernel(
                    &low,
                    &high,
                    &specs,
                    &thresholds,
                    ITERATIONS,
                ));
                shared.push(time_shared_kernel(&node, &specs, &thresholds, ITERATIONS));
            } else {
                shared.push(time_shared_kernel(&node, &specs, &thresholds, ITERATIONS));
                direct.push(time_reference_kernel(
                    &low,
                    &high,
                    &specs,
                    &thresholds,
                    ITERATIONS,
                ));
            }
        }
        direct.sort_unstable();
        shared.sort_unstable();
        let direct_median = direct[direct.len() / 2];
        let shared_median = shared[shared.len() / 2];
        let ns_per_call = |duration: std::time::Duration| {
            duration.as_secs_f64() * 1_000_000_000.0 / ITERATIONS as f64
        };
        println!(
            "direct_median_ns={:.2} shared_median_ns={:.2} shared_over_direct={:.4}",
            ns_per_call(direct_median),
            ns_per_call(shared_median),
            shared_median.as_secs_f64() / direct_median.as_secs_f64()
        );
    }

    #[test]
    fn node_debug_keeps_the_existing_field_labels() {
        let node = node([0.0; 5], [1.0; 5], true);
        assert_eq!(
            format!("{node:?}"),
            "Node { min: [0.0, 0.0, 0.0, 0.0, 0.0], max: [1.0, 1.0, 1.0, 1.0, 1.0], bounded: true }"
        );
    }
}

#[cfg(test)]
mod spec_read_tests {
    use super::{object, Heap, HeapObj, Spec};
    use pinewasm_mir::{heap::ElemType, HandleId, Value};

    fn alloc_array(heap: &mut Heap, elem_ty: ElemType, items: Vec<Value>) -> HandleId {
        heap.alloc(HeapObj::Array { elem_ty, items }).unwrap()
    }

    fn alloc_object(heap: &mut Heap, fields: Vec<Value>) -> HandleId {
        heap.alloc(HeapObj::Object {
            type_id: 0,
            fields,
            table_identity: None,
        })
        .unwrap()
    }

    fn with_guard_descriptors(
        mut args: Vec<Value>,
        count_descriptor: Value,
        neighbors_descriptor: Value,
    ) -> Vec<Value> {
        args.push(count_descriptor);
        args.push(neighbors_descriptor);
        args
    }

    fn assert_same_spec(left: &Spec, right: &Spec) {
        assert_eq!(
            (left.start, left.end, left.count, left.neighbors),
            (right.start, right.end, right.count, right.neighbors)
        );
        assert_eq!(left.query.map(f64::to_bits), right.query.map(f64::to_bits));
        assert_eq!(
            (
                left.query_fields.f0.to_bits(),
                left.query_fields.f1.to_bits(),
                left.query_fields.f2.to_bits(),
                left.query_fields.f3.to_bits(),
                left.query_fields.f4.to_bits(),
            ),
            (
                right.query_fields.f0.to_bits(),
                right.query_fields.f1.to_bits(),
                right.query_fields.f2.to_bits(),
                right.query_fields.f3.to_bits(),
                right.query_fields.f4.to_bits(),
            )
        );
        assert_eq!(left.arrays, right.arrays);
        assert_eq!(left.labels, right.labels);
        assert_eq!(left.distances, right.distances);
        assert_eq!(left.predictions, right.predictions);
        assert_eq!(left.threshold.to_bits(), right.threshold.to_bits());
    }

    // Clone-based model of Vm::distance_scan_eligible. Comparing the
    // borrowed guard with it protects the guard contract while keeping the
    // production path extraction-friendly.
    fn reference_guard_accepts(heap: &Heap, args: &[Value]) -> bool {
        if args.len() != 12
            || args[..10]
                .iter()
                .any(|value| matches!(value, Value::String(_)))
        {
            return false;
        }
        let mut captured: [Value; 10] = std::array::from_fn(|j| args[j].clone());
        for (position, descriptor) in [(2, &args[10]), (8, &args[11])] {
            let Value::Int(field) = descriptor else {
                return false;
            };
            if *field == -1 {
                continue;
            }
            let Ok(field) = usize::try_from(*field) else {
                return false;
            };
            let Some(Value::Int(value)) =
                object(heap, &args[position]).and_then(|fields| fields.get(field))
            else {
                return false;
            };
            captured[position] = Value::Int(*value);
        }
        Spec::read(heap, &captured).is_some()
    }

    fn assert_matches_reference_guard(heap: &Heap, args: &[Value]) {
        assert_eq!(
            Spec::read_guard(heap, args).is_some(),
            reference_guard_accepts(heap, args),
            "borrowed guard behavior differs from the clone-based model"
        );
    }

    fn valid_inputs() -> (Heap, Vec<Value>, [HandleId; 2]) {
        let mut heap = Heap::new();
        let feature0 = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(0.25), Value::Float(0.5)],
        );
        let feature1 = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(0.75), Value::Float(1.0)],
        );
        let query = alloc_object(&mut heap, vec![Value::Float(0.25), Value::Na]);
        let arrays = alloc_object(&mut heap, vec![Value::Ref(feature0), Value::Ref(feature1)]);
        let labels = alloc_array(&mut heap, ElemType::Int, vec![Value::Int(0), Value::Int(1)]);
        let distances = alloc_array(&mut heap, ElemType::Float, vec![Value::Float(0.5)]);
        let predictions = alloc_array(&mut heap, ElemType::Float, vec![Value::Float(1.0)]);
        let args = vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(2),
            Value::Ref(query),
            Value::Ref(arrays),
            Value::Ref(labels),
            Value::Ref(distances),
            Value::Ref(predictions),
            Value::Int(2),
            Value::Float(0.5),
        ];
        (heap, args, [feature0, feature1])
    }

    fn valid_inputs_for_count(count: usize) -> (Heap, Vec<Value>, Vec<HandleId>) {
        let (mut heap, mut args, base_features) = valid_inputs();
        let mut features = vec![base_features[0], base_features[1]];
        while features.len() < count {
            let index = features.len();
            features.push(alloc_array(
                &mut heap,
                ElemType::Float,
                vec![
                    Value::Float(index as f64 + 0.25),
                    Value::Float(index as f64 + 0.5),
                ],
            ));
        }

        let query = alloc_object(
            &mut heap,
            (0..count)
                .map(|index| Value::Float(index as f64 + 0.25))
                .collect(),
        );
        let arrays = alloc_object(
            &mut heap,
            features.iter().copied().map(Value::Ref).collect(),
        );
        args[2] = Value::Int(count as i64);
        args[3] = Value::Ref(query);
        args[4] = Value::Ref(arrays);
        (heap, args, features)
    }

    #[test]
    fn spec_read_preserves_admitted_query_and_feature_fields() {
        let (heap, args, features) = valid_inputs();
        let spec = Spec::read(&heap, &args).expect("well-shaped scan operands should be admitted");

        assert_eq!(
            (spec.start, spec.end, spec.count, spec.neighbors),
            (0, 1, 2, 2)
        );
        assert_eq!(spec.arrays[..2], features);
        assert_eq!(spec.query[0], 0.25);
        assert_eq!(spec.query_fields.f0, 0.25);
        assert!(spec.query[1].is_nan());
        assert!(spec.query_fields.f1.is_nan());
        assert!(spec.query_fields.f2.is_nan());
        assert_eq!(spec.threshold, 0.5);
    }

    #[test]
    fn spec_read_accepts_every_supported_count_and_reads_the_last_active_slot() {
        for count in 2..=5 {
            let (heap, args, features) = valid_inputs_for_count(count);
            let spec = Spec::read(&heap, &args)
                .unwrap_or_else(|| panic!("well-shaped count={count} inputs should be admitted"));
            let last = count - 1;
            let expected = last as f64 + 0.25;

            assert_eq!(spec.count, count);
            assert_eq!(spec.query[last], expected);
            assert_eq!(spec.arrays[last], features[last]);
            let final_query_field = match last {
                0 => spec.query_fields.f0,
                1 => spec.query_fields.f1,
                2 => spec.query_fields.f2,
                3 => spec.query_fields.f3,
                4 => spec.query_fields.f4,
                _ => unreachable!("count is bounded by five"),
            };
            assert_eq!(final_query_field, expected);
        }
    }

    #[test]
    fn spec_read_accepts_na_but_refuses_nonfinite_float_thresholds() {
        let (heap, mut args, _) = valid_inputs();
        args[9] = Value::Na;
        assert!(Spec::read(&heap, &args).unwrap().threshold.is_nan());

        args[9] = Value::Float(f64::INFINITY);
        assert!(Spec::read(&heap, &args).is_none());
        args[9] = Value::Float(f64::NAN);
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_integer_threshold_uses_the_vm_binary64_conversion() {
        let (heap, mut args, _) = valid_inputs();
        args[9] = Value::Int(i64::MAX);
        let spec = Spec::read(&heap, &args).expect("integer thresholds are admitted");
        let expected = crate::vm_ops::vm_f64_ops::from_i64(i64::MAX);
        assert_eq!(spec.threshold.to_bits(), expected.to_bits());
    }

    #[test]
    fn spec_read_refuses_invalid_counts_neighbors_and_negative_ranges() {
        let (heap, mut args, _) = valid_inputs();
        for count in [1, 6] {
            args[2] = Value::Int(count);
            assert!(Spec::read(&heap, &args).is_none());
        }
        args[2] = Value::Int(2);
        for neighbors in [0, 101] {
            args[8] = Value::Int(neighbors);
            assert!(Spec::read(&heap, &args).is_none());
        }
        args[8] = Value::Int(2);
        args[0] = Value::Int(-1);
        assert!(Spec::read(&heap, &args).is_none());
        args[0] = Value::Int(0);
        args[1] = Value::Int(-1);
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_refuses_wrong_query_types_and_result_array_aliases() {
        let (mut heap, mut args, _) = valid_inputs();
        let query = alloc_object(&mut heap, vec![Value::Float(0.25), Value::Int(1)]);
        args[3] = Value::Ref(query);
        assert!(Spec::read(&heap, &args).is_none());

        let (heap, mut args, _) = valid_inputs();
        args[7] = args[6].clone();
        assert!(Spec::read(&heap, &args).is_none());

        for output_index in [6, 7] {
            let (mut heap, mut args, features) = valid_inputs();
            let fields = vec![args[output_index].clone(), Value::Ref(features[1])];
            let arrays = alloc_object(&mut heap, fields);
            args[4] = Value::Ref(arrays);
            assert!(Spec::read(&heap, &args).is_none());
        }

        for output_index in [6, 7] {
            let (heap, mut args, _) = valid_inputs();
            args[5] = args[output_index].clone();
            assert!(Spec::read(&heap, &args).is_none());
        }
    }

    #[test]
    fn spec_read_refuses_malformed_outer_shape_and_endpoints_without_array_coverage() {
        let (heap, args, _) = valid_inputs();
        assert!(Spec::read(&heap, &args[..9]).is_none());

        let mut extra = args.clone();
        extra.push(Value::Na);
        assert!(Spec::read(&heap, &extra).is_none());

        let (heap, mut args, _) = valid_inputs();
        args[0] = Value::Float(0.0);
        assert!(Spec::read(&heap, &args).is_none());
        args[0] = Value::Int(i64::MAX);
        assert!(Spec::read(&heap, &args).is_none());
        args[0] = Value::Int(0);
        args[1] = Value::Int(i64::MAX);
        assert!(Spec::read(&heap, &args).is_none());

        let (heap, mut args, _) = valid_inputs();
        args[9] = Value::String("not a threshold".to_owned());
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_preserves_reversed_ranges_shared_features_and_ignored_tails() {
        let mut heap = Heap::new();
        let feature = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(0.25), Value::Float(0.5)],
        );
        let query = alloc_object(
            &mut heap,
            vec![
                Value::Float(f64::INFINITY),
                Value::Na,
                Value::String("ignored".to_owned()),
            ],
        );
        let arrays = alloc_object(
            &mut heap,
            vec![
                Value::Ref(feature),
                Value::Ref(feature),
                Value::String("ignored".to_owned()),
            ],
        );
        let labels = alloc_array(&mut heap, ElemType::Int, vec![Value::Int(0), Value::Int(1)]);
        let distances = alloc_array(&mut heap, ElemType::Float, vec![]);
        let predictions = alloc_array(&mut heap, ElemType::Float, vec![]);
        let args = vec![
            Value::Int(1),
            Value::Int(0),
            Value::Int(2),
            Value::Ref(query),
            Value::Ref(arrays),
            Value::Ref(labels),
            Value::Ref(distances),
            Value::Ref(predictions),
            Value::Int(100),
            Value::Int(7),
        ];

        let spec = Spec::read(&heap, &args).expect(
            "the scan accepts reversed endpoints, shared feature arrays, and empty outputs",
        );
        assert_eq!(
            (spec.start, spec.end, spec.count, spec.neighbors),
            (1, 0, 2, 100)
        );
        assert_eq!(spec.arrays[0], feature);
        assert_eq!(spec.arrays[1], feature);
        assert_eq!(spec.arrays[2..], [labels; 3]);
        assert!(spec.query[0].is_nan());
        assert!(spec.query_fields.f0.is_nan());
        assert!(spec.threshold.is_finite());
        assert_eq!(spec.threshold, 7.0);
    }

    #[test]
    fn spec_read_refuses_wrong_object_and_array_kinds_or_mismatched_outputs() {
        let (heap, mut args, features) = valid_inputs();
        args[3] = Value::Ref(features[0]);
        assert!(Spec::read(&heap, &args).is_none());

        let (heap, mut args, _) = valid_inputs();
        args[3] = args[4].clone();
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, _) = valid_inputs();
        let arrays = alloc_array(&mut heap, ElemType::Float, vec![Value::Float(0.25)]);
        args[4] = Value::Ref(arrays);
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, _) = valid_inputs();
        let labels = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(0.0), Value::Float(1.0)],
        );
        args[5] = Value::Ref(labels);
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, _) = valid_inputs();
        let predictions = alloc_array(&mut heap, ElemType::Int, vec![Value::Int(1)]);
        args[7] = Value::Ref(predictions);
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, _) = valid_inputs();
        let predictions = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(1.0), Value::Float(2.0)],
        );
        args[7] = Value::Ref(predictions);
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_refuses_output_length_above_neighbors() {
        let (mut heap, mut args, _) = valid_inputs();
        let distances = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(0.5), Value::Float(0.75)],
        );
        let predictions = alloc_array(
            &mut heap,
            ElemType::Float,
            vec![Value::Float(1.0), Value::Float(1.25)],
        );
        args[6] = Value::Ref(distances);
        args[7] = Value::Ref(predictions);
        args[8] = Value::Int(1);

        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_refuses_query_and_feature_objects_shorter_than_count() {
        let (mut heap, mut args, _) = valid_inputs();
        let short_query = alloc_object(&mut heap, vec![Value::Float(0.25)]);
        args[3] = Value::Ref(short_query);
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, features) = valid_inputs();
        let short_arrays = alloc_object(&mut heap, vec![Value::Ref(features[0])]);
        args[4] = Value::Ref(short_arrays);
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_read_refuses_short_referenced_arrays() {
        let (mut heap, mut args, _) = valid_inputs();
        let short_label = alloc_array(&mut heap, ElemType::Int, vec![Value::Int(0)]);
        args[5] = Value::Ref(short_label);
        assert!(Spec::read(&heap, &args).is_none());

        let (mut heap, mut args, _) = valid_inputs();
        let short_feature = alloc_array(&mut heap, ElemType::Float, vec![Value::Float(0.25)]);
        let arrays = alloc_object(
            &mut heap,
            vec![Value::Ref(short_feature), Value::Ref(short_feature)],
        );
        args[4] = Value::Ref(arrays);
        assert!(Spec::read(&heap, &args).is_none());
    }

    #[test]
    fn spec_guard_accepts_both_sentinels_and_each_or_both_field_substitutions() {
        let (mut heap, args, _) = valid_inputs();
        let direct = Spec::read(&heap, &args).unwrap();

        let sentinel_args = with_guard_descriptors(args.clone(), Value::Int(-1), Value::Int(-1));
        assert_matches_reference_guard(&heap, &sentinel_args);
        let sentinel = Spec::read_guard(&heap, &sentinel_args).unwrap();
        assert_same_spec(&sentinel, &direct);

        let count_object = alloc_object(&mut heap, vec![Value::Bool(true), Value::Int(2)]);
        let neighbors_object = alloc_object(&mut heap, vec![Value::Float(1.5), Value::Int(2)]);

        let mut count_args = args.clone();
        count_args[2] = Value::Ref(count_object);
        let count_args = with_guard_descriptors(count_args, Value::Int(1), Value::Int(-1));
        assert_matches_reference_guard(&heap, &count_args);
        let count_only = Spec::read_guard(&heap, &count_args).unwrap();
        assert_same_spec(&count_only, &direct);

        let mut neighbors_args = args.clone();
        neighbors_args[8] = Value::Ref(neighbors_object);
        let neighbors_args = with_guard_descriptors(neighbors_args, Value::Int(-1), Value::Int(1));
        assert_matches_reference_guard(&heap, &neighbors_args);
        let neighbors_only = Spec::read_guard(&heap, &neighbors_args).unwrap();
        assert_same_spec(&neighbors_only, &direct);

        let mut mixed_args = args;
        mixed_args[2] = Value::Ref(count_object);
        mixed_args[8] = Value::Ref(neighbors_object);
        let mixed_args = with_guard_descriptors(mixed_args, Value::Int(1), Value::Int(1));
        assert_matches_reference_guard(&heap, &mixed_args);
        let mixed = Spec::read_guard(&heap, &mixed_args).unwrap();
        assert_same_spec(&mixed, &direct);
    }

    #[test]
    fn spec_guard_requires_exact_arity_and_refuses_strings_in_each_original_slot() {
        let (heap, args, _) = valid_inputs();
        let too_short = with_guard_descriptors(args.clone(), Value::Int(-1), Value::Int(-1));
        assert_matches_reference_guard(&heap, &too_short[..11]);
        assert!(Spec::read_guard(&heap, &too_short[..11]).is_none());
        let mut too_long = too_short.clone();
        too_long.push(Value::Na);
        assert_matches_reference_guard(&heap, &too_long);
        assert!(Spec::read_guard(&heap, &too_long).is_none());

        for position in 0..10 {
            let mut guarded = args.clone();
            guarded[position] = Value::String(format!("string at original slot {position}"));
            let count_descriptor = Value::Int(if position == 2 { 0 } else { -1 });
            let neighbors_descriptor = Value::Int(if position == 8 { 0 } else { -1 });
            let guarded = with_guard_descriptors(guarded, count_descriptor, neighbors_descriptor);
            assert_matches_reference_guard(&heap, &guarded);
            assert!(
                Spec::read_guard(&heap, &guarded).is_none(),
                "guard must reject String in original slot {position}"
            );
        }
    }

    #[test]
    fn spec_guard_requires_integer_descriptors_and_only_minus_one_is_a_sentinel() {
        let (heap, args, _) = valid_inputs();
        let invalid_descriptors = vec![
            Value::Bool(true),
            Value::Na,
            Value::Float(0.0),
            Value::String("0".to_owned()),
            Value::Ref(HandleId(u32::MAX)),
            Value::Int(-2),
            Value::Int(i64::MIN),
        ];
        for invalid in invalid_descriptors {
            let guarded = with_guard_descriptors(args.clone(), invalid.clone(), Value::Int(-1));
            assert_matches_reference_guard(&heap, &guarded);
            assert!(Spec::read_guard(&heap, &guarded).is_none());
            let guarded = with_guard_descriptors(args.clone(), Value::Int(-1), invalid);
            assert_matches_reference_guard(&heap, &guarded);
            assert!(Spec::read_guard(&heap, &guarded).is_none());
        }
    }

    #[test]
    fn spec_guard_field_descriptors_require_live_objects_in_range_integer_fields() {
        let (mut heap, args, features) = valid_inputs();

        for position in [2, 8] {
            for receiver in [
                Value::Bool(true),
                Value::Float(2.0),
                Value::Int(2),
                Value::Ref(features[0]),        // An array is not an object.
                Value::Ref(HandleId(u32::MAX)), // A dangling handle is not an object.
            ] {
                let mut guarded = args.clone();
                guarded[position] = receiver;
                let count_descriptor = Value::Int(if position == 2 { 0 } else { -1 });
                let neighbors_descriptor = Value::Int(if position == 8 { 0 } else { -1 });
                let guarded =
                    with_guard_descriptors(guarded, count_descriptor, neighbors_descriptor);
                assert_matches_reference_guard(&heap, &guarded);
                assert!(Spec::read_guard(&heap, &guarded).is_none());
            }

            for field in [
                Value::Bool(true),
                Value::Float(2.0),
                Value::Na,
                Value::String("not an integer".to_owned()),
                Value::Ref(features[0]),
                Value::Int(i64::MAX), // A positive, out-of-range object-field index.
            ] {
                let object = alloc_object(&mut heap, vec![field]);
                let mut guarded = args.clone();
                guarded[position] = Value::Ref(object);
                let count_descriptor = Value::Int(if position == 2 { 0 } else { -1 });
                let neighbors_descriptor = Value::Int(if position == 8 { 0 } else { -1 });
                let guarded =
                    with_guard_descriptors(guarded, count_descriptor, neighbors_descriptor);
                assert_matches_reference_guard(&heap, &guarded);
                assert!(Spec::read_guard(&heap, &guarded).is_none());
            }

            let short_object = alloc_object(&mut heap, vec![Value::Int(2)]);
            let mut guarded = args.clone();
            guarded[position] = Value::Ref(short_object);
            let count_descriptor = Value::Int(if position == 2 { 1 } else { -1 });
            let neighbors_descriptor = Value::Int(if position == 8 { 1 } else { -1 });
            let guarded = with_guard_descriptors(guarded, count_descriptor, neighbors_descriptor);
            assert_matches_reference_guard(&heap, &guarded);
            assert!(Spec::read_guard(&heap, &guarded).is_none());
        }
    }

    #[test]
    fn spec_guard_shares_spec_reader_validation_and_refusal_behavior() {
        let (heap, args, _) = valid_inputs();
        for (position, invalid) in [
            (0, Value::Int(-1)),
            (1, Value::Int(i64::MAX)),
            (2, Value::Int(1)),
            (8, Value::Int(0)),
            (9, Value::Float(f64::INFINITY)),
            (3, Value::Int(0)),
        ] {
            let mut raw = args.clone();
            raw[position] = invalid;
            let guarded = with_guard_descriptors(raw.clone(), Value::Int(-1), Value::Int(-1));
            assert_matches_reference_guard(&heap, &guarded);
            assert!(Spec::read(&heap, &raw).is_none());
            assert!(Spec::read_guard(&heap, &guarded).is_none());
        }
    }
}
