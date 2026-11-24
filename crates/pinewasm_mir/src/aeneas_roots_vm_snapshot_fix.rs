//! Aeneas extraction roots for the varip rollback handle-identity kernels
//! (`--features aeneas` only).
//!
//! Each function is a thin, behavior-free wrapper whose only job is to make
//! Charon translate the handle-identity kernels of
//! `Heap::restore_preserving_roots` in `crate::heap_rollback`. They are
//! generic over the heap view and the object type because the kernels are
//! (the heap instantiates them at `Heap` and `HeapObj`, which stay out of the
//! extraction).

use crate::heap_rollback::{self, RebaseError, RefGraph};

/// Root for `heap_rollback::reachable_handles`.
pub fn root_reachable_handles<G: RefGraph>(
    graph: &G,
    roots: &[usize],
) -> Result<Vec<usize>, usize> {
    heap_rollback::reachable_handles(graph, roots)
}

/// Root for `heap_rollback::rebase_handles`.
pub fn root_rebase_handles<S: RefGraph>(
    snapshot: &S,
    reachable: &[usize],
) -> Result<Vec<usize>, RebaseError> {
    heap_rollback::rebase_handles(snapshot, reachable)
}

/// Root for `heap_rollback::install_overlays`.
pub fn root_install_overlays<T>(
    objects: &mut Vec<Option<T>>,
    targets: &[usize],
    payload: &mut [Option<T>],
) {
    heap_rollback::install_overlays(objects, targets, payload);
}

/// Root for `heap_rollback::next_handle_index` (what `Heap::alloc` issues).
pub fn root_next_handle_index(arena_len: usize) -> Option<u32> {
    heap_rollback::next_handle_index(arena_len)
}
