//! Aeneas extraction roots for the heap glue: the whole heap half of
//! realtime rollback with `varip` collections (`--features aeneas` only).
//!
//! Each function is a thin, behavior-free wrapper that makes Charon translate
//! an inherent method or a free function of `crate::heap` together with its
//! dependencies: `restore_graph` (object cloning, reference rewriting,
//! accounting, limit errors, and the `heap_rollback` kernels through
//! `impl RefGraph for Heap`), the VM glue `restore_varip_heap` and
//! `rewrite_varip_cells`, and `Heap::alloc`/`Heap::collect` (the only
//! methods that change the arena). `Value` stays opaque: its `f64` and
//! `String` payloads have no Aeneas translation, so `value::ref_handle` and
//! `value::set_ref` are modeled by hand.

use crate::heap::{self, Heap, HeapLimitError, HeapObj, HeapRestoreError, RestoredHeap};
use crate::value::{HandleId, Value};

/// Root for `heap::restore_graph`.
pub fn root_restore_graph(
    snapshot: &Heap,
    current: &Heap,
    roots: &[usize],
) -> Result<RestoredHeap, HeapRestoreError> {
    heap::restore_graph(snapshot, current, roots)
}

/// Root for `heap::restore_varip_heap`.
#[allow(clippy::type_complexity)]
pub fn root_restore_varip_heap(
    snapshot: &Heap,
    current: &Heap,
    varip: &[bool],
    slots: &[Vec<Value>],
) -> Result<(Heap, Vec<(usize, usize, HandleId)>), HeapRestoreError> {
    heap::restore_varip_heap(snapshot, current, varip, slots)
}

/// Root for `heap::rewrite_varip_cells`.
pub fn root_rewrite_varip_cells(slots: &mut [Vec<Value>], cells: &[(usize, usize, HandleId)]) {
    heap::rewrite_varip_cells(slots, cells);
}

/// Root for `Heap::alloc`.
pub fn root_heap_alloc(arena: &mut Heap, obj: HeapObj) -> Result<HandleId, HeapLimitError> {
    arena.alloc(obj)
}

/// Root for `Heap::collect`.
pub fn root_heap_collect(arena: &mut Heap, roots: &[HandleId]) -> usize {
    arena.collect(roots)
}

/// Root for `Heap::get`.
pub fn root_heap_get(arena: &Heap, handle: HandleId) -> Option<&HeapObj> {
    arena.get(handle)
}
