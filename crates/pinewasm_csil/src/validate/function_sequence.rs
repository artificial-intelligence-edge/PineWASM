//! The production ordered function inventory, retaining the caller's error vector.
use super::{
    function_adapter::{self, Renderer},
    function_operations::{HtfPolicy, StackNarrowFailure},
    TaStateFacts, TypedTables, ValidationError,
};
use crate::CsilProgram;

#[allow(clippy::too_many_arguments)]
pub(super) fn all_functions<
    T: TypedTables,
    F: TaStateFacts,
    H: HtfPolicy,
    N: StackNarrowFailure,
    R: Renderer,
>(
    program: &CsilProgram,
    errors: &mut Vec<ValidationError>,
    tables: &T,
    states: &[F],
    htf: &H,
    narrowing: &N,
    renderer: &R,
) {
    for func in &program.functions {
        function_adapter::run(
            program, func, errors, tables, states, htf, narrowing, renderer,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        function_adapter::ProductionRenderer,
        function_operations::{ProductionStackNarrowFailure, ProgramHtfPolicy},
        ProgramTables,
    };
    use super::*;
    fn compiled() -> CsilProgram {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"Sequence χ\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).unwrap()
    }
    fn new_loop<R: Renderer>(
        program: &CsilProgram,
        errors: &mut Vec<ValidationError>,
        renderer: &R,
    ) {
        all_functions(
            program,
            errors,
            &ProgramTables { program },
            &program.ta_states,
            &ProgramHtfPolicy {
                inputs: &program.inputs,
            },
            &ProductionStackNarrowFailure,
            renderer,
        );
    }
    // Test-only copy of the per-function loop, including each concrete entry's own tables.
    fn old_loop(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
        for func in &program.functions {
            super::super::validate_function(program, func, errors);
        }
    }
    fn equivalent(program: &CsilProgram) -> Vec<ValidationError> {
        let mut old = vec![ValidationError::Header("existing χ".into())];
        let mut new = vec![ValidationError::Header("existing χ".into())];
        old_loop(program, &mut old);
        new_loop(program, &mut new, &ProductionRenderer);
        assert_eq!(new, old);
        new
    }
    #[test]
    fn actual_success_preserves_existing_prefix() {
        assert_eq!(
            equivalent(&compiled()),
            vec![ValidationError::Header("existing χ".into())]
        );
    }
    #[test]
    fn malformed_second_function_is_observed_in_order() {
        let mut p = compiled();
        let mut second = p.functions[0].clone();
        second.name = "second χ".into();
        second.code.clear();
        second.source_map.clear();
        p.functions.push(second);
        let errors = equivalent(&p);
        assert_eq!(errors.len(), 2);
        assert!(
            matches!(&errors[1], ValidationError::Structure { function, message } if function == "second χ" && message.contains("empty"))
        );
    }
    #[test]
    fn source_map_failure_does_not_skip_later_function() {
        let mut p = compiled();
        p.functions[0].source_map.clear();
        let mut second = p.functions[0].clone();
        second.name = "later".into();
        second.code.clear();
        second.source_map.clear();
        p.functions.push(second);
        let errors = equivalent(&p);
        assert_eq!(errors.len(), 3);
        assert!(
            matches!(&errors[1], ValidationError::Structure { message, .. } if message.starts_with("source map"))
        );
        assert!(
            matches!(&errors[2], ValidationError::Structure { function, message } if function == "later" && message.contains("empty"))
        );
    }
    #[test]
    fn empty_inventory_preserves_prefix_without_rendering() {
        struct PanicRenderer;
        impl Renderer for PanicRenderer {
            fn render(
                &self,
                _: &str,
                _: usize,
                _: super::super::function_diagnostics::Issue,
            ) -> ValidationError {
                panic!("unexpected render")
            }
        }
        let mut p = compiled();
        p.functions.clear();
        let mut errors = equivalent(&p);
        new_loop(&p, &mut errors, &PanicRenderer);
        assert_eq!(errors, vec![ValidationError::Header("existing χ".into())]);
    }
    #[test]
    fn panic_keeps_prefix_and_prior_function_diagnostics() {
        use std::cell::Cell;
        struct SecondPanic(Cell<usize>);
        impl Renderer for SecondPanic {
            fn render(
                &self,
                name: &str,
                count: usize,
                issue: super::super::function_diagnostics::Issue,
            ) -> ValidationError {
                let n = self.0.get();
                self.0.set(n + 1);
                assert_eq!(n, 0, "second emission");
                super::super::function_diagnostics::render(name, count, issue)
            }
        }
        let mut p = compiled();
        p.functions[0].code.clear();
        p.functions[0].source_map.clear();
        let mut second = p.functions[0].clone();
        second.name = "second".into();
        p.functions.push(second);
        let mut errors = vec![ValidationError::Header("prefix".into())];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            new_loop(&p, &mut errors, &SecondPanic(Cell::new(0)));
        }));
        assert!(result.is_err());
        let mut old_errors = vec![ValidationError::Header("prefix".into())];
        let old_renderer = SecondPanic(Cell::new(0));
        let old_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for func in &p.functions {
                function_adapter::run(
                    &p,
                    func,
                    &mut old_errors,
                    &ProgramTables { program: &p },
                    &p.ta_states,
                    &ProgramHtfPolicy { inputs: &p.inputs },
                    &ProductionStackNarrowFailure,
                    &old_renderer,
                );
            }
        }));
        assert!(old_result.is_err());
        assert_eq!(errors, old_errors);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], ValidationError::Header("prefix".into()));
        assert!(
            matches!(&errors[1], ValidationError::Structure { message, .. } if message.contains("empty"))
        );
    }
}
