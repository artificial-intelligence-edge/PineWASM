//! Production function entry that borrows the caller's diagnostic vector.
use super::{
    function_diagnostics::{self, Issue},
    function_operations::{HtfPolicy, StackNarrowFailure},
    TaStateFacts, TypedTables, ValidationError,
};
use crate::{CsilFunction, CsilProgram};

pub(super) trait Renderer {
    fn render(&self, name: &str, state_count: usize, issue: Issue) -> ValidationError;
}
pub(super) struct ProductionRenderer;
impl Renderer for ProductionRenderer {
    fn render(&self, name: &str, state_count: usize, issue: Issue) -> ValidationError {
        function_diagnostics::render(name, state_count, issue)
    }
}
// Keep the caller's diagnostic vector borrowed in place through the actual core.
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
    super::function_state::run(
        program, func, errors, tables, states, htf, narrowing, renderer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn compiled() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"Adapter χ\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).unwrap()
    }
    struct PanicRenderer;
    impl Renderer for PanicRenderer {
        fn render(&self, _: &str, _: usize, _: Issue) -> ValidationError {
            panic!("renderer invoked")
        }
    }
    #[test]
    fn happy_path_never_renders_and_preserves_existing_errors() {
        let p = compiled();
        let before = vec![ValidationError::Header("existing χ".into())];
        let mut errors = vec![ValidationError::Header("existing χ".into())];
        run(
            &p,
            &p.functions[0],
            &mut errors,
            &super::super::ProgramTables { program: &p },
            &p.ta_states,
            &super::super::function_operations::ProgramHtfPolicy { inputs: &p.inputs },
            &super::super::function_operations::ProductionStackNarrowFailure,
            &PanicRenderer,
        );
        assert_eq!(errors, before);
    }
    #[test]
    fn ordinary_renderer_matches_existing_borrowed_entry() {
        let mut p = compiled();
        p.functions[0].source_map.clear();
        p.functions[0].max_stack = 0;
        let mut old = vec![ValidationError::Header("prefix".into())];
        let mut new = vec![ValidationError::Header("prefix".into())];
        let mut sink = function_diagnostics::VecSink {
            name: &p.functions[0].name,
            state_count: p.ta_states.len(),
            errors: &mut old,
        };
        super::super::validate_function_with_sink(&p, &p.functions[0], &mut sink);
        super::super::validate_function(&p, &p.functions[0], &mut new);
        assert_eq!(new, old);
        assert!(new.len() > 1);
    }
    #[test]
    fn renderer_panic_retains_original_prefix_and_prior_emits() {
        use std::cell::Cell;
        struct SecondPanic(Cell<usize>);
        impl Renderer for SecondPanic {
            fn render(&self, name: &str, count: usize, issue: Issue) -> ValidationError {
                let n = self.0.get();
                self.0.set(n + 1);
                assert!(n == 0, "second emission");
                function_diagnostics::render(name, count, issue)
            }
        }
        let mut p = compiled();
        p.functions[0].source_map.clear();
        p.functions[0].max_stack = super::super::MAX_DECLARED_STACK + 1;
        let mut errors = vec![ValidationError::Header("prefix".into())];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(
                &p,
                &p.functions[0],
                &mut errors,
                &super::super::ProgramTables { program: &p },
                &p.ta_states,
                &super::super::function_operations::ProgramHtfPolicy { inputs: &p.inputs },
                &super::super::function_operations::ProductionStackNarrowFailure,
                &SecondPanic(Cell::new(0)),
            );
        }));
        assert!(result.is_err());
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], ValidationError::Header("prefix".into()));
        assert!(
            matches!(&errors[1], ValidationError::Structure { message, .. } if message.starts_with("source map"))
        );
    }
}
