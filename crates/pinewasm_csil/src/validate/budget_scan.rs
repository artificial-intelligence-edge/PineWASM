//! Test-only adapter that runs the reference visit with owned state.
use crate::CsilProgram;

pub(super) trait Reference: Sized {
    fn observe(self, index: usize) -> Self;
}
struct Owned;
impl<S: Reference> super::budget_state::Reference<Option<S>> for Owned {
    fn observe(&self, state: &mut Option<S>, index: usize) {
        let old = state.take().expect("owned compatibility state present");
        *state = Some(old.observe(index));
    }
}
pub(super) fn references<S: Reference>(program: &CsilProgram, state: S) -> S {
    let mut state = Some(state);
    super::budget_state::references(program, &Owned, &mut state);
    state.expect("owned compatibility state retained")
}
