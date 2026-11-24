//! Validation phase order with immutable context and accumulated errors state.
use super::{function_finish, program_prefix};

pub(super) trait Stages<E> {
    fn prefix(&self, errors: &mut Vec<E>) -> program_prefix::Flow;
    fn ta_states(&self, errors: &mut Vec<E>);
    fn ta_bindings(&self, errors: &mut Vec<E>);
    fn inputs(&self, errors: &mut Vec<E>);
    fn ta_users(&self, errors: &mut Vec<E>);
    fn functions(&self, errors: &mut Vec<E>);
    fn budgets(&self, errors: &mut Vec<E>);
}

pub(super) fn run<C: Stages<E>, E>(context: &C) -> Result<(), Vec<E>> {
    let mut errors = Vec::new();
    if context.prefix(&mut errors) == program_prefix::Flow::Stop {
        return Err(errors);
    }
    let before_ta_states = errors.len();
    context.ta_states(&mut errors);
    context.ta_bindings(&mut errors);
    if errors.len() > before_ta_states {
        return Err(errors);
    }
    let before_inputs = errors.len();
    context.inputs(&mut errors);
    if errors.len() > before_inputs {
        return Err(errors);
    }
    context.ta_users(&mut errors);
    context.functions(&mut errors);
    context.budgets(&mut errors);
    function_finish::finish_errors(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Debug)]
    struct Error {
        phase: usize,
        drops: Rc<RefCell<Vec<usize>>>,
    }
    impl Drop for Error {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.phase);
        }
    }
    struct Context {
        effects: [i8; 7],
        stop: bool,
        panic_at: Option<usize>,
        events: RefCell<Vec<usize>>,
        pointer: RefCell<Option<*const Error>>,
        drops: Rc<RefCell<Vec<usize>>>,
    }
    impl Context {
        fn new(effects: [i8; 7]) -> Self {
            Self {
                effects,
                stop: false,
                panic_at: None,
                events: RefCell::new(vec![]),
                pointer: RefCell::new(None),
                drops: Rc::new(RefCell::new(vec![])),
            }
        }
        fn phase(&self, errors: &mut Vec<Error>, phase: usize) {
            self.events.borrow_mut().push(phase);
            assert_ne!(self.panic_at, Some(phase), "injected stage panic");
            match self.effects[phase] {
                1 => errors.push(Error {
                    phase,
                    drops: Rc::clone(&self.drops),
                }),
                -1 => {
                    errors.pop();
                }
                _ => {}
            }
            if phase == 0 && !errors.is_empty() {
                *self.pointer.borrow_mut() = Some(errors.as_ptr());
            }
        }
    }
    impl Stages<Error> for Context {
        fn prefix(&self, e: &mut Vec<Error>) -> program_prefix::Flow {
            self.phase(e, 0);
            if self.stop {
                program_prefix::Flow::Stop
            } else {
                program_prefix::Flow::Continue
            }
        }
        fn ta_states(&self, e: &mut Vec<Error>) {
            self.phase(e, 1);
        }
        fn ta_bindings(&self, e: &mut Vec<Error>) {
            self.phase(e, 2);
        }
        fn inputs(&self, e: &mut Vec<Error>) {
            self.phase(e, 3);
        }
        fn ta_users(&self, e: &mut Vec<Error>) {
            self.phase(e, 4);
        }
        fn functions(&self, e: &mut Vec<Error>) {
            self.phase(e, 5);
        }
        fn budgets(&self, e: &mut Vec<Error>) {
            self.phase(e, 6);
        }
    }
    #[test]
    fn prefix_and_strict_phase_delta_guards() {
        let prefix = Context::new([1, 0, 0, 0, 0, 0, 0]);
        let errors = run(&prefix).unwrap_err();
        assert_eq!(*prefix.events.borrow(), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(errors.as_ptr(), prefix.pointer.borrow().unwrap());
        assert_eq!(errors[0].phase, 0);
        let mut stop = Context::new([1, 0, 0, 0, 0, 0, 0]);
        stop.stop = true;
        assert!(run(&stop).is_err());
        assert_eq!(*stop.events.borrow(), [0]);
        for effects in [[0, 1, 1, 0, 0, 0, 0], [0, 0, 1, 0, 0, 0, 0]] {
            let context = Context::new(effects);
            let errors = run(&context).unwrap_err();
            assert_eq!(*context.events.borrow(), [0, 1, 2]);
            assert_eq!(errors.last().unwrap().phase, 2);
        }
        let input = Context::new([0, 0, 0, 1, 0, 0, 0]);
        assert!(run(&input).is_err());
        assert_eq!(*input.events.borrow(), [0, 1, 2, 3]);
        let decreasing = Context::new([1, -1, 0, 0, 0, 0, 0]);
        assert!(run(&decreasing).is_ok());
        assert_eq!(*decreasing.events.borrow(), [0, 1, 2, 3, 4, 5, 6]);
    }
    #[test]
    fn late_errors_continue_and_unwind_drops_original_payload() {
        let late = Context::new([0, 0, 0, 0, 1, 1, 1]);
        let errors = run(&late).unwrap_err();
        assert_eq!(
            errors.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [4, 5, 6]
        );
        assert_eq!(*late.events.borrow(), [0, 1, 2, 3, 4, 5, 6]);
        drop(errors);
        assert_eq!(*late.drops.borrow(), [4, 5, 6]);
        let mut panic = Context::new([1, 0, 0, 0, 0, 0, 0]);
        panic.panic_at = Some(2);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&panic))).is_err());
        assert_eq!(*panic.events.borrow(), [0, 1, 2]);
        assert_eq!(*panic.drops.borrow(), [0]);
        assert!(run(&Context::new([0; 7])).is_ok());
    }
    fn program() -> crate::CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"driver\")\nplot(close)");
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    #[test]
    fn real_program_diagnostics_match_a_separate_reference_loop() {
        let base = program();
        for mode in 0..7 {
            let mut p = base.clone();
            match mode {
                1 => p.functions.clear(),
                2 => p.functions[0].source_map.clear(),
                3 => p.functions[0].code = vec![crate::CsilInstr::Pop, crate::CsilInstr::Return],
                4 => p.functions[0].max_stack = 0,
                5 => p.header.runtime_abi_version = u32::MAX,
                6 => {
                    let f = &p.functions[0];
                    p.functions[0].budget_reference = Some(crate::BudgetReference {
                        version: 99,
                        code: f.code.clone(),
                        source_map: f.source_map.clone(),
                        max_stack: f.max_stack,
                    });
                }
                _ => {}
            }
            assert_eq!(
                super::super::validate(&p),
                super::super::baseline_validate_driver(&p),
                "mode {mode}"
            );
        }
    }
    #[test]
    fn generic_production_specialization_preserves_full_diagnostics_and_guards() {
        use super::super::budget_observer::{Checks, ProductionChecks};
        struct TracedChecks {
            events: RefCell<Vec<&'static str>>,
        }
        impl Checks for TracedChecks {
            type ReferenceProgram = crate::CsilProgram;
            fn within_limit(&self, reference: &crate::BudgetReference) -> bool {
                self.events.borrow_mut().push("limit");
                ProductionChecks.within_limit(reference)
            }
            fn reference_validates(
                &self,
                program: &crate::CsilProgram,
                index: usize,
                reference: &crate::BudgetReference,
            ) -> (bool, Self::ReferenceProgram) {
                self.events.borrow_mut().push("validate");
                // This deliberately calls the production dictionary: recursive validation
                // must not re-enter with this supplied checks dictionary.
                ProductionChecks.reference_validates(program, index, reference)
            }
            fn matches(
                &self,
                reference: &crate::BudgetReference,
                target: &crate::CsilFunction,
                slots: &[crate::SlotDef],
            ) -> bool {
                self.events.borrow_mut().push("matches");
                ProductionChecks.matches(reference, target, slots)
            }
        }
        let parsed = pinewasm_parse::parse(
            "//@version=6\nindicator(\"generic stages\")\nx = input.int(3)\nplot(ta.ema(close, x))",
        );
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        let mut base = crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers");
        for function in &mut base.functions {
            function.budget_reference = None;
        }
        for mode in 0..8 {
            let mut p = base.clone();
            let f = &p.functions[0];
            let mut reference = crate::BudgetReference {
                version: 1,
                code: f.code.clone(),
                source_map: f.source_map.clone(),
                max_stack: f.max_stack,
            };
            match mode {
                1 => p.header.runtime_abi_version = u32::MAX,
                2 => p.ta_states[0].params.clear(),
                3 => p.inputs[0].id = u32::MAX,
                4 => reference.version = 99,
                5 => reference.code = vec![crate::CsilInstr::Pop; reference.code.len()],
                6 => reference.source_map.clear(),
                7 => reference.code.reserve(
                    crate::MAX_BUDGET_REFERENCE_BYTES / std::mem::size_of::<crate::CsilInstr>() + 1,
                ),
                _ => {}
            }
            p.functions[0].budget_reference = Some(reference);
            let checks = TracedChecks {
                events: RefCell::new(Vec::new()),
            };
            let before = p.clone();
            let generic = super::super::validate_with_checks(&p, &checks);
            assert_eq!(
                generic,
                super::super::validate(&p),
                "default specialization mode {mode}"
            );
            assert_eq!(
                generic,
                super::super::baseline_validate_driver(&p),
                "sequential driver diagnostics mode {mode}"
            );
            assert_eq!(p, before);
            let events = checks.events.borrow();
            match mode {
                0 => assert_eq!(*events, ["limit", "validate", "matches"]),
                1..=4 => assert!(
                    events.is_empty(),
                    "early rejection must skip supplied callbacks"
                ),
                5 => assert_eq!(*events, ["limit", "validate"]),
                6 | 7 => assert_eq!(*events, ["limit"]),
                _ => unreachable!(),
            }
            #[cfg(feature = "aeneas")]
            assert_eq!(
                super::super::aeneas_native_program_stages(&p, &ProductionChecks),
                generic
            );
        }
    }
}
