//! Immutable diagnostic context with the caller's explicit mutable error state.
use super::{
    function_adapter::Renderer,
    function_diagnostics::{Emitter, Issue},
    function_operations::{HtfPolicy, StackNarrowFailure},
    function_structure::{StructureEmitter, StructureIssue},
    validate_function_with_emitter, TaStateFacts, TypedTables, ValidationError,
};
use crate::{CsilFunction, CsilProgram};
struct Context<'a, R> {
    name: &'a str,
    state_count: usize,
    renderer: &'a R,
}
impl<R: Renderer> Emitter<Vec<ValidationError>> for Context<'_, R> {
    fn emit(&self, state: &mut Vec<ValidationError>, issue: Issue) {
        state.push(self.renderer.render(self.name, self.state_count, issue));
    }
}
pub(super) struct StructureContext<'a, E> {
    pub emitter: &'a E,
}
impl<S, E: Emitter<S>> StructureEmitter<S> for StructureContext<'_, E> {
    fn emit(&self, state: &mut S, offset: usize, issue: StructureIssue) {
        self.emitter.emit(state, Issue::Structure { offset, issue });
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn run<
    T: TypedTables,
    F: TaStateFacts,
    H: HtfPolicy,
    N: StackNarrowFailure,
    R: Renderer,
>(
    program: &CsilProgram,
    func: &CsilFunction,
    errors: &mut Vec<ValidationError>,
    tables: &T,
    states: &[F],
    htf: &H,
    narrowing: &N,
    renderer: &R,
) {
    let context = Context {
        name: &func.name,
        state_count: program.ta_states.len(),
        renderer,
    };
    validate_function_with_emitter(
        program, func, &context, errors, tables, states, htf, narrowing,
    );
}
