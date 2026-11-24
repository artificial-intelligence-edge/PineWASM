//! The per-run VM heap backing reference-typed collections.
//!
//! PineWASM's collections (`array`, `matrix`, `map`) use
//! **reference semantics**: a variable holds a [`HandleId`], not the
//! collection's contents. Assignment copies the handle, so two variables can
//! alias one heap object and observe each other's mutations. `var`-held
//! collections persist across bars because the handle is carried forward and
//! the heap object lives for the whole run.
//!
//! This module owns the storage side of that model: [`Heap`] is a flat arena
//! of [`HeapObj`] values allocated in handle order. [`Heap::collect`] drops
//! objects the program can no longer reach and returns their element and
//! object budget, so both caps bound the *live* set rather than everything a
//! run has ever built; the VM decides when to call it. Collected indices are
//! never reissued, so a handle names at most one object for the life of a run.
//! The typing, CSIL opcodes, and per-operation semantics that drive this heap
//! are added by the collection features; this is the fixed foundation
//! they build on.

use crate::heap_rollback::{self, RebaseError, RefGraph};
use crate::value::{self, HandleId, Value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The element type of a collection, tracked so heterogeneous inserts can be
/// rejected and `na` fills produce the right typed zero. Mirrors the scalar
/// types a collection may hold (`array.new<float>()` etc.); `matrix`/`map`
/// reuse the same set for their element / key+value types. Nested collections
/// are not representable; they are allowed only through UDTs (see the type
/// layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ElemType {
    Float,
    Int,
    Bool,
    String,
    Color,
    /// A user-defined-type element (collections-of-UDTs): the payload is the
    /// object's `type_id` (its index into `HirProgram::object_types`). Elements
    /// are reference values ([`Value::Ref`]) or `na`; nesting a collection or
    /// UDT through a UDT field is the one form of nesting that is allowed.
    Object(u32),
}

impl ElemType {
    pub fn describe(self) -> &'static str {
        match self {
            ElemType::Float => "float",
            ElemType::Int => "int",
            ElemType::Bool => "bool",
            ElemType::String => "string",
            ElemType::Color => "color",
            // The concrete type name is not carried here (only the id); callers
            // that need it resolve through the program's object-type table.
            ElemType::Object(_) => "object",
        }
    }

    /// The HIR scalar [`Type`](pinewasm_hir::Type) this element type
    /// corresponds to. The inverse of [`ElemType::from_hir_type`]. Object
    /// element types are not representable as a scalar name here (the object
    /// name lives in the program's type table), so this returns the erased
    /// [`pinewasm_hir::Type::Object`] with an empty name — callers that need the concrete
    /// name carry it out of band.
    pub fn to_hir_type(self) -> pinewasm_hir::Type {
        use pinewasm_hir::Type;
        match self {
            ElemType::Float => Type::Float,
            ElemType::Int => Type::Int,
            ElemType::Bool => Type::Bool,
            ElemType::String => Type::String,
            ElemType::Color => Type::Color,
            ElemType::Object(_) => Type::Object(String::new()),
        }
    }

    /// The element type for an HIR scalar [`Type`](pinewasm_hir::Type), or
    /// `None` for non-scalar types (void, series, collections, error). A
    /// [`pinewasm_hir::Type::Object`] maps to an object element type, but its `type_id` is not
    /// recoverable from the name alone — use [`ElemType::object_from_type_id`]
    /// when the id is known; this scalar path returns `None` for objects.
    pub fn from_hir_type(ty: &pinewasm_hir::Type) -> Option<ElemType> {
        use pinewasm_hir::Type;
        match ty {
            Type::Float => Some(ElemType::Float),
            Type::Int => Some(ElemType::Int),
            Type::Bool => Some(ElemType::Bool),
            Type::String => Some(ElemType::String),
            Type::Color => Some(ElemType::Color),
            _ => None,
        }
    }

    /// An object element type for the given `type_id`.
    pub fn object_from_type_id(type_id: u32) -> ElemType {
        ElemType::Object(type_id)
    }

    /// A stable integer tag for this element type. Used to pass a collection's
    /// key/value types through the CSIL as `ConstInt` arguments (the checker
    /// materializes a map constructor's key/value types as two int tags the VM
    /// decodes here). The inverse is [`ElemType::from_tag`].
    pub fn to_tag(self) -> i64 {
        match self {
            ElemType::Float => 0,
            ElemType::Int => 1,
            ElemType::Bool => 2,
            ElemType::String => 3,
            ElemType::Color => 4,
            // Object element types are encoded above the scalar range as
            // `OBJECT_TAG_BASE + type_id`, so `from_tag` recovers the id.
            ElemType::Object(id) => Self::OBJECT_TAG_BASE + id as i64,
        }
    }

    /// The tag offset above which a value encodes an object element type
    /// (`OBJECT_TAG_BASE + type_id`); below it are the scalar element types.
    const OBJECT_TAG_BASE: i64 = 1000;

    /// Decode a [`ElemType::to_tag`] value, or `None` for an out-of-range tag
    /// (defense in depth — a validated program never produces one).
    ///
    /// An object tag whose id does not fit a `u32` (`tag - OBJECT_TAG_BASE >
    /// u32::MAX`) is out of range too. The checked conversion makes `from_tag`
    /// injective on the tags it accepts, and it is the exact inverse of `to_tag`.
    pub fn from_tag(tag: i64) -> Option<ElemType> {
        match tag {
            0 => Some(ElemType::Float),
            1 => Some(ElemType::Int),
            2 => Some(ElemType::Bool),
            3 => Some(ElemType::String),
            4 => Some(ElemType::Color),
            t if t >= Self::OBJECT_TAG_BASE => match u32::try_from(t - Self::OBJECT_TAG_BASE) {
                Ok(id) => Some(ElemType::Object(id)),
                Err(_) => None,
            },
            _ => None,
        }
    }

    /// Test-only decoder with an unchecked `as u32` cast, used to cross-check
    /// [`ElemType::from_tag`].
    #[cfg(test)]
    fn from_tag_reference(tag: i64) -> Option<ElemType> {
        match tag {
            0 => Some(ElemType::Float),
            1 => Some(ElemType::Int),
            2 => Some(ElemType::Bool),
            3 => Some(ElemType::String),
            4 => Some(ElemType::Color),
            t if t >= Self::OBJECT_TAG_BASE => {
                Some(ElemType::Object((t - Self::OBJECT_TAG_BASE) as u32))
            }
            _ => None,
        }
    }

    /// The `na`-like default a freshly grown/`fill`ed slot of this type takes
    /// before an explicit value is written. An omitted initial value defaults
    /// to `na` for every type, so the default is uniformly [`Value::Na`]. Kept
    /// as a method so a future typed-zero policy has a single call site.
    pub fn default_value(self) -> Value {
        Value::Na
    }
}

/// A heap-allocated collection object. Insertion order is preserved for maps.
/// All three shapes carry their element
/// type(s) so mutating operations can type-check their arguments at runtime as
/// defense in depth behind the static checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HeapObj {
    Array {
        elem_ty: ElemType,
        items: Vec<Value>,
    },
    Matrix {
        elem_ty: ElemType,
        rows: usize,
        cols: usize,
        /// Row-major, `rows * cols` cells.
        items: Vec<Value>,
    },
    Map {
        key_ty: ElemType,
        val_ty: ElemType,
        /// Insertion-ordered key/value pairs.
        entries: Vec<(Value, Value)>,
    },
    /// A user-defined-type instance. `type_id` identifies the declared
    /// type (its index into the program's object-type table); `fields` are the
    /// field values in declaration (slot) order. Field values may themselves be
    /// `Value::Ref` (a UDT-typed or collection-typed field), so an object can
    /// point at other heap objects. Scalar field typing is enforced statically
    /// by the checker; the runtime stores values positionally.
    Object {
        type_id: u32,
        fields: Vec<Value>,
        /// Runtime-only table lifetime identity. Heap cloning/rollback keeps
        /// it with the object even when its physical handle is remapped.
        /// A new object starts without an identity; table registration assigns
        /// one. It is never transported or constructed by serialized input.
        /// Heap JSON is structural data, not an identity-preserving VM resume
        /// format; in-process snapshots clone this metadata with the object.
        #[serde(skip)]
        table_identity: Option<std::num::NonZeroU64>,
    },
}

impl HeapObj {
    /// Total element count for the [`Heap::max_elements`] budget: array length,
    /// matrix cell count, or map entry count.
    pub fn element_count(&self) -> usize {
        match self {
            HeapObj::Array { items, .. } => items.len(),
            HeapObj::Matrix { items, .. } => items.len(),
            HeapObj::Map { entries, .. } => entries.len(),
            // Each field is one element for the heap-budget accounting.
            HeapObj::Object { fields, .. } => fields.len(),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            HeapObj::Array { .. } => "array",
            HeapObj::Matrix { .. } => "matrix",
            HeapObj::Map { .. } => "map",
            HeapObj::Object { .. } => "object",
        }
    }

    /// Visit every [`Value`] this object contains, including map *keys* (a key
    /// is typed by `key_ty` and so may itself be a `Value::Ref`). Written as an
    /// exhaustive `match` so a future variant carrying values cannot be added
    /// without the compiler pointing at [`Heap::collect`]'s traversal.
    pub fn visit_values(&self, f: &mut dyn FnMut(&Value)) {
        match self {
            HeapObj::Array { items, .. } | HeapObj::Matrix { items, .. } => {
                for v in items {
                    f(v);
                }
            }
            HeapObj::Map { entries, .. } => {
                for (k, v) in entries {
                    f(k);
                    f(v);
                }
            }
            HeapObj::Object { fields, .. } => {
                for v in fields {
                    f(v);
                }
            }
        }
    }

    /// Visit embedded values in order, stopping when `f` returns `false`.
    /// Used by budget-bounded graph planning, where completing a large object
    /// scan after the caller's work budget is exhausted would itself be an
    /// unmetered denial of service.
    fn visit_values_until(&self, f: &mut dyn FnMut(&Value) -> bool) -> bool {
        match self {
            HeapObj::Array { items, .. } | HeapObj::Matrix { items, .. } => {
                for value in items {
                    if !f(value) {
                        return false;
                    }
                }
            }
            HeapObj::Map { entries, .. } => {
                for (key, value) in entries {
                    if !f(key) || !f(value) {
                        return false;
                    }
                }
            }
            HeapObj::Object { fields, .. } => {
                for value in fields {
                    if !f(value) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Rewrite every embedded value, including map keys. Heap lifecycle
    /// operations use this when a reference becomes `na` or is remapped.
    fn visit_values_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        match self {
            HeapObj::Array { items, .. } | HeapObj::Matrix { items, .. } => {
                for value in items {
                    f(value);
                }
            }
            HeapObj::Map { entries, .. } => {
                for (key, value) in entries {
                    f(key);
                    f(value);
                }
            }
            HeapObj::Object { fields, .. } => {
                for value in fields {
                    f(value);
                }
            }
        }
    }

    /// The heap indices named by this object's [`Value::Ref`]s, in
    /// [`HeapObj::visit_values`] order (a map key before its value).
    pub fn ref_indices(&self) -> Vec<usize> {
        let mut out = Vec::new();
        match self {
            HeapObj::Array { items, .. } => push_ref_indices(items, &mut out),
            HeapObj::Matrix { items, .. } => push_ref_indices(items, &mut out),
            HeapObj::Map { entries, .. } => {
                let mut i = 0;
                while i < entries.len() {
                    push_ref_index(&entries[i].0, &mut out);
                    push_ref_index(&entries[i].1, &mut out);
                    i += 1;
                }
            }
            HeapObj::Object { fields, .. } => push_ref_indices(fields, &mut out),
        }
        out
    }

    /// Rewrite every [`Value::Ref`] `h` of this object, in
    /// [`HeapObj::visit_values`] order, to the handle `remap` gives `h`
    /// ([`heap_rollback::remapped_index`]). A reference without an entry is
    /// left as it is; the last such handle is returned.
    fn remap_refs(&mut self, remap: &[Option<usize>]) -> Option<HandleId> {
        match self {
            HeapObj::Array { items, .. } => remap_values(items, remap),
            HeapObj::Matrix { items, .. } => remap_values(items, remap),
            HeapObj::Map { entries, .. } => {
                let mut dangling = None;
                let mut i = 0;
                while i < entries.len() {
                    dangling = remap_value(&mut entries[i].0, remap, dangling);
                    dangling = remap_value(&mut entries[i].1, remap, dangling);
                    i += 1;
                }
                dangling
            }
            HeapObj::Object { fields, .. } => remap_values(fields, remap),
        }
    }
}

/// Append the heap index of `item` to `out` if it is a reference.
fn push_ref_index(item: &Value, out: &mut Vec<usize>) {
    if let Some(handle) = value::ref_handle(item) {
        out.push(handle.index());
    }
}

/// Append the heap indices of the references among `values`, in order.
fn push_ref_indices(values: &[Value], out: &mut Vec<usize>) {
    let mut i = 0;
    while i < values.len() {
        push_ref_index(&values[i], out);
        i += 1;
    }
}

/// Rewrite `item` through `remap` if it is a reference with an entry.
/// Returns the handle of a reference without an entry, else `dangling`.
fn remap_value(
    item: &mut Value,
    remap: &[Option<usize>],
    dangling: Option<HandleId>,
) -> Option<HandleId> {
    match value::ref_handle(item) {
        None => dangling,
        Some(old) => match heap_rollback::remapped_index(remap, old.index()) {
            Some(new) => {
                value::set_ref(item, HandleId(new as u32));
                dangling
            }
            None => Some(old),
        },
    }
}

/// [`remap_value`] over `values` in order, starting from no dangling handle.
fn remap_values(values: &mut [Value], remap: &[Option<usize>]) -> Option<HandleId> {
    let mut dangling = None;
    let mut i = 0;
    while i < values.len() {
        dangling = remap_value(&mut values[i], remap, dangling);
        i += 1;
    }
    dangling
}

/// Which heap budget a [`HeapLimitError`] tripped: the run-wide live
/// *element* count, or the run-wide live *object* (arena slot) count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapLimitKind {
    Elements,
    Objects,
}

/// Structured error raised when a collection operation would push the run's
/// total live element count past [`Heap::max_elements`], or allocate more
/// objects than [`DEFAULT_MAX_LIVE_OBJECTS`]. Surfaced to the VM as a runtime
/// error rather than aborting the process;
/// never a panic. The VM maps this into its own `RuntimeError` so this crate
/// stays free of VM types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapLimitError {
    /// Which budget was hit.
    pub kind: HeapLimitKind,
    /// The cap that was hit (`Heap::max_elements` or the object cap).
    pub limit: usize,
    /// How many elements (or objects) the operation attempted to add.
    pub attempted: usize,
}

/// Failure while restoring a bar-start heap together with the graph that
/// remains observable through `varip` roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeapRestoreError {
    DanglingReference(HandleId),
    MismatchedLimits,
    Limit(HeapLimitError),
    ArithmeticOverflow,
}

impl std::fmt::Display for HeapRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DanglingReference(handle) => {
                write!(f, "varip heap graph contains dangling handle {handle:?}")
            }
            Self::MismatchedLimits => write!(
                f,
                "heap snapshot resource limits do not match the live heap"
            ),
            Self::Limit(error) => error.fmt(f),
            Self::ArithmeticOverflow => write!(f, "varip heap restore accounting overflowed"),
        }
    }
}

impl std::error::Error for HeapRestoreError {}

impl std::fmt::Display for HeapLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            HeapLimitKind::Elements => write!(
                f,
                "collection element limit exceeded: adding {} element(s) would exceed the limit of {}",
                self.attempted, self.limit
            ),
            HeapLimitKind::Objects => write!(
                f,
                "collection object limit exceeded: allocating another object would exceed the limit of {} live objects",
                self.limit
            ),
        }
    }
}

impl std::error::Error for HeapLimitError {}

/// Default `max_collection_elements` cap (per the `RunOptions` contract): the
/// total number of live collection elements across the whole run. Guards
/// against heap-growth bombs (e.g. a loop that pushes forever). The VM plumbs
/// its `RunOptions` value in; this constant is the documented default.
pub const DEFAULT_MAX_COLLECTION_ELEMENTS: usize = 100_000;

/// Cap on the number of live heap *objects* ([`Heap::len`]). The element budget
/// alone does not bound them: a zero-element allocation (an empty array, map,
/// or matrix) charges nothing, so [`MAX_ARENA_SLOTS`] would otherwise be the
/// only heap limit on a per-bar loop allocating empties. That bound, not this
/// cap, limits the arena, which also keeps one `None` per collected object (a
/// collected slot is never reused). One million live objects bounds their
/// slots to tens of megabytes while leaving generous headroom for legitimate
/// drawing-heavy scripts (which allocate a handful of objects per bar).
pub const DEFAULT_MAX_LIVE_OBJECTS: usize = 1_000_000;

/// Hard bound on arena slots, live or collected. [`HandleId`] is a `u32` index
/// and [`Heap::collect`] never reissues one, so total allocations over a run
/// are capped by the handle space; this makes reaching it a structured error
/// instead of a wrapped index aliasing two objects.
pub const MAX_ARENA_SLOTS: usize = u32::MAX as usize;

// Explicit `match`es instead of `Option::map`/`?` here and below: the Aeneas
// extraction (`aeneas_roots_heap_glue`) has no model for closures or `Try`.
#[allow(clippy::manual_map)]
fn next_handle_id(arena_len: usize) -> Option<HandleId> {
    match heap_rollback::next_handle_index(arena_len) {
        Some(index) => Some(HandleId(index)),
        None => None,
    }
}

/// Test-only copy of `next_handle_id`, used to cross-check it.
#[cfg(test)]
fn next_handle_id_reference(arena_len: usize) -> Option<HandleId> {
    match u32::try_from(arena_len) {
        Ok(index) if index < u32::MAX => Some(HandleId(index)),
        _ => None,
    }
}

fn default_max_objects() -> usize {
    DEFAULT_MAX_LIVE_OBJECTS
}

/// A per-run arena of heap objects. Handles index directly into `objects`, so
/// a slot's index is its identity for the whole run: never moved, and never
/// handed to a second object.
///
/// [`Heap::collect`] drops an unreachable object and returns its elements and
/// its object-budget charge, but leaves the index spent. Two reasons not to
/// recycle indices, both load-bearing:
///
/// * A handle names one object for the whole run. Slot history, the drawing
///   registry and its table grids all hold handles, and a recycled index
///   would let a stale one name a different object. (A handle is not an
///   output: a drawing's reported `id` is its own registration ordinal.)
/// * It bounds the blast radius of a root-enumeration slip. A handle whose
///   object was wrongly collected reads as absent and raises the ordinary
///   structured error, instead of silently resolving to whatever object landed
///   in the slot next.
///
/// The residue is one `None` per collected object (the object's own storage is
/// freed). See `docs/runtime.md` for why that is bounded in practice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heap {
    /// `None` marks a collected slot. `Option` rather than a `HeapObj::Free`
    /// variant so every existing `match` on [`HeapObj`] stays exhaustive, and
    /// so `get`/`get_mut` keep their signatures: a collected slot reads as
    /// absent through the same path as an out-of-range handle.
    objects: Vec<Option<HeapObj>>,
    /// How many arena slots [`Heap::collect`] has emptied. Live objects are
    /// `objects.len() - collected`, which is what the object budget caps.
    ///
    /// Counting the *collected* slots rather than the live ones is what makes
    /// the serde default correct: a heap written before collection existed has
    /// no empty slots, so `0` is its true value and its live count still comes
    /// out as `objects.len()`.
    #[serde(default)]
    collected: usize,
    /// Total live element count across all objects. Maintained incrementally by
    /// [`Heap::reserve`] so the limit check is O(1) rather than a full scan.
    live_elements: usize,
    /// The `max_collection_elements` cap for this run.
    max_elements: usize,
    /// The live-object (arena slot) cap for this run. Serde-defaulted so heaps
    /// serialized before the cap existed deserialize with the documented value.
    #[serde(default = "default_max_objects")]
    max_objects: usize,
    /// Non-semantic cache invalidation stamps. Appending retains the old
    /// prefix; every unrestricted mutable borrow invalidates it. Not persisted.
    #[serde(skip)]
    prefix_revisions: BTreeMap<HandleId, u64>,
}

/// Planned mutation of reachable heap values that alias a deleted drawing.
/// Construct it with [`Heap::plan_refs_to_na`], charge its work, then apply it
/// before the heap changes.
#[derive(Debug, Clone)]
pub struct RefNaPlan {
    affected: Vec<HandleId>,
    work_units: u64,
}

impl RefNaPlan {
    /// Instruction work units needed for the graph walk and rewrite.
    pub fn work_units(&self) -> u64 {
        self.work_units
    }
}

impl PartialEq for Heap {
    fn eq(&self, other: &Self) -> bool {
        self.objects == other.objects
            && self.collected == other.collected
            && self.live_elements == other.live_elements
            && self.max_elements == other.max_elements
            && self.max_objects == other.max_objects
    }
}

impl Heap {
    /// A heap with the given element cap. `0` is treated as "no collections may
    /// hold any element" — every growth fails — which is a degenerate but
    /// well-defined configuration, not a panic. The object-slot cap is always
    /// [`DEFAULT_MAX_LIVE_OBJECTS`].
    pub fn with_limit(max_elements: usize) -> Self {
        Heap {
            objects: Vec::new(),
            collected: 0,
            live_elements: 0,
            max_elements,
            max_objects: DEFAULT_MAX_LIVE_OBJECTS,
            prefix_revisions: BTreeMap::new(),
        }
    }

    /// A heap with explicit element and object caps (tests exercise small
    /// object caps without allocating a million objects).
    pub fn with_limits(max_elements: usize, max_objects: usize) -> Self {
        Heap {
            max_objects,
            ..Self::with_limit(max_elements)
        }
    }

    pub fn new() -> Self {
        Self::with_limit(DEFAULT_MAX_COLLECTION_ELEMENTS)
    }

    pub fn max_elements(&self) -> usize {
        self.max_elements
    }

    pub fn live_elements(&self) -> usize {
        self.live_elements
    }

    /// Live object count: arena slots currently holding an object. This is what
    /// the object budget caps, so after a [`Heap::collect`] it falls.
    pub fn len(&self) -> usize {
        self.objects.len() - self.collected
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Arena size including collected slots. Only the growth heuristic that
    /// decides *when* to collect looks at this; the budget is [`Heap::len`].
    pub fn arena_len(&self) -> usize {
        self.objects.len()
    }

    /// Allocate `obj`, returning its handle. The object's initial elements are
    /// charged against the element budget, and the arena slot against the
    /// object budget; a construction that already exceeds either cap (e.g.
    /// `array.new<float>(200_000)`, or the millionth-and-first live array)
    /// fails without allocating.
    ///
    /// The budget is against *live* objects, so a program that churns
    /// collections bar over bar is charged only for what it still holds rather
    /// than for everything it has ever built.
    #[allow(clippy::question_mark)]
    pub fn alloc(&mut self, mut obj: HeapObj) -> Result<HandleId, HeapLimitError> {
        if self.len() >= self.max_objects {
            return Err(HeapLimitError {
                kind: HeapLimitKind::Objects,
                limit: self.max_objects,
                attempted: 1,
            });
        }
        // A handle is a u32 index and collected indices are never reissued, so
        // the arena is bounded by the handle space rather than by the live
        // budget. Refuse at the limit: past it the `as u32` would wrap and
        // alias a fresh handle onto an existing object, which is a silently
        // wrong answer rather than an error. Unreachable in practice (the
        // arena would need ~200 GB first), but it must fail as a budget, not
        // as corruption.
        // Use a checked conversion rather than relying on the ordinary append
        // path as the only constructor: Heap also has a serialized form. The
        // explicit upper bound remains meaningful on 32-bit hosts, where the
        // cap is `usize::MAX`, and reserves u32::MAX as the refusal boundary.
        let Some(id) = next_handle_id(self.objects.len()) else {
            return Err(HeapLimitError {
                kind: HeapLimitKind::Objects,
                limit: MAX_ARENA_SLOTS,
                attempted: 1,
            });
        };
        let count = obj.element_count();
        // Explicit `if let` rather than `?`: the Aeneas extraction has no model
        // for `Try`.
        if let Err(error) = self.reserve(count) {
            return Err(error);
        }
        // Allocation creates a new logical object, even if its fields came
        // from a clone. Snapshot/retained-graph restoration installs cloned
        // objects directly, preserving their existing lifetime instead.
        if let HeapObj::Object { table_identity, .. } = &mut obj {
            *table_identity = None;
        }
        self.objects.push(Some(obj));
        Ok(id)
    }

    /// Restore the snapshot while preserving the current transitive heap graph
    /// reachable from `roots`. Pre-frontier handles keep their IDs; reachable
    /// post-frontier objects are packed into a dense suffix and every nested
    /// reference is rewritten. Unreachable tick allocations are discarded.
    /// The operation is transactional: it builds a separate heap and remap and
    /// returns an error without mutating either input.
    pub fn restore_preserving_roots(
        snapshot: &Heap,
        current: &Heap,
        roots: &[HandleId],
    ) -> Result<(Heap, BTreeMap<HandleId, HandleId>), HeapRestoreError> {
        let restored = restore_graph(snapshot, current, &handle_indices(roots))?;
        // Handles are u32, so every index converts back exactly.
        let remap: BTreeMap<HandleId, HandleId> = restored
            .reachable
            .iter()
            .zip(&restored.targets)
            .map(|(handle, target)| (HandleId(*handle as u32), HandleId(*target as u32)))
            .collect();
        Ok((restored.heap, remap))
    }

    /// Test-only copy of [`Heap::restore_preserving_roots`], used for
    /// differential tests.
    #[cfg(test)]
    pub(crate) fn restore_preserving_roots_reference(
        snapshot: &Heap,
        current: &Heap,
        roots: &[HandleId],
    ) -> Result<(Heap, BTreeMap<HandleId, HandleId>), HeapRestoreError> {
        if snapshot.max_elements != current.max_elements
            || snapshot.max_objects != current.max_objects
        {
            return Err(HeapRestoreError::MismatchedLimits);
        }

        let mut reachable = BTreeSet::new();
        let mut work = roots.to_vec();
        while let Some(handle) = work.pop() {
            if !reachable.insert(handle) {
                continue;
            }
            let object = current
                .get(handle)
                .ok_or(HeapRestoreError::DanglingReference(handle))?;
            for child in object.ref_indices() {
                work.push(HandleId(child as u32));
            }
        }

        let frontier = snapshot.arena_len();
        let mut remap = BTreeMap::new();
        let mut next = frontier;
        for handle in &reachable {
            if handle.index() < frontier {
                if snapshot.get(*handle).is_none() {
                    return Err(HeapRestoreError::DanglingReference(*handle));
                }
                remap.insert(*handle, *handle);
            } else {
                let id = next_handle_id(next).ok_or({
                    HeapRestoreError::Limit(HeapLimitError {
                        kind: HeapLimitKind::Objects,
                        limit: MAX_ARENA_SLOTS,
                        attempted: 1,
                    })
                })?;
                remap.insert(*handle, id);
                next = next
                    .checked_add(1)
                    .ok_or(HeapRestoreError::ArithmeticOverflow)?;
            }
        }

        let mut restored = snapshot.clone();
        let mut live_elements = snapshot.live_elements;
        let mut live_objects = snapshot.len();
        let mut appended = frontier;
        let mut overlays = Vec::with_capacity(reachable.len());
        for handle in reachable {
            let target = *remap
                .get(&handle)
                .ok_or(HeapRestoreError::DanglingReference(handle))?;
            let mut object = current
                .get(handle)
                .cloned()
                .ok_or(HeapRestoreError::DanglingReference(handle))?;
            let mut dangling = None;
            object.visit_values_mut(&mut |value| {
                if let Value::Ref(old) = value {
                    if let Some(new) = remap.get(old) {
                        *old = *new;
                    } else {
                        dangling = Some(*old);
                    }
                }
            });
            if let Some(handle) = dangling {
                return Err(HeapRestoreError::DanglingReference(handle));
            }
            if target.index() < frontier {
                let previous = restored.objects[target.index()]
                    .as_ref()
                    .ok_or(HeapRestoreError::DanglingReference(target))?;
                live_elements = live_elements
                    .checked_sub(previous.element_count())
                    .and_then(|n| n.checked_add(object.element_count()))
                    .ok_or(HeapRestoreError::ArithmeticOverflow)?;
            } else {
                if target.index() != appended {
                    return Err(HeapRestoreError::ArithmeticOverflow);
                }
                appended = appended
                    .checked_add(1)
                    .ok_or(HeapRestoreError::ArithmeticOverflow)?;
                live_elements = live_elements
                    .checked_add(object.element_count())
                    .ok_or(HeapRestoreError::ArithmeticOverflow)?;
                live_objects = live_objects
                    .checked_add(1)
                    .ok_or(HeapRestoreError::ArithmeticOverflow)?;
            }
            overlays.push((target, object));
        }
        if live_elements > snapshot.max_elements {
            return Err(HeapRestoreError::Limit(HeapLimitError {
                kind: HeapLimitKind::Elements,
                limit: snapshot.max_elements,
                attempted: live_elements.saturating_sub(snapshot.live_elements),
            }));
        }
        if live_objects > snapshot.max_objects {
            return Err(HeapRestoreError::Limit(HeapLimitError {
                kind: HeapLimitKind::Objects,
                limit: snapshot.max_objects,
                attempted: live_objects.saturating_sub(snapshot.len()),
            }));
        }
        for (target, object) in overlays {
            if target.index() == restored.objects.len() {
                restored.objects.push(Some(object));
            } else {
                restored.objects[target.index()] = Some(object);
            }
        }
        restored.live_elements = live_elements;
        restored.collected = snapshot.collected;
        Ok((restored, remap))
    }

    /// Immutable access to a heap object. Returns `None` for an out-of-range
    /// handle or a collected slot (defense in depth — a validated program with
    /// a complete root set never produces either).
    pub fn get(&self, h: HandleId) -> Option<&HeapObj> {
        let index = h.index();
        if index < self.objects.len() {
            match &self.objects[index] {
                Some(object) => Some(object),
                None => None,
            }
        } else {
            None
        }
    }

    /// Mutable access to a heap object. Callers that grow the object must call
    /// [`Heap::reserve`] first so the element budget stays accurate.
    pub fn get_mut(&mut self, h: HandleId) -> Option<&mut HeapObj> {
        if let Some(revision) = self.prefix_revisions.get_mut(&h) {
            *revision = revision.saturating_add(1);
        }
        self.objects.get_mut(h.index())?.as_mut()
    }

    /// Find reachable heap objects that contain `target` and estimate the
    /// work required to rewrite their values. The returned unit count covers
    /// the graph walk, value inspection, and the later rewrite of affected
    /// objects. The caller charges it before applying the plan.
    pub fn plan_refs_to_na(
        &self,
        target: HandleId,
        roots: &[HandleId],
        max_work: u64,
    ) -> Option<RefNaPlan> {
        let mut reachable = BTreeSet::new();
        let mut pending = roots.to_vec();
        let mut affected = Vec::new();
        let mut work_units = 0u64;
        while let Some(handle) = pending.pop() {
            if !reachable.insert(handle) {
                continue;
            }
            work_units = work_units.saturating_add(1);
            if work_units > max_work {
                return None;
            }
            if let Some(object) = self.get(handle) {
                let mut value_count = 0u64;
                let mut contains_target = false;
                let complete = object.visit_values_until(&mut |value| {
                    value_count = value_count.saturating_add(1);
                    work_units = work_units.saturating_add(1);
                    if work_units > max_work {
                        return false;
                    }
                    if let Value::Ref(child) = value {
                        contains_target |= *child == target;
                        pending.push(*child);
                    }
                    true
                });
                if !complete {
                    return None;
                }
                if contains_target {
                    // `apply_refs_to_na` inspects these values once more.
                    work_units = work_units.saturating_add(value_count);
                    if work_units > max_work {
                        return None;
                    }
                    affected.push(handle);
                }
            }
        }
        Some(RefNaPlan {
            affected,
            work_units,
        })
    }

    /// Apply a plan produced by [`Heap::plan_refs_to_na`]. Returns the number
    /// of reference values replaced. The heap must not change between planning
    /// and applying; callers charge [`RefNaPlan::work_units`] first.
    pub fn apply_refs_to_na(&mut self, target: HandleId, plan: &RefNaPlan) -> usize {
        let mut replaced = 0;
        for handle in &plan.affected {
            if let Some(object) = self.get_mut(*handle) {
                object.visit_values_mut(&mut |value| {
                    if matches!(value, Value::Ref(reference) if *reference == target) {
                        *value = Value::Na;
                        replaced += 1;
                    }
                });
            }
        }
        replaced
    }

    /// Apply the same charged alias plan for same-position table replacement.
    /// Unlike explicit deletion, displacement preserves the supplied table
    /// lifetime identity in the unavailable alias, without a heap reference.
    /// The heap must not change between planning and applying the plan.
    pub fn apply_refs_to_displaced_table(
        &mut self,
        target: HandleId,
        plan: &RefNaPlan,
        identity: std::num::NonZeroU64,
    ) -> usize {
        let mut replaced = 0;
        for handle in &plan.affected {
            if let Some(object) = self.get_mut(*handle) {
                object.visit_values_mut(&mut |value| {
                    if matches!(value, Value::Ref(reference) if *reference == target) {
                        *value = Value::DisplacedTable(identity);
                        replaced += 1;
                    }
                });
            }
        }
        replaced
    }

    /// Invalidation stamp for caches of an array's existing prefix. A
    /// saturated stamp disables reuse rather than allowing counter wraparound.
    pub fn array_prefix_revision(&self, h: HandleId) -> Option<u64> {
        let revision = self.prefix_revisions.get(&h).copied()?;
        (revision != u64::MAX).then_some(revision)
    }

    /// Start tracking a prefix. Bound the optimization's bookkeeping even for
    /// programs that repeatedly allocate unrelated arrays. A full table simply
    /// declines cache reuse; it never weakens invalidation.
    pub fn watch_array_prefix(&mut self, h: HandleId) -> Option<u64> {
        if self.prefix_revisions.len() < 32 {
            self.prefix_revisions.entry(h).or_insert(0);
        }
        self.array_prefix_revision(h)
    }

    /// Append without changing existing cells. The caller must already have
    /// coerced the value and reserved its element/string budget. All other
    /// mutations must use `get_mut`, which invalidates prefix caches.
    pub fn append_array_value(&mut self, h: HandleId, value: Value) -> bool {
        match self.objects.get_mut(h.index()).and_then(Option::as_mut) {
            Some(HeapObj::Array { items, .. }) => {
                items.push(value);
                true
            }
            _ => false,
        }
    }

    /// Drop every object unreachable from `roots`, returning their elements and
    /// their object-budget charge. Returns the number of objects collected.
    /// Their handles stay spent (see the type docs).
    ///
    /// Mark-sweep rather than refcounting because the object graph is cyclic in
    /// general: a UDT field may point back at a collection holding the object.
    /// Cost is O(arena + live elements), paid only when the caller's growth
    /// heuristic fires.
    ///
    /// **`roots` must be complete.** Any handle reachable by the program that is
    /// not transitively named here loses its object and reads as absent: most
    /// uses raise the ordinary structured error, but some raise nothing.
    /// `Vm::gc_roots` enumerates the roots and documents why each of the VM's
    /// other `Value` stores cannot hold a [`Value::Ref`].
    pub fn collect(&mut self, roots: &[HandleId]) -> usize {
        let mut marked = vec![false; self.objects.len()];
        let mut work: Vec<usize> = Vec::new();
        let mut r = 0;
        while r < roots.len() {
            mark_index(roots[r].index(), &mut marked, &mut work);
            r += 1;
        }
        while let Some(idx) = work.pop() {
            if let Some(obj) = &self.objects[idx] {
                let reached = obj.ref_indices();
                let mut j = 0;
                while j < reached.len() {
                    mark_index(reached[j], &mut marked, &mut work);
                    j += 1;
                }
            }
        }
        let mut freed = 0;
        let mut idx = 0;
        while idx < marked.len() {
            if !marked[idx] {
                if let Some(obj) = self.objects[idx].take() {
                    self.live_elements = self.live_elements.saturating_sub(obj.element_count());
                    self.collected += 1;
                    freed += 1;
                }
            }
            idx += 1;
        }
        freed
    }

    /// Test-only copy of [`Heap::collect`] (closure marker and
    /// [`HeapObj::visit_values`] traversal), used to cross-check it.
    #[cfg(test)]
    pub(crate) fn collect_reference(&mut self, roots: &[HandleId]) -> usize {
        let mut marked = vec![false; self.objects.len()];
        let mut work: Vec<usize> = Vec::new();
        let mark = |idx: usize, marked: &mut Vec<bool>, work: &mut Vec<usize>| {
            if idx < marked.len() && !marked[idx] {
                marked[idx] = true;
                work.push(idx);
            }
        };
        for r in roots {
            mark(r.index(), &mut marked, &mut work);
        }
        while let Some(idx) = work.pop() {
            let Some(obj) = self.objects[idx].as_ref() else {
                continue;
            };
            let mut reached: Vec<usize> = Vec::new();
            obj.visit_values(&mut |v| {
                if let Value::Ref(h) = v {
                    reached.push(h.index());
                }
            });
            for idx in reached {
                mark(idx, &mut marked, &mut work);
            }
        }
        let mut freed = 0;
        for (idx, reachable) in marked.iter().enumerate() {
            if *reachable {
                continue;
            }
            if let Some(obj) = self.objects[idx].take() {
                self.live_elements = self.live_elements.saturating_sub(obj.element_count());
                self.collected += 1;
                freed += 1;
            }
        }
        freed
    }

    /// Charge `additional` elements against the run-wide budget, failing (with
    /// no state change) if it would exceed the cap. Callers invoke this
    /// *before* growing an object so the failure leaves the heap untouched.
    /// Shrinking or replacing in place needs no reservation; callers that
    /// remove elements should call [`Heap::release`] to return budget.
    pub fn reserve(&mut self, additional: usize) -> Result<(), HeapLimitError> {
        let after = self.live_elements.saturating_add(additional);
        if after > self.max_elements {
            return Err(HeapLimitError {
                kind: HeapLimitKind::Elements,
                limit: self.max_elements,
                attempted: additional,
            });
        }
        self.live_elements = after;
        Ok(())
    }

    /// Return `removed` elements to the budget (after a `pop`/`remove`/`clear`).
    /// Saturates at zero so an accounting slip can never underflow.
    pub fn release(&mut self, removed: usize) {
        self.live_elements = self.live_elements.saturating_sub(removed);
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// Charon/Aeneas extraction roots for the heap (`--features aeneas`
/// only): thin, behavior-free wrappers naming `Heap::collect`, `Heap::alloc`
/// and the element-type tag codec, which Charon cannot start from directly
/// (inherent methods). `Value` stays opaque in the extraction; the heap reads
/// it only through [`crate::value::ref_handle`]. Parameter names avoid the module
/// names `heap`/`value` (an Aeneas name-capture issue).
#[cfg(feature = "aeneas")]
#[allow(dead_code)] // Extraction roots; never called from Rust.
pub(crate) mod aeneas_roots_heap_collect {
    use super::{ElemType, Heap, HeapLimitError, HeapObj};
    use crate::value::HandleId;

    /// Root for `Heap::collect`.
    pub fn root_heap_collect(arena: &mut Heap, roots: &[HandleId]) -> usize {
        arena.collect(roots)
    }

    /// Root for `Heap::alloc`.
    pub fn root_heap_alloc(arena: &mut Heap, obj: HeapObj) -> Result<HandleId, HeapLimitError> {
        arena.alloc(obj)
    }

    /// Root for `ElemType::from_tag`.
    pub fn root_elem_type_from_tag(tag: i64) -> Option<ElemType> {
        ElemType::from_tag(tag)
    }

    /// Root for `ElemType::to_tag`.
    pub fn root_elem_type_to_tag(ty: ElemType) -> i64 {
        ty.to_tag()
    }
}

/// The heap as seen by the rollback kernels: slot `i` is handle `HandleId(i)`.
impl RefGraph for Heap {
    fn slot_count(&self) -> usize {
        self.objects.len()
    }

    fn is_live(&self, index: usize) -> bool {
        index < self.objects.len() && self.objects[index].is_some()
    }

    #[allow(clippy::manual_map)]
    fn children(&self, index: usize) -> Option<Vec<usize>> {
        if index < self.objects.len() {
            match &self.objects[index] {
                Some(object) => Some(object.ref_indices()),
                None => None,
            }
        } else {
            None
        }
    }
}

/// Mark `idx` and queue it for [`Heap::collect`]'s traversal, unless it is
/// out of range or already marked.
fn mark_index(idx: usize, marked: &mut [bool], work: &mut Vec<usize>) {
    if idx < marked.len() && !marked[idx] {
        marked[idx] = true;
        work.push(idx);
    }
}

/// The heap index of every handle, in order.
fn handle_indices(handles: &[HandleId]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < handles.len() {
        out.push(handles[i].index());
        i += 1;
    }
    out
}

/// The [`HeapRestoreError`] for a failed [`heap_rollback::rebase_handles`].
fn rebase_error(error: RebaseError) -> HeapRestoreError {
    match error {
        RebaseError::Dangling(index) => HeapRestoreError::DanglingReference(HandleId(index as u32)),
        RebaseError::ArenaFull => HeapRestoreError::Limit(HeapLimitError {
            kind: HeapLimitKind::Objects,
            limit: MAX_ARENA_SLOTS,
            attempted: 1,
        }),
        RebaseError::Overflow => HeapRestoreError::ArithmeticOverflow,
    }
}

/// What [`restore_graph`] rebuilds.
#[derive(Debug)]
pub struct RestoredHeap {
    /// The bar-start heap with the retained graph overlaid.
    pub heap: Heap,
    /// Every handle reachable from the roots in the live heap, increasing.
    pub reachable: Vec<usize>,
    /// The new handle of `reachable[i]` is `targets[i]`.
    pub targets: Vec<usize>,
    /// `remap[h] = Some(new)` for every reachable `h` (dense over the live
    /// arena; see [`heap_rollback::remap_table`]).
    pub remap: Vec<Option<usize>>,
}

/// The running totals of [`restore_graph`]'s overlay loop: live elements
/// and objects of the rebuilt heap so far, and the next index appended after
/// the snapshot frontier.
#[derive(Debug, Clone, Copy)]
struct RestoreCounts {
    live_elements: usize,
    live_objects: usize,
    next_appended_index: usize,
}

/// One step of [`restore_graph`]'s overlay loop: copy the live object at
/// `handle` in `current`, rewrite its references through `remap`, and charge
/// it at `target` in `restored` (the snapshot clone; a target below the
/// snapshot frontier replaces the object there, one at or past it is
/// appended).
fn copy_retained(
    current: &Heap,
    restored: &Heap,
    remap: &[Option<usize>],
    handle: usize,
    target: usize,
    counts: RestoreCounts,
) -> Result<(HeapObj, RestoreCounts), HeapRestoreError> {
    let handle = HandleId(handle as u32);
    let mut object = match current.get(handle) {
        Some(object) => object.clone(),
        None => return Err(HeapRestoreError::DanglingReference(handle)),
    };
    if let Some(dangling) = object.remap_refs(remap) {
        return Err(HeapRestoreError::DanglingReference(dangling));
    }
    let mut next = counts;
    // `restored` is the snapshot clone, so its arena length is the frontier.
    if target < restored.objects.len() {
        let previous = match &restored.objects[target] {
            Some(previous) => previous.element_count(),
            None => return Err(HeapRestoreError::DanglingReference(HandleId(target as u32))),
        };
        next.live_elements = match counts.live_elements.checked_sub(previous) {
            Some(count) => match count.checked_add(object.element_count()) {
                Some(count) => count,
                None => return Err(HeapRestoreError::ArithmeticOverflow),
            },
            None => return Err(HeapRestoreError::ArithmeticOverflow),
        };
    } else {
        if target != counts.next_appended_index {
            return Err(HeapRestoreError::ArithmeticOverflow);
        }
        next.next_appended_index = match counts.next_appended_index.checked_add(1) {
            Some(index) => index,
            None => return Err(HeapRestoreError::ArithmeticOverflow),
        };
        next.live_elements = match counts.live_elements.checked_add(object.element_count()) {
            Some(count) => count,
            None => return Err(HeapRestoreError::ArithmeticOverflow),
        };
        next.live_objects = match counts.live_objects.checked_add(1) {
            Some(count) => count,
            None => return Err(HeapRestoreError::ArithmeticOverflow),
        };
    }
    Ok((object, next))
}

/// [`copy_retained`] for every `(reachable[i], targets[i])` in order: the
/// overlay targets, the copied objects, and the final counts.
#[allow(clippy::type_complexity)]
fn copy_all_retained(
    current: &Heap,
    restored: &Heap,
    remap: &[Option<usize>],
    reachable: &[usize],
    targets: &[usize],
    counts: RestoreCounts,
) -> Result<(Vec<usize>, Vec<Option<HeapObj>>, RestoreCounts), HeapRestoreError> {
    let mut counts = counts;
    let mut overlay_targets = Vec::new();
    let mut overlay_objects = Vec::new();
    let mut i = 0;
    while i < reachable.len() {
        let target = targets[i];
        match copy_retained(current, restored, remap, reachable[i], target, counts) {
            Ok((object, next)) => {
                counts = next;
                overlay_targets.push(target);
                overlay_objects.push(Some(object));
            }
            Err(error) => return Err(error),
        }
        i += 1;
    }
    Ok((overlay_targets, overlay_objects, counts))
}

/// The whole of [`Heap::restore_preserving_roots`] over heap indices: rebuild
/// `snapshot` while keeping the object graph reachable from `roots` in
/// `current` (see that method for the contract). Handle identity comes from
/// the [`heap_rollback`] kernels; this function copies the reachable
/// objects, rewrites the references inside them, charges the element and
/// object budgets, and installs the copies.
#[allow(clippy::question_mark)]
pub fn restore_graph(
    snapshot: &Heap,
    current: &Heap,
    roots: &[usize],
) -> Result<RestoredHeap, HeapRestoreError> {
    if snapshot.max_elements != current.max_elements || snapshot.max_objects != current.max_objects
    {
        return Err(HeapRestoreError::MismatchedLimits);
    }
    let reachable = match heap_rollback::reachable_handles(current, roots) {
        Ok(reachable) => reachable,
        Err(index) => return Err(HeapRestoreError::DanglingReference(HandleId(index as u32))),
    };
    let targets = match heap_rollback::rebase_handles(snapshot, &reachable) {
        Ok(targets) => targets,
        Err(error) => return Err(rebase_error(error)),
    };
    let remap = heap_rollback::remap_table(current.objects.len(), &reachable, &targets);

    let snapshot_frontier = snapshot.objects.len();
    let mut restored = snapshot.clone();
    let counts = RestoreCounts {
        live_elements: snapshot.live_elements,
        live_objects: snapshot.len(),
        next_appended_index: snapshot_frontier,
    };
    let (overlay_targets, mut overlay_objects, counts) =
        match copy_all_retained(current, &restored, &remap, &reachable, &targets, counts) {
            Ok(overlays) => overlays,
            Err(error) => return Err(error),
        };
    let live_elements = counts.live_elements;
    let live_objects = counts.live_objects;

    if live_elements > snapshot.max_elements {
        return Err(HeapRestoreError::Limit(HeapLimitError {
            kind: HeapLimitKind::Elements,
            limit: snapshot.max_elements,
            attempted: live_elements.saturating_sub(snapshot.live_elements),
        }));
    }
    if live_objects > snapshot.max_objects {
        return Err(HeapRestoreError::Limit(HeapLimitError {
            kind: HeapLimitKind::Objects,
            limit: snapshot.max_objects,
            attempted: live_objects.saturating_sub(snapshot.len()),
        }));
    }

    heap_rollback::install_overlays(
        &mut restored.objects,
        &overlay_targets,
        &mut overlay_objects,
    );
    restored.live_elements = live_elements;
    restored.collected = snapshot.collected;
    Ok(RestoredHeap {
        heap: restored,
        reachable,
        targets,
        remap,
    })
}

/// Every `varip` cell that holds a reference, as `(slot, cell, handle)`, in
/// slot then cell order: the roots of a realtime rollback. `varip[slot]` says
/// whether `slots[slot]` (a slot's history ring) belongs to a `varip`
/// variable.
pub fn varip_root_cells(varip: &[bool], slots: &[Vec<Value>]) -> Vec<(usize, usize, HandleId)> {
    let mut out = Vec::new();
    let mut slot = 0;
    while slot < varip.len() {
        if varip[slot] {
            let cells = &slots[slot];
            let mut cell = 0;
            while cell < cells.len() {
                if let Some(handle) = value::ref_handle(&cells[cell]) {
                    out.push((slot, cell, handle));
                }
                cell += 1;
            }
        }
        slot += 1;
    }
    out
}

/// The heap half of the VM's realtime rollback: rebuild `snapshot` while
/// keeping the graph reachable from the `varip` cells of `slots` in
/// `current` ([`restore_graph`]), and compute the new handle of every such
/// cell ([`varip_root_cells`] order). The caller installs the heap and writes
/// the cells back with [`rewrite_varip_cells`].
#[allow(clippy::type_complexity, clippy::question_mark)]
pub fn restore_varip_heap(
    snapshot: &Heap,
    current: &Heap,
    varip: &[bool],
    slots: &[Vec<Value>],
) -> Result<(Heap, Vec<(usize, usize, HandleId)>), HeapRestoreError> {
    let root_cells = varip_root_cells(varip, slots);
    let mut roots = Vec::new();
    let mut i = 0;
    while i < root_cells.len() {
        roots.push(root_cells[i].2.index());
        i += 1;
    }
    let restored = match restore_graph(snapshot, current, &roots) {
        Ok(restored) => restored,
        Err(error) => return Err(error),
    };
    let mut remapped = Vec::new();
    let mut j = 0;
    while j < root_cells.len() {
        let (slot, cell, old) = root_cells[j];
        match heap_rollback::remapped_index(&restored.remap, old.index()) {
            Some(new) => remapped.push((slot, cell, HandleId(new as u32))),
            None => return Err(HeapRestoreError::DanglingReference(old)),
        }
        j += 1;
    }
    Ok((restored.heap, remapped))
}

/// Point each `(slot, cell, handle)` cell at `Value::Ref(handle)`.
pub fn rewrite_varip_cells(slots: &mut [Vec<Value>], cells: &[(usize, usize, HandleId)]) {
    let mut i = 0;
    while i < cells.len() {
        let (slot, cell, handle) = cells[i];
        value::set_ref(&mut slots[slot][cell], handle);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_identity_clones_with_heap_objects_but_is_not_a_wire_resume_format() {
        let identity = std::num::NonZeroU64::new(31).unwrap();
        let object = HeapObj::Object {
            type_id: 0,
            fields: vec![Value::Na],
            table_identity: Some(identity),
        };
        assert_eq!(object.clone(), object);
        let wire = serde_json::to_value(&object).unwrap();
        assert!(wire["Object"].get("table_identity").is_none());
        let decoded: HeapObj = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(
            decoded,
            HeapObj::Object {
                table_identity: None,
                ..
            }
        ));
        let mut supplied = wire;
        supplied["Object"]["table_identity"] = serde_json::json!(31);
        let decoded: HeapObj = serde_json::from_value(supplied).unwrap();
        assert!(matches!(
            decoded,
            HeapObj::Object {
                table_identity: None,
                ..
            }
        ));
    }

    #[test]
    fn logical_allocation_clears_identity_without_changing_snapshot_clones() {
        let mut heap = Heap::new();
        let identity = std::num::NonZeroU64::new(41).unwrap();
        let original = heap
            .alloc(HeapObj::Object {
                type_id: 0,
                fields: vec![],
                table_identity: Some(identity),
            })
            .unwrap();
        let Some(HeapObj::Object { table_identity, .. }) = heap.get_mut(original) else {
            panic!("new object")
        };
        assert_eq!(*table_identity, None);
        *table_identity = Some(identity);
        let snapshot = heap.clone();
        let copied = heap.alloc(heap.get(original).unwrap().clone()).unwrap();
        assert!(matches!(
            heap.get(copied),
            Some(HeapObj::Object {
                table_identity: None,
                ..
            })
        ));
        assert!(
            matches!(snapshot.get(original), Some(HeapObj::Object { table_identity: Some(actual), .. }) if *actual == identity)
        );
        assert!(
            matches!(heap.get(original), Some(HeapObj::Object { table_identity: Some(actual), .. }) if *actual == identity)
        );
    }

    #[test]
    fn retired_tokens_survive_varip_graph_rebasing_without_becoming_heap_roots() {
        let snapshot = Heap::new();
        let mut current = snapshot.clone();
        let identity = std::num::NonZeroU64::new(17).unwrap();
        let dead = current
            .alloc(HeapObj::Object {
                type_id: 0,
                fields: vec![],
                table_identity: Some(identity),
            })
            .unwrap();
        if let Some(HeapObj::Object { table_identity, .. }) = current.get_mut(dead) {
            *table_identity = Some(identity);
        }
        let retained = current
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: vec![Value::DisplacedTable(identity)],
            })
            .unwrap();
        let (restored, remap) =
            Heap::restore_preserving_roots(&snapshot, &current, &[retained]).unwrap();
        assert_eq!(restored.len(), 1, "the dead table is not a retained root");
        let new = remap[&retained];
        assert_eq!(
            new, dead,
            "a rebased collection can occupy the dead table's old index"
        );
        let Some(HeapObj::Array { items, .. }) = restored.get(new) else {
            panic!("retained array")
        };
        assert_eq!(items, &vec![Value::DisplacedTable(identity)]);
        assert_eq!(items[0].as_ref(), None);
    }

    #[test]
    fn active_table_identity_survives_retained_graph_rebasing() {
        let snapshot = Heap::new();
        let mut current = snapshot.clone();
        current
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Int,
                items: vec![],
            })
            .unwrap();
        let identity = std::num::NonZeroU64::new(23).unwrap();
        let live = current
            .alloc(HeapObj::Object {
                type_id: 0,
                fields: vec![],
                table_identity: Some(identity),
            })
            .unwrap();
        if let Some(HeapObj::Object { table_identity, .. }) = current.get_mut(live) {
            *table_identity = Some(identity);
        }
        let (restored, remap) =
            Heap::restore_preserving_roots(&snapshot, &current, &[live]).unwrap();
        assert_ne!(remap[&live], live);
        assert!(matches!(
            restored.get(remap[&live]),
            Some(HeapObj::Object { table_identity: Some(actual), .. }) if *actual == identity
        ));
    }

    #[test]
    fn displaced_table_alias_plan_preserves_nonreferences_and_allows_retired_collection() {
        let mut heap = Heap::with_limit(100);
        let table = heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![],
            })
            .unwrap();
        let alias = heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: vec![Value::Ref(table), Value::Na],
            })
            .unwrap();
        let plan = heap.plan_refs_to_na(table, &[alias], 1_000).unwrap();
        assert_eq!(
            heap.apply_refs_to_displaced_table(table, &plan, std::num::NonZeroU64::new(7).unwrap()),
            1
        );
        let Some(HeapObj::Array { items, .. }) = heap.get(alias) else {
            panic!("alias array")
        };
        assert!(matches!(items[0], Value::DisplacedTable(_)));
        assert!(matches!(items[1], Value::Na));
        assert_eq!(heap.collect(&[alias]), 1);
        assert!(heap.get(table).is_none());
        assert!(heap.get(alias).is_some());
    }

    /// Small deterministic generator for the cross-check tests.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, percent: usize) -> bool {
            self.below(100) < percent
        }
    }

    fn random_value(g: &mut Gen, span: usize) -> Value {
        match g.below(7) {
            0 => Value::Na,
            1 => Value::Bool(g.chance(50)),
            2 => Value::Int(g.next() as i64),
            3 => Value::Float(g.below(1000) as f64),
            4 => Value::String(format!("s{}", g.below(10))),
            5 => Value::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 4.0,
                input_origin: None,
            },
            // Handles may point past the arena (a stale or dangling handle).
            _ => Value::Ref(HandleId(g.below(span + 3) as u32)),
        }
    }

    fn random_values(g: &mut Gen, span: usize) -> Vec<Value> {
        let n = g.below(5);
        (0..n).map(|_| random_value(g, span)).collect()
    }

    fn random_object(g: &mut Gen, span: usize) -> HeapObj {
        match g.below(4) {
            0 => HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: random_values(g, span),
            },
            1 => {
                let items = random_values(g, span);
                HeapObj::Matrix {
                    elem_ty: ElemType::Float,
                    rows: 1,
                    cols: items.len(),
                    items,
                }
            }
            2 => {
                let n = g.below(4);
                HeapObj::Map {
                    key_ty: ElemType::Object(0),
                    val_ty: ElemType::Object(0),
                    entries: (0..n)
                        .map(|_| (random_value(g, span), random_value(g, span)))
                        .collect(),
                }
            }
            _ => HeapObj::Object {
                table_identity: None,
                type_id: g.below(3) as u32,
                fields: random_values(g, span),
            },
        }
    }

    fn random_heap(g: &mut Gen) -> Heap {
        let span = g.below(12);
        let mut heap = Heap::with_limits(1_000, 1_000);
        for _ in 0..span {
            heap.alloc(random_object(g, span)).unwrap();
        }
        // Some slots already collected (a second, disjoint collection).
        if g.chance(40) {
            let roots: Vec<HandleId> = (0..g.below(span + 1))
                .map(|_| HandleId(g.below(span + 1) as u32))
                .collect();
            heap.collect_reference(&roots);
        }
        heap
    }

    fn visited_refs(obj: &HeapObj) -> Vec<usize> {
        let mut out = Vec::new();
        obj.visit_values(&mut |v| {
            if let Value::Ref(h) = v {
                out.push(h.index());
            }
        });
        out
    }

    #[test]
    fn collect_matches_reference_on_random_heaps() {
        let mut g = Gen(0x9e37_79b9_7f4a_7c15);
        let (mut freed_total, mut kept_total, mut cases) = (0, 0, 0);
        for _ in 0..10_000 {
            let heap = random_heap(&mut g);
            let span = heap.arena_len();
            let roots: Vec<HandleId> = (0..g.below(4))
                .map(|_| HandleId(g.below(span + 2) as u32))
                .collect();
            let mut new = heap.clone();
            let mut old = heap.clone();
            let freed_new = new.collect(&roots);
            let freed_old = old.collect_reference(&roots);
            assert_eq!(freed_new, freed_old, "freed count, roots {roots:?}");
            assert_eq!(new, old, "heap after collect, roots {roots:?}");
            for slot in heap.objects.iter().flatten() {
                assert_eq!(slot.ref_indices(), visited_refs(slot));
            }
            freed_total += freed_new;
            kept_total += new.len();
            cases += 1;
        }
        assert_eq!(cases, 10_000);
        assert!(freed_total > 1_000 && kept_total > 1_000);
    }

    #[test]
    fn from_tag_matches_reference_except_on_wrapping_object_tags() {
        let base = ElemType::OBJECT_TAG_BASE;
        let wrap = base + (1_i64 << 32);
        let mut tags = vec![
            i64::MIN,
            -1,
            0,
            1,
            2,
            3,
            4,
            5,
            999,
            base,
            base + 1,
            base + u32::MAX as i64,
            wrap,
            wrap + 7,
            i64::MAX,
        ];
        let mut g = Gen(0x2545_f491_4f6c_dd1d);
        for _ in 0..10_000 {
            tags.push(g.next() as i64);
            tags.push(base + (g.next() % (1 << 33)) as i64);
        }
        for tag in tags {
            let new = ElemType::from_tag(tag);
            let old = ElemType::from_tag_reference(tag);
            if tag >= base && tag - base > u32::MAX as i64 {
                // Object tags past the u32 id range decode to None, not to a
                // valid object id.
                assert_eq!(new, None, "tag {tag}");
                assert!(old.is_some());
            } else {
                assert_eq!(new, old, "tag {tag}");
            }
            if let Some(ty) = new {
                assert_eq!(ty.to_tag(), tag, "from_tag is the inverse of to_tag");
            }
        }
        // The unchecked decoder aliases this tag onto object 0.
        assert_eq!(
            ElemType::from_tag_reference(wrap),
            Some(ElemType::Object(0))
        );
        assert_eq!(ElemType::from_tag(wrap), None);
        assert_eq!(ElemType::from_tag(base), Some(ElemType::Object(0)));
        for ty in [
            ElemType::Float,
            ElemType::Int,
            ElemType::Bool,
            ElemType::String,
            ElemType::Color,
            ElemType::Object(0),
            ElemType::Object(u32::MAX),
        ] {
            assert_eq!(ElemType::from_tag(ty.to_tag()), Some(ty));
        }
    }

    fn float_array(items: Vec<Value>) -> HeapObj {
        HeapObj::Array {
            elem_ty: ElemType::Float,
            items,
        }
    }

    fn ref_array(items: Vec<HandleId>) -> HeapObj {
        HeapObj::Array {
            elem_ty: ElemType::Object(0),
            items: items.into_iter().map(Value::Ref).collect(),
        }
    }

    #[test]
    fn next_handle_id_matches_reference() {
        let max = u32::MAX as usize;
        for len in [
            0,
            1,
            2,
            1000,
            max - 2,
            max - 1,
            max,
            max.saturating_add(1),
            usize::MAX,
        ] {
            assert_eq!(
                next_handle_id(len),
                next_handle_id_reference(len),
                "len {len}"
            );
        }
    }

    /// A small deterministic generator (xorshift64*), so the differential
    /// test needs no extra dependency.
    struct RollbackGen(u64);

    impl RollbackGen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, percent: usize) -> bool {
            self.below(100) < percent
        }
    }

    /// A random object whose references point anywhere in `0..span`
    /// (possibly past the arena: dangling).
    fn random_rollback_object(g: &mut RollbackGen, span: usize) -> HeapObj {
        let value = |g: &mut RollbackGen| {
            if g.chance(50) {
                Value::Ref(HandleId(g.below(span) as u32))
            } else {
                Value::Int(g.below(1000) as i64)
            }
        };
        let len = g.below(4);
        match g.below(3) {
            0 => HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: (0..len).map(|_| value(g)).collect(),
            },
            1 => HeapObj::Map {
                key_ty: ElemType::Object(0),
                val_ty: ElemType::Object(0),
                entries: (0..len).map(|_| (value(g), value(g))).collect(),
            },
            _ => HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: (0..len).map(|_| value(g)).collect(),
            },
        }
    }

    /// A random value of any kind; references point anywhere in `0..span`.
    fn random_value_any(g: &mut Gen, span: usize) -> Value {
        match g.below(8) {
            0..=2 => Value::Ref(HandleId(g.below(span) as u32)),
            3 => Value::Int(g.below(1000) as i64),
            4 => Value::Float(g.below(1000) as f64 / 8.0),
            5 => Value::String(format!("s{}", g.below(10))),
            6 => Value::Color {
                r: g.below(256) as u8,
                g: 7,
                b: 9,
                t: g.below(101) as f64,
                input_origin: None,
            },
            _ => Value::Na,
        }
    }

    /// A random object of any of the four shapes.
    fn random_object_any(g: &mut Gen, span: usize) -> HeapObj {
        let len = g.below(5);
        let values = |g: &mut Gen, n: usize| -> Vec<Value> {
            (0..n).map(|_| random_value_any(g, span)).collect()
        };
        match g.below(4) {
            0 => HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: values(g, len),
            },
            1 => {
                let (rows, cols) = (g.below(3), g.below(3));
                HeapObj::Matrix {
                    elem_ty: ElemType::Float,
                    rows,
                    cols,
                    items: values(g, rows * cols),
                }
            }
            2 => HeapObj::Map {
                key_ty: ElemType::String,
                val_ty: ElemType::Object(1),
                entries: (0..len)
                    .map(|_| (random_value_any(g, span), random_value_any(g, span)))
                    .collect(),
            },
            _ => HeapObj::Object {
                table_identity: None,
                type_id: g.below(3) as u32,
                fields: values(g, len),
            },
        }
    }

    /// A random `(snapshot, current)` pair of the same lineage (or, rarely,
    /// with different limits), as a realtime tick produces them.
    fn random_heap_pair(g: &mut Gen) -> (Heap, Heap, usize) {
        let (max_elements, max_objects) = if g.chance(50) {
            (1000, 100)
        } else {
            (4 + g.below(12), 3 + g.below(6))
        };
        let mut snapshot = Heap::with_limits(max_elements, max_objects);
        let n0 = g.below(7);
        let span = n0 + 6;
        for _ in 0..n0 {
            let _ = snapshot.alloc(random_object_any(g, span));
        }
        if g.chance(30) {
            let roots: Vec<HandleId> = (0..g.below(3))
                .map(|_| HandleId(g.below(span) as u32))
                .collect();
            snapshot.collect(&roots);
        }
        let mut current = if g.chance(3) {
            Heap::with_limits(max_elements, max_objects + 1)
        } else {
            snapshot.clone()
        };
        for index in 0..current.arena_len() {
            if g.chance(30) {
                let replacement = random_object_any(g, span);
                if let Some(object) = current.get_mut(HandleId(index as u32)) {
                    *object = replacement;
                }
            }
        }
        if g.chance(60) {
            let roots: Vec<HandleId> = (0..g.below(3))
                .map(|_| HandleId(g.below(span) as u32))
                .collect();
            current.collect(&roots);
        }
        for _ in 0..g.below(7) {
            let _ = current.alloc(random_object_any(g, span));
        }
        (snapshot, current, span)
    }

    /// Test-only copy of the `children` method of `impl RefGraph for Heap`.
    fn children_reference(heap: &Heap, index: usize) -> Option<Vec<usize>> {
        let object = heap.objects.get(index)?.as_ref()?;
        let mut children = Vec::new();
        object.visit_values(&mut |value| {
            if let Value::Ref(child) = value {
                children.push(child.index());
            }
        });
        Some(children)
    }

    /// Test-only copy of the `Vm::restore_varip_heap` body over a `varip` mask
    /// and the slot rings, using the reference heap restore.
    #[allow(clippy::type_complexity)]
    fn restore_varip_heap_reference(
        snapshot: &Heap,
        current: &Heap,
        varip: &[bool],
        slots: &[Vec<Value>],
    ) -> Result<(Heap, Vec<(usize, usize, HandleId)>), HeapRestoreError> {
        let mut varip_root_cells = Vec::new();
        let mut varip_roots = Vec::new();
        for (slot, is_varip) in varip.iter().enumerate() {
            if !is_varip {
                continue;
            }
            for (cell, value) in slots[slot].iter().enumerate() {
                if let Value::Ref(handle) = value {
                    varip_root_cells.push((slot, cell, *handle));
                    varip_roots.push(*handle);
                }
            }
        }
        let (restored_heap, handle_remap) =
            Heap::restore_preserving_roots_reference(snapshot, current, &varip_roots)?;
        let remapped_roots = varip_root_cells
            .into_iter()
            .map(|(slot, cell, old)| {
                handle_remap
                    .get(&old)
                    .copied()
                    .map(|new| (slot, cell, new))
                    .ok_or(HeapRestoreError::DanglingReference(old))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((restored_heap, remapped_roots))
    }

    /// Checks the heap-glue functions against their test-only copies, covering
    /// `get`, `impl RefGraph for Heap`, `collect`, the whole
    /// `restore_preserving_roots` (all value kinds and all four shapes), the
    /// `varip` root collection and remap (`restore_varip_heap`), and the cell
    /// rewrite (`rewrite_varip_cells`).
    #[test]
    fn heap_glue_matches_reference_on_random_heaps() {
        let mut g = Gen(0xD1B5_4A32_D192_ED03);
        let (mut oks, mut dangling, mut limits, mut mismatched, mut rewritten) = (0, 0, 0, 0, 0);
        for _ in 0..10_000 {
            let (snapshot, current, span) = random_heap_pair(&mut g);

            for index in 0..span + 2 {
                let handle = HandleId(index as u32);
                let old_get = current.objects.get(index).and_then(|slot| slot.as_ref());
                assert_eq!(current.get(handle), old_get);
                assert_eq!(current.children(index), children_reference(&current, index));
                assert_eq!(
                    current.is_live(index),
                    matches!(current.objects.get(index), Some(Some(_)))
                );
            }

            let collect_roots: Vec<HandleId> = (0..g.below(4))
                .map(|_| HandleId(g.below(span + 2) as u32))
                .collect();
            let (mut new_heap, mut old_heap) = (current.clone(), current.clone());
            assert_eq!(
                new_heap.collect(&collect_roots),
                old_heap.collect_reference(&collect_roots)
            );
            assert_eq!(new_heap, old_heap);

            let roots: Vec<HandleId> = (0..g.below(5))
                .map(|_| HandleId(g.below(span) as u32))
                .collect();
            assert_eq!(
                Heap::restore_preserving_roots(&snapshot, &current, &roots),
                Heap::restore_preserving_roots_reference(&snapshot, &current, &roots)
            );

            let n_slots = g.below(4);
            let varip: Vec<bool> = (0..n_slots).map(|_| g.chance(60)).collect();
            let slots: Vec<Vec<Value>> = (0..n_slots)
                .map(|_| {
                    let cells = 1 + g.below(3);
                    let live: Vec<usize> = (0..current.arena_len())
                        .filter(|index| current.objects[*index].is_some())
                        .collect();
                    (0..cells)
                        .map(|_| {
                            if !live.is_empty() && g.chance(60) {
                                Value::Ref(HandleId(live[g.below(live.len())] as u32))
                            } else {
                                random_value_any(&mut g, span)
                            }
                        })
                        .collect()
                })
                .collect();
            let new = restore_varip_heap(&snapshot, &current, &varip, &slots);
            let old = restore_varip_heap_reference(&snapshot, &current, &varip, &slots);
            assert_eq!(new, old, "varip {varip:?}\nslots {slots:?}");
            match &new {
                Ok((_, cells)) => {
                    oks += 1;
                    let mut new_slots = slots.clone();
                    let mut old_slots = slots.clone();
                    rewrite_varip_cells(&mut new_slots, cells);
                    for (slot, cell, handle) in cells {
                        old_slots[*slot][*cell] = Value::Ref(*handle);
                    }
                    assert_eq!(new_slots, old_slots);
                    rewritten += cells.len();
                }
                Err(HeapRestoreError::DanglingReference(_)) => dangling += 1,
                Err(HeapRestoreError::Limit(_)) => limits += 1,
                Err(HeapRestoreError::MismatchedLimits) => mismatched += 1,
                Err(_) => {}
            }
        }
        eprintln!(
            "heap glue coverage: ok {oks}, dangling {dangling}, limit {limits}, mismatched {mismatched}, cells {rewritten}"
        );
        assert!(
            oks > 1000 && dangling > 500 && limits > 100 && mismatched > 100 && rewritten > 1000,
            "coverage: ok {oks}, dangling {dangling}, limit {limits}, mismatched {mismatched}, cells {rewritten}"
        );
    }

    #[test]
    fn restore_preserving_roots_matches_reference_on_random_heaps() {
        let mut g = RollbackGen(0x9E37_79B9_7F4A_7C15);
        let (mut oks, mut dangling, mut limits, mut mismatched) = (0, 0, 0, 0);
        for _ in 0..10_000 {
            let (max_elements, max_objects) = if g.chance(50) {
                (1000, 100)
            } else {
                (4 + g.below(10), 3 + g.below(6))
            };
            let mut snapshot = Heap::with_limits(max_elements, max_objects);
            let n0 = g.below(6);
            let span = n0 + 6;
            for _ in 0..n0 {
                let _ = snapshot.alloc(random_rollback_object(&mut g, span));
            }
            if g.chance(30) {
                let roots: Vec<HandleId> = (0..g.below(3))
                    .map(|_| HandleId(g.below(span) as u32))
                    .filter(|h| snapshot.get(*h).is_some())
                    .collect();
                snapshot.collect(&roots);
            }
            let mut current = if g.chance(4) {
                mismatched += 1;
                Heap::with_limits(max_elements + 1, max_objects)
            } else {
                snapshot.clone()
            };
            // A tick: mutate some objects, collect, allocate more.
            for index in 0..current.arena_len() {
                if g.chance(30) {
                    let replacement = random_rollback_object(&mut g, span);
                    if let Some(object) = current.get_mut(HandleId(index as u32)) {
                        *object = replacement;
                    }
                }
            }
            if g.chance(60) {
                let roots: Vec<HandleId> = (0..g.below(3))
                    .map(|_| HandleId(g.below(span) as u32))
                    .filter(|h| current.get(*h).is_some())
                    .collect();
                current.collect(&roots);
            }
            for _ in 0..g.below(6) {
                let _ = current.alloc(random_rollback_object(&mut g, span));
            }
            let live: Vec<HandleId> = (0..current.arena_len())
                .map(|index| HandleId(index as u32))
                .filter(|h| current.get(*h).is_some())
                .collect();
            let roots: Vec<HandleId> = (0..g.below(5))
                .map(|_| {
                    if !live.is_empty() && g.chance(80) {
                        live[g.below(live.len())]
                    } else {
                        HandleId(g.below(span) as u32)
                    }
                })
                .collect();

            let new = Heap::restore_preserving_roots(&snapshot, &current, &roots);
            let old = Heap::restore_preserving_roots_reference(&snapshot, &current, &roots);
            assert_eq!(
                new, old,
                "roots {roots:?}\nsnapshot {snapshot:?}\ncurrent {current:?}"
            );
            match new {
                Ok(_) => oks += 1,
                Err(HeapRestoreError::DanglingReference(_)) => dangling += 1,
                Err(HeapRestoreError::Limit(_)) => limits += 1,
                Err(_) => {}
            }
        }
        assert!(
            oks > 1000 && dangling > 1000 && limits > 50 && mismatched > 100,
            "coverage: ok {oks}, dangling {dangling}, limit {limits}, mismatched {mismatched}"
        );
    }

    #[test]
    fn rollback_kernels_on_the_reference_example() {
        // The graph of `restore_preserves_reachable_varip_graph_and_rebases_new_handles`
        // seen through `RefGraph`: slot 0 predates the snapshot.
        let mut snapshot = Heap::new();
        snapshot
            .alloc(float_array(vec![Value::Float(1.0)]))
            .unwrap();
        let mut current = snapshot.clone();
        current.alloc(float_array(vec![Value::Int(777)])).unwrap();
        let child = current.alloc(ref_array(vec![])).unwrap();
        let parent = current
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Ref(child)],
            })
            .unwrap();
        let HeapObj::Array { items, .. } = current.get_mut(child).unwrap() else {
            panic!("child array")
        };
        items.push(Value::Ref(parent));
        assert_eq!(
            heap_rollback::reachable_handles(&current, &[0, parent.index()]),
            Ok(vec![0, 2, 3])
        );
        assert_eq!(heap_rollback::reachable_handles(&current, &[9]), Err(9));
        assert_eq!(
            heap_rollback::rebase_handles(&snapshot, &[0, 2, 3]),
            Ok(vec![0, 1, 2])
        );
        let mut objects = vec![Some('a')];
        heap_rollback::install_overlays(
            &mut objects,
            &[0, 1, 2],
            &mut [Some('x'), Some('y'), Some('z')],
        );
        assert_eq!(objects, vec![Some('x'), Some('y'), Some('z')]);
        assert_eq!(heap_rollback::next_handle_index(3), Some(3));
    }

    #[test]
    fn collect_frees_unreachable_and_credits_both_budgets() {
        let mut heap = Heap::new();
        let keep = heap.alloc(float_array(vec![Value::Float(1.0)])).unwrap();
        let drop1 = heap.alloc(float_array(vec![Value::Float(2.0); 4])).unwrap();
        let drop2 = heap.alloc(float_array(vec![Value::Float(3.0); 5])).unwrap();
        assert_eq!(heap.live_elements(), 10);
        assert_eq!(heap.len(), 3);

        assert_eq!(heap.collect(&[keep]), 2);

        // Only `keep`'s single element is still charged, and only its slot.
        assert_eq!(heap.live_elements(), 1);
        assert_eq!(heap.len(), 1);
        assert!(heap.get(keep).is_some());
        // A freed slot reads as absent rather than as some other object.
        assert!(heap.get(drop1).is_none());
        assert!(heap.get(drop2).is_none());
    }

    #[test]
    fn collected_handles_are_never_reissued() {
        // A handle names one object for the whole run. A recycled index would
        // let a stale or wrongly collected handle name a different object, so a
        // collected index stays spent even though its budget charge comes back.
        let mut heap = Heap::new();
        let keep = heap.alloc(float_array(vec![])).unwrap();
        let dropped = heap.alloc(float_array(vec![])).unwrap();
        heap.collect(&[keep]);

        let next = heap.alloc(float_array(vec![])).unwrap();
        assert_ne!(next, dropped, "must not hand out a collected index again");
        assert_eq!(next, HandleId(2));
        // A stale handle reads as absent, never as the new occupant.
        assert!(heap.get(dropped).is_none());
        assert_eq!(heap.len(), 2, "live count excludes the collected object");
        assert_eq!(heap.arena_len(), 3, "the index is spent");
    }

    #[test]
    fn restore_preserves_reachable_varip_graph_and_rebases_new_handles() {
        let mut snapshot = Heap::new();
        let baseline = snapshot
            .alloc(float_array(vec![Value::Float(1.0)]))
            .unwrap();
        let mut current = snapshot.clone();
        let HeapObj::Array { items, .. } = current.get_mut(baseline).unwrap() else {
            panic!("baseline array")
        };
        items[0] = Value::Float(2.0);

        // An unrelated tick-local object occupies the next source handle.
        let temporary = current.alloc(float_array(vec![Value::Int(777)])).unwrap();
        let child = current
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(0),
                items: vec![],
            })
            .unwrap();
        let parent = current
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Ref(child)],
            })
            .unwrap();
        let map = current
            .alloc(HeapObj::Map {
                key_ty: ElemType::Object(0),
                val_ty: ElemType::Object(0),
                entries: vec![(Value::Ref(child), Value::Ref(parent))],
            })
            .unwrap();
        // Exercise cycle-safe reachability and nested-handle rewriting.
        let HeapObj::Array { items, .. } = current.get_mut(child).unwrap() else {
            panic!("child array")
        };
        items.push(Value::Ref(parent));

        let (mut restored, remap) =
            Heap::restore_preserving_roots(&snapshot, &current, &[baseline, parent, map]).unwrap();

        assert_eq!(remap[&baseline], baseline);
        assert_eq!(remap[&child], HandleId(1));
        assert_eq!(remap[&parent], HandleId(2));
        assert_eq!(remap[&map], HandleId(3));
        assert_eq!(restored.arena_len(), 4);
        assert_eq!(restored.len(), 4);
        assert_eq!(restored.live_elements(), 4);
        let HeapObj::Array { items, .. } = restored.get(baseline).unwrap() else {
            panic!("baseline array")
        };
        assert_eq!(items, &[Value::Float(2.0)]);
        let HeapObj::Array { items, .. } = restored.get(HandleId(1)).unwrap() else {
            panic!("child array")
        };
        assert_eq!(items, &[Value::Ref(HandleId(2))]);
        let HeapObj::Object { fields, .. } = restored.get(HandleId(2)).unwrap() else {
            panic!("parent object")
        };
        assert_eq!(fields, &[Value::Ref(HandleId(1))]);
        let HeapObj::Map { entries, .. } = restored.get(HandleId(3)).unwrap() else {
            panic!("map")
        };
        assert_eq!(
            entries,
            &[(Value::Ref(HandleId(1)), Value::Ref(HandleId(2)))]
        );
        assert_ne!(
            items,
            &[Value::Int(777)],
            "the dead temporary must not survive under the compacted handle"
        );
        assert_eq!(
            restored.alloc(float_array(vec![])).unwrap(),
            HandleId(4),
            "the next allocation must not alias a varip root"
        );
        assert_eq!(temporary, HandleId(1));
    }

    #[test]
    fn restore_rejects_varip_overlay_that_exceeds_element_budget() {
        let mut snapshot = Heap::with_limits(2, 4);
        let baseline = snapshot
            .alloc(float_array(vec![Value::Int(1), Value::Int(2)]))
            .unwrap();
        let mut current = snapshot.clone();
        let HeapObj::Array { items, .. } = current.get_mut(baseline).unwrap() else {
            panic!("baseline array")
        };
        items.pop();
        current.release(1);
        let held = current.alloc(float_array(vec![Value::Int(9)])).unwrap();
        assert_eq!(current.live_elements(), 2);

        let error = Heap::restore_preserving_roots(&snapshot, &current, &[held]).unwrap_err();
        assert!(matches!(
            error,
            HeapRestoreError::Limit(HeapLimitError {
                kind: HeapLimitKind::Elements,
                limit: 2,
                attempted: 1,
            })
        ));
    }

    #[test]
    fn restore_rejects_varip_overlay_that_exceeds_object_budget() {
        let mut snapshot = Heap::with_limits(10, 1);
        snapshot.alloc(float_array(vec![])).unwrap();
        let mut current = snapshot.clone();
        current.collect(&[]);
        let held = current.alloc(float_array(vec![])).unwrap();

        let error = Heap::restore_preserving_roots(&snapshot, &current, &[held]).unwrap_err();
        assert!(matches!(
            error,
            HeapRestoreError::Limit(HeapLimitError {
                kind: HeapLimitKind::Objects,
                limit: 1,
                attempted: 1,
            })
        ));
    }

    #[test]
    fn restore_rejects_dangling_roots_and_nested_references() {
        let snapshot = Heap::new();
        let current = Heap::new();
        assert_eq!(
            Heap::restore_preserving_roots(&snapshot, &current, &[HandleId(9)]).unwrap_err(),
            HeapRestoreError::DanglingReference(HandleId(9))
        );

        let mut current = Heap::new();
        let parent = current
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Ref(HandleId(9))],
            })
            .unwrap();
        assert_eq!(
            Heap::restore_preserving_roots(&snapshot, &current, &[parent]).unwrap_err(),
            HeapRestoreError::DanglingReference(HandleId(9))
        );
    }

    #[test]
    fn restore_rejects_heaps_with_different_resource_limits() {
        let snapshot = Heap::with_limits(10, 4);
        let current = Heap::with_limits(11, 4);
        assert_eq!(
            Heap::restore_preserving_roots(&snapshot, &current, &[]).unwrap_err(),
            HeapRestoreError::MismatchedLimits
        );
    }

    #[test]
    fn collect_marks_transitively_through_arrays_objects_and_map_keys() {
        let mut heap = Heap::new();
        let leaf = heap.alloc(float_array(vec![Value::Float(1.0)])).unwrap();
        let via_array = heap.alloc(ref_array(vec![leaf])).unwrap();
        let via_field = heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Ref(via_array)],
            })
            .unwrap();
        // A map reached only through its *key*, which is a handle.
        let key_only = heap.alloc(float_array(vec![])).unwrap();
        let map = heap
            .alloc(HeapObj::Map {
                key_ty: ElemType::Object(0),
                val_ty: ElemType::Float,
                entries: vec![(Value::Ref(key_only), Value::Float(0.0))],
            })
            .unwrap();
        let unreachable = heap.alloc(float_array(vec![Value::Float(9.0)])).unwrap();

        assert_eq!(heap.collect(&[via_field, map]), 1);

        for live in [leaf, via_array, via_field, key_only, map] {
            assert!(heap.get(live).is_some(), "{live:?} was reachable");
        }
        assert!(heap.get(unreachable).is_none());
    }

    #[test]
    fn collect_terminates_on_a_reference_cycle() {
        // A UDT field pointing back at the array that holds the object is the
        // reason this is mark-sweep and not refcounting.
        let mut heap = Heap::new();
        let arr = heap.alloc(ref_array(vec![])).unwrap();
        let obj = heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Ref(arr)],
            })
            .unwrap();
        let HeapObj::Array { items, .. } = heap.get_mut(arr).unwrap() else {
            unreachable!()
        };
        items.push(Value::Ref(obj));

        assert_eq!(heap.collect(&[arr]), 0, "the cycle is reachable");
        assert_eq!(heap.collect(&[]), 2, "and collectable once it is not");
    }

    #[test]
    fn collected_slots_do_not_count_against_the_object_budget() {
        // The regression in miniature: without reclamation the third alloc
        // fails even though at most one object is ever live.
        let mut heap = Heap::with_limits(100, 2);
        let a = heap.alloc(float_array(vec![])).unwrap();
        heap.alloc(float_array(vec![])).unwrap();
        assert!(heap.alloc(float_array(vec![])).is_err(), "budget is 2");

        heap.collect(&[a]);
        heap.alloc(float_array(vec![])).unwrap();
        assert_eq!(heap.len(), 2);
    }

    #[test]
    fn alloc_returns_dense_handles_from_zero() {
        let mut heap = Heap::new();
        let a = heap.alloc(float_array(vec![])).unwrap();
        let b = heap.alloc(float_array(vec![])).unwrap();
        assert_eq!(a, HandleId(0));
        assert_eq!(b, HandleId(1));
        assert_eq!(heap.len(), 2);
    }

    #[test]
    fn aliasing_two_handles_observe_one_mutation() {
        // Reference semantics: copying a handle aliases the same heap object.
        // A push through one handle is visible through the other.
        let mut heap = Heap::new();
        let h = heap.alloc(float_array(vec![Value::Float(1.0)])).unwrap();
        let alias = h; // a copied handle aliases the same heap object

        heap.reserve(1).unwrap();
        if let Some(HeapObj::Array { items, .. }) = heap.get_mut(h) {
            items.push(Value::Float(2.0));
        }

        // Reading through the alias sees the mutation, and the two handles are
        // equal (same object), never a deep copy.
        let via_alias = heap.get(alias).unwrap();
        assert_eq!(via_alias.element_count(), 2);
        assert_eq!(
            via_alias,
            &float_array(vec![Value::Float(1.0), Value::Float(2.0)])
        );
    }

    #[test]
    fn reserve_enforces_limit_with_structured_error() {
        // Heap-growth bomb guard: reserving past the cap returns a structured
        // error (never a panic), and leaves the live count unchanged.
        let mut heap = Heap::with_limit(3);
        heap.reserve(2).unwrap();
        assert_eq!(heap.live_elements(), 2);

        let err = heap.reserve(2).unwrap_err();
        assert_eq!(
            err,
            HeapLimitError {
                kind: HeapLimitKind::Elements,
                limit: 3,
                attempted: 2
            }
        );
        // The failed reservation did not partially commit.
        assert_eq!(heap.live_elements(), 2);
        // One more still fits (2 + 1 == 3).
        heap.reserve(1).unwrap();
        assert_eq!(heap.live_elements(), 3);
    }

    #[test]
    fn alloc_that_overflows_limit_fails_without_allocating() {
        let mut heap = Heap::with_limit(2);
        let err = heap
            .alloc(float_array(vec![Value::Na, Value::Na, Value::Na]))
            .unwrap_err();
        assert_eq!(err.limit, 2);
        assert_eq!(err.attempted, 3);
        // Nothing was pushed; the next handle is still 0.
        assert!(heap.is_empty());
        assert_eq!(heap.live_elements(), 0);
        let ok = heap.alloc(float_array(vec![Value::Na])).unwrap();
        assert_eq!(ok, HandleId(0));
    }

    #[test]
    fn object_slot_cap_bounds_empty_allocations() {
        // Zero-element objects charge no elements but permanently occupy
        // arena slots (a collected slot is never reused); the object
        // cap bounds them so an empty-allocation loop cannot grow the arena
        // without limit (unbounded all-empty growth).
        let mut heap = Heap::with_limits(100, 3);
        for _ in 0..3 {
            heap.alloc(float_array(Vec::new())).unwrap();
        }
        assert_eq!(heap.len(), 3);
        let err = heap.alloc(float_array(Vec::new())).unwrap_err();
        assert_eq!(err.kind, HeapLimitKind::Objects);
        assert_eq!(err.limit, 3);
        assert!(err.to_string().contains("object limit"), "{err}");
        // The failed allocation did not push a slot.
        assert_eq!(heap.len(), 3);
    }

    #[test]
    fn handle_conversion_rejects_at_and_above_the_arena_cap() {
        assert_eq!(
            next_handle_id(MAX_ARENA_SLOTS - 1),
            Some(HandleId(u32::MAX - 1))
        );
        assert_eq!(next_handle_id(MAX_ARENA_SLOTS), None);
        if let Some(oversized) = MAX_ARENA_SLOTS.checked_add(1) {
            assert_eq!(next_handle_id(oversized), None);
        }
    }

    #[test]
    fn release_returns_budget_and_saturates() {
        let mut heap = Heap::with_limit(5);
        heap.reserve(4).unwrap();
        heap.release(3);
        assert_eq!(heap.live_elements(), 1);
        // Over-release saturates at zero rather than underflowing.
        heap.release(100);
        assert_eq!(heap.live_elements(), 0);
    }

    #[test]
    fn out_of_range_handle_returns_none_not_panic() {
        let mut heap = Heap::new();
        heap.alloc(float_array(vec![])).unwrap();
        assert!(heap.get(HandleId(0)).is_some());
        assert!(heap.get(HandleId(1)).is_none());
        assert!(heap.get_mut(HandleId(9)).is_none());
    }

    #[test]
    fn element_count_covers_all_three_shapes() {
        let arr = float_array(vec![Value::Na, Value::Na]);
        assert_eq!(arr.element_count(), 2);
        assert_eq!(arr.kind_name(), "array");

        let matrix = HeapObj::Matrix {
            elem_ty: ElemType::Int,
            rows: 2,
            cols: 3,
            items: vec![Value::Int(0); 6],
        };
        assert_eq!(matrix.element_count(), 6);
        assert_eq!(matrix.kind_name(), "matrix");

        let map = HeapObj::Map {
            key_ty: ElemType::String,
            val_ty: ElemType::Float,
            entries: vec![
                (Value::String("a".into()), Value::Float(1.0)),
                (Value::String("b".into()), Value::Float(2.0)),
            ],
        };
        assert_eq!(map.element_count(), 2);
        assert_eq!(map.kind_name(), "map");
    }

    #[test]
    fn zero_limit_rejects_every_growth() {
        let mut heap = Heap::with_limit(0);
        // An empty collection still allocates (0 elements charged).
        assert!(heap.alloc(float_array(vec![])).is_ok());
        // But any growth fails.
        assert!(heap.reserve(1).is_err());
    }

    #[test]
    fn elem_type_hir_round_trip() {
        use pinewasm_hir::Type;
        for et in [
            ElemType::Float,
            ElemType::Int,
            ElemType::Bool,
            ElemType::String,
            ElemType::Color,
        ] {
            let ty = et.to_hir_type();
            assert_eq!(ElemType::from_hir_type(&ty), Some(et));
        }
        // Non-scalar HIR types have no element type.
        assert_eq!(ElemType::from_hir_type(&Type::Void), None);
        assert_eq!(
            ElemType::from_hir_type(&Type::Array(Box::new(Type::Float))),
            None
        );
    }

    #[test]
    fn heap_round_trips_through_serde() {
        let mut heap = Heap::with_limit(10);
        heap.alloc(float_array(vec![Value::Float(1.5), Value::Na]))
            .unwrap();
        let json = serde_json::to_string(&heap).unwrap();
        let back: Heap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, heap);
    }
}
