//! Present-reference visits with immutable context and explicit caller state.
use crate::CsilProgram;

pub(super) trait Reference<S> {
    fn observe(&self, state: &mut S, index: usize);
}

pub(super) fn references<C: Reference<S>, S>(program: &CsilProgram, context: &C, state: &mut S) {
    let mut index = 0;
    while index < program.functions.len() {
        if program.functions[index].budget_reference.is_some() {
            context.observe(state, index);
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::super::{budget_scan::Reference as _, BudgetContext, BudgetErrors, ValidationError};
    use super::*;
    fn program() -> CsilProgram {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"budget state\")\nplot(close)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        let mut p = crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers");
        for f in &mut p.functions {
            f.budget_reference = None;
        }
        p
    }
    fn reference(p: &CsilProgram) -> crate::BudgetReference {
        let f = &p.functions[0];
        crate::BudgetReference {
            version: 1,
            code: f.code.clone(),
            source_map: f.source_map.clone(),
            max_stack: f.max_stack,
        }
    }
    struct Trace {
        panic_at: Option<usize>,
    }
    impl Reference<Vec<usize>> for Trace {
        fn observe(&self, state: &mut Vec<usize>, index: usize) {
            assert_ne!(self.panic_at, Some(index), "injected observer failure");
            state.push(index);
        }
    }
    #[test]
    fn none_empty_order_and_panic_prefix() {
        let mut p = program();
        let mut state = vec![99];
        references(&p, &Trace { panic_at: Some(0) }, &mut state);
        assert_eq!(state, [99]);
        let r = reference(&p);
        let f = p.functions[0].clone();
        p.functions = vec![f.clone(), f.clone(), f.clone(), f];
        p.functions[0].budget_reference = Some(r.clone());
        p.functions[2].budget_reference = Some(r.clone());
        p.functions[3].budget_reference = Some(r);
        references(&p, &Trace { panic_at: None }, &mut state);
        assert_eq!(state, [99, 0, 2, 3]);
        let mut prefix = vec![99];
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| references(
                &p,
                &Trace { panic_at: Some(2) },
                &mut prefix
            )))
            .is_err()
        );
        assert_eq!(prefix, [99, 0]);
        p.functions.clear();
        references(&p, &Trace { panic_at: None }, &mut prefix);
        assert_eq!(prefix, [99, 0]);
    }
    #[test]
    fn production_callback_matches_independent_old_body() {
        let base = program();
        for mode in 0..7 {
            let mut p = base.clone();
            let mut r = reference(&p);
            match mode {
                1 => r.version = 99,
                2 => r.source_map.clear(),
                3 => r.code = vec![crate::CsilInstr::Pop; r.source_map.len()],
                4 => r.max_stack = 0,
                5 => {
                    p.functions.push(p.functions[0].clone());
                }
                6 => {
                    r.version = 99;
                    r.code = vec![crate::CsilInstr::Pop; r.source_map.len()];
                }
                _ => {}
            }
            let index = usize::from(mode == 5);
            p.functions[index].budget_reference = Some(r);
            let sentinel = ValidationError::Metadata("sentinel".into());
            let mut old = vec![ValidationError::Metadata("sentinel".into())];
            let mut new = vec![sentinel];
            // Direct loop and callback, not the owned-state adapter.
            let mut old_state = BudgetErrors {
                program: &p,
                errors: &mut old,
            };
            let mut i = 0;
            while i < p.functions.len() {
                if p.functions[i].budget_reference.is_some() {
                    old_state = old_state.observe(i);
                }
                i += 1;
            }
            references(
                &p,
                &BudgetContext {
                    program: &p,
                    checks: &super::super::budget_observer::ProductionChecks,
                },
                &mut new,
            );
            assert_eq!(new, old, "mode {mode}");
            if mode == 0 {
                assert_eq!(new.len(), 1);
            }
            if mode == 6 {
                assert!(
                    matches!(&new[1],ValidationError::Structure{message,..} if message=="invalid or oversized budget reference")
                );
                assert_eq!(new.len(), 2);
            }
        }
    }
}
