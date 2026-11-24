//! Handle-identity kernels of [`Heap::restore_preserving_roots`], the heap
//! half of realtime rollback with `varip` collections.
//!
//! Rollback rebuilds the bar-start heap and keeps the object graph that is
//! still reachable from `varip` cells. The part of that operation that decides
//! handle identity lives here, as free functions over plain indices so that
//! Charon/Aeneas can extract them:
//!
//! * [`reachable_handles`]: the ordered set of handles reachable from the
//!   roots in the live heap (the worklist traversal), or the first dangling
//!   handle the traversal meets;
//! * [`rebase_handles`]: the new handle of each reachable handle. Handles
//!   below the snapshot frontier keep their identity; the others are packed
//!   densely after the frontier, in increasing order;
//! * [`install_overlays`]: write the rewritten objects into the rebuilt arena
//!   at those handles;
//! * [`next_handle_index`]: the handle [`Heap::alloc`] issues next.
//!
//! The heap is seen through [`RefGraph`], which `Heap` implements. Element
//! and object accounting, reference rewriting inside objects and the ordered
//! map the caller returns stay in [`Heap::restore_preserving_roots`].
//!
//! [`Heap::restore_preserving_roots`]: crate::heap::Heap::restore_preserving_roots
//! [`Heap::alloc`]: crate::heap::Heap::alloc

use crate::heap::MAX_ARENA_SLOTS;

/// A heap arena seen as a graph of handle indices.
pub trait RefGraph {
    /// Number of arena slots, live or collected.
    fn slot_count(&self) -> usize;
    /// Whether slot `index` holds a live object.
    fn is_live(&self, index: usize) -> bool;
    /// The handle indices named by the live object in slot `index`, in
    /// `HeapObj::visit_values` order, or `None` for a collected or
    /// out-of-range slot. Implementations return `Some` only for
    /// `index < slot_count()`.
    fn children(&self, index: usize) -> Option<Vec<usize>>;
}

/// Why [`rebase_handles`] failed. The caller maps it to `HeapRestoreError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseError {
    /// A reachable handle below the snapshot frontier is collected in the
    /// snapshot.
    Dangling(usize),
    /// The rebased arena would run past the handle space.
    ArenaFull,
    /// The next rebased index overflowed `usize`.
    Overflow,
}

/// The handle index issued for arena length `arena_len`, if the handle space
/// has room (`next_handle_id`).
pub fn next_handle_index(arena_len: usize) -> Option<u32> {
    if arena_len < MAX_ARENA_SLOTS {
        Some(arena_len as u32)
    } else {
        None
    }
}

/// Every handle index reachable from `roots` in `graph`, in increasing order,
/// or `Err(h)` for the first handle `h` the depth-first worklist pops that is
/// collected or out of range.
pub fn reachable_handles<G: RefGraph>(graph: &G, roots: &[usize]) -> Result<Vec<usize>, usize> {
    let mut visited = vec![false; graph.slot_count()];
    let mut work = roots.to_vec();
    while let Some(handle) = work.pop() {
        if handle < visited.len() && visited[handle] {
            continue;
        }
        match graph.children(handle) {
            None => return Err(handle),
            Some(children) => {
                visited[handle] = true;
                work.extend_from_slice(&children);
            }
        }
    }
    Ok(marked_indices(&visited))
}

/// The indices `i` with `marks[i]`, in increasing order.
fn marked_indices(marks: &[bool]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < marks.len() {
        if marks[i] {
            out.push(i);
        }
        i += 1;
    }
    out
}

/// The new handle index of each reachable handle (same order as
/// `reachable`). A handle below the snapshot frontier (`snapshot`'s slot
/// count) keeps its index and must be live in the snapshot; the others get
/// `frontier, frontier + 1, ...` in order.
pub fn rebase_handles<S: RefGraph>(
    snapshot: &S,
    reachable: &[usize],
) -> Result<Vec<usize>, RebaseError> {
    let frontier = snapshot.slot_count();
    let mut targets = Vec::new();
    let mut next = frontier;
    let mut i = 0;
    while i < reachable.len() {
        let handle = reachable[i];
        if handle < frontier {
            if !snapshot.is_live(handle) {
                return Err(RebaseError::Dangling(handle));
            }
            targets.push(handle);
        } else {
            match next_handle_index(next) {
                None => return Err(RebaseError::ArenaFull),
                Some(index) => targets.push(index as usize),
            }
            match next.checked_add(1) {
                None => return Err(RebaseError::Overflow),
                Some(after) => next = after,
            }
        }
        i += 1;
    }
    Ok(targets)
}

/// Install `payload[i]` at `targets[i]` for every `i`, in order: append when
/// the target is the arena's current end, overwrite otherwise. Each payload
/// entry is moved out (left `None`).
pub fn install_overlays<T>(
    objects: &mut Vec<Option<T>>,
    targets: &[usize],
    payload: &mut [Option<T>],
) {
    let mut i = 0;
    while i < targets.len() {
        let object = payload[i].take();
        if targets[i] == objects.len() {
            objects.push(object);
        } else {
            objects[targets[i]] = object;
        }
        i += 1;
    }
}

/// A dense old-to-new handle table over an arena of `len` slots:
/// `table[reachable[i]] = Some(targets[i])`, `None` elsewhere. Handles at or
/// past `len` get no entry (a reachable handle is live, so in range).
pub fn remap_table(len: usize, reachable: &[usize], targets: &[usize]) -> Vec<Option<usize>> {
    let mut table = vec![None; len];
    let mut i = 0;
    while i < reachable.len() && i < targets.len() {
        let handle = reachable[i];
        if handle < table.len() {
            table[handle] = Some(targets[i]);
        }
        i += 1;
    }
    table
}

/// The entry of `table` for `handle`, or `None` past its end.
pub fn remapped_index(table: &[Option<usize>], handle: usize) -> Option<usize> {
    if handle < table.len() {
        table[handle]
    } else {
        None
    }
}
