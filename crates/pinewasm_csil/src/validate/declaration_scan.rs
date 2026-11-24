//! Test-only adapters that run the declaration visits with owned state.
use super::declaration_state;
use crate::CsilProgram;

pub(super) trait TaDeclaration: Sized {
    fn observe(self, index: usize) -> Self;
}
pub(super) trait InputDeclaration: Sized {
    fn observe(self, index: usize) -> Self;
}
struct OwnedContext;
impl<S: TaDeclaration> declaration_state::TaDeclaration<Option<S>> for OwnedContext {
    fn observe(&self, state: &mut Option<S>, index: usize) {
        *state = Some(
            state
                .take()
                .expect("owned declaration state")
                .observe(index),
        );
    }
}
impl<S: InputDeclaration> declaration_state::InputDeclaration<Option<S>> for OwnedContext {
    fn observe(&self, state: &mut Option<S>, index: usize) {
        *state = Some(
            state
                .take()
                .expect("owned declaration state")
                .observe(index),
        );
    }
}
pub(super) fn ta_states<S: TaDeclaration>(program: &CsilProgram, state: S) -> S {
    let mut state = Some(state);
    declaration_state::ta_states(program, &mut state, &OwnedContext);
    state.expect("owned declaration state")
}
pub(super) fn inputs<S: InputDeclaration>(program: &CsilProgram, state: S) -> S {
    let mut state = Some(state);
    declaration_state::inputs(program, &mut state, &OwnedContext);
    state.expect("owned declaration state")
}
