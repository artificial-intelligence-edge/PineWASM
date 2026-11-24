//! Production budget validation with immutable checks and explicit diagnostics.
use super::ValidationError;
use crate::{BudgetReference, CsilFunction, CsilProgram, SlotDef};

pub(super) trait Checks {
    type ReferenceProgram;
    fn within_limit(&self, reference: &BudgetReference) -> bool;
    fn reference_validates(
        &self,
        program: &CsilProgram,
        index: usize,
        reference: &BudgetReference,
    ) -> (bool, Self::ReferenceProgram);
    fn matches(
        &self,
        reference: &BudgetReference,
        target: &CsilFunction,
        slots: &[SlotDef],
    ) -> bool;
}

pub(super) trait ErrorFactory<E> {
    fn invalid(&self, function: &CsilFunction) -> E;
    fn mismatch(&self, function: &CsilFunction) -> E;
}

/// Callbacks cannot borrow or replace the caller's mutable diagnostic vector.
/// Their truth is not needed for prefix retention, but production uses the
/// concrete checks below rather than an assumed successful validator.
pub(super) fn observe<C: Checks, F: ErrorFactory<E>, E>(
    program: &CsilProgram,
    errors: &mut Vec<E>,
    index: usize,
    checks: &C,
    factory: &F,
) {
    let func = &program.functions[index];
    let Some(reference) = &func.budget_reference else {
        return;
    };
    if index != 0
        || !crate::budget_reference_version_known(reference.version)
        || !checks.within_limit(reference)
        || reference.code.len() != reference.source_map.len()
    {
        errors.push(factory.invalid(func));
        return;
    }
    // Keep the owned reference copy alive through matching and error allocation.
    // A named binding drops at scope end.
    let (valid, _reference_program) = checks.reference_validates(program, index, reference);
    if !valid || !checks.matches(reference, func, &program.series_slots) {
        errors.push(factory.mismatch(func));
    }
}

pub(super) struct ProductionChecks;

/// The production copy keeps every field of the input program, clears all
/// references, and changes only the selected function's reference-owned
/// code/map/stack fields.
fn reference_program(
    program: &CsilProgram,
    index: usize,
    reference: &BudgetReference,
) -> CsilProgram {
    let mut reference_program = program.clone();
    for function in &mut reference_program.functions {
        function.budget_reference = None;
    }
    let reference_func = &mut reference_program.functions[index];
    reference_func.code.clone_from(&reference.code);
    reference_func.source_map.clone_from(&reference.source_map);
    reference_func.max_stack = reference.max_stack;
    reference_program
}

impl Checks for ProductionChecks {
    type ReferenceProgram = CsilProgram;
    fn within_limit(&self, reference: &BudgetReference) -> bool {
        crate::budget_reference_within_limit(reference)
    }
    fn reference_validates(
        &self,
        program: &CsilProgram,
        index: usize,
        reference: &BudgetReference,
    ) -> (bool, CsilProgram) {
        let reference_program = reference_program(program, index, reference);
        let valid = super::validate(&reference_program).is_ok();
        (valid, reference_program)
    }
    fn matches(
        &self,
        reference: &BudgetReference,
        target: &CsilFunction,
        slots: &[SlotDef],
    ) -> bool {
        crate::lower::matches_budget_reference(reference, target, slots)
    }
}

/// The checks for a program [`crate::lower_mir`] returned in this process
/// and that nothing has changed since.
///
/// Lowering attaches a reference only when its encoding is under the size
/// limit, and derives the function's code from the reference with the
/// pipeline the reference's version names. Checking either here would repeat,
/// on the same input, the count and the optimization lowering has just done.
/// That the reference is valid code is not something lowering establishes, so
/// that check stays.
pub(super) struct LoweredChecks;

impl Checks for LoweredChecks {
    type ReferenceProgram = CsilProgram;
    fn within_limit(&self, _: &BudgetReference) -> bool {
        true
    }
    fn reference_validates(
        &self,
        program: &CsilProgram,
        index: usize,
        reference: &BudgetReference,
    ) -> (bool, CsilProgram) {
        ProductionChecks.reference_validates(program, index, reference)
    }
    fn matches(&self, _: &BudgetReference, _: &CsilFunction, _: &[SlotDef]) -> bool {
        true
    }
}

pub(super) struct ProductionErrorFactory;
impl ErrorFactory<ValidationError> for ProductionErrorFactory {
    fn invalid(&self, function: &CsilFunction) -> ValidationError {
        ValidationError::Structure {
            function: function.name.clone(),
            message: "invalid or oversized budget reference".to_string(),
        }
    }
    fn mismatch(&self, function: &CsilFunction) -> ValidationError {
        ValidationError::Structure {
            function: function.name.clone(),
            message: "budget reference does not validate or reproduce optimized code".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn program() -> CsilProgram {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"budget observer χ\")\nplot(close)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        let mut program = crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers");
        for function in &mut program.functions {
            function.budget_reference = None;
        }
        program
    }
    fn reference(program: &CsilProgram) -> BudgetReference {
        let function = &program.functions[0];
        BudgetReference {
            version: 1,
            code: function.code.clone(),
            source_map: function.source_map.clone(),
            max_stack: function.max_stack,
        }
    }
    type Events = Rc<RefCell<Vec<&'static str>>>;
    struct KeepAlive(Events);
    impl Drop for KeepAlive {
        fn drop(&mut self) {
            self.0.borrow_mut().push("drop");
        }
    }
    struct Spy {
        events: Events,
        limit: bool,
        valid: bool,
        matched: bool,
        panic_at: Option<&'static str>,
    }
    impl Spy {
        fn record(&self, event: &'static str) {
            self.events.borrow_mut().push(event);
            assert_ne!(self.panic_at, Some(event), "injected {event}");
        }
    }
    impl Checks for Spy {
        type ReferenceProgram = KeepAlive;
        fn within_limit(&self, _: &BudgetReference) -> bool {
            self.record("cap");
            self.limit
        }
        fn reference_validates(
            &self,
            _: &CsilProgram,
            _: usize,
            _: &BudgetReference,
        ) -> (bool, KeepAlive) {
            self.record("validate");
            (self.valid, KeepAlive(Rc::clone(&self.events)))
        }
        fn matches(&self, _: &BudgetReference, _: &CsilFunction, _: &[SlotDef]) -> bool {
            self.record("matches");
            self.matched
        }
    }
    impl ErrorFactory<ValidationError> for Spy {
        fn invalid(&self, function: &CsilFunction) -> ValidationError {
            self.record("invalid");
            ProductionErrorFactory.invalid(function)
        }
        fn mismatch(&self, function: &CsilFunction) -> ValidationError {
            self.record("mismatch");
            ProductionErrorFactory.mismatch(function)
        }
    }
    fn spy(limit: bool, valid: bool, matched: bool, panic_at: Option<&'static str>) -> Spy {
        Spy {
            events: Rc::new(RefCell::new(Vec::new())),
            limit,
            valid,
            matched,
            panic_at,
        }
    }

    #[test]
    fn guards_short_circuit_and_factories_are_lazy() {
        for mode in 0..7 {
            let mut program = program();
            let mut reference = reference(&program);
            let spy = spy(mode != 3, true, true, None);
            let mut index = 0;
            match mode {
                0 => {}
                1 => {
                    program.functions.push(program.functions[0].clone());
                    index = 1;
                }
                2 => reference.version = 99,
                4 => reference.source_map.clear(),
                _ => {}
            }
            if mode != 0 {
                program.functions[index].budget_reference = Some(reference);
            }
            let mut errors = vec![ValidationError::Metadata("sentinel".into())];
            observe(&program, &mut errors, index, &spy, &spy);
            let expected: &[&str] = match mode {
                0 => &[],
                1 | 2 => &["invalid"],
                3 | 4 => &["cap", "invalid"],
                _ => &["cap", "validate", "matches", "drop"],
            };
            assert_eq!(*spy.events.borrow(), expected, "mode {mode}");
            assert_eq!(errors[0], ValidationError::Metadata("sentinel".into()));
            assert_eq!(errors.len(), usize::from((1..=4).contains(&mode)) + 1);
        }
    }

    #[test]
    fn validation_matching_and_drop_order() {
        for (valid, matched, expected) in [
            (false, true, vec!["cap", "validate", "mismatch", "drop"]),
            (
                true,
                false,
                vec!["cap", "validate", "matches", "mismatch", "drop"],
            ),
            (true, true, vec!["cap", "validate", "matches", "drop"]),
        ] {
            let mut program = program();
            program.functions[0].budget_reference = Some(reference(&program));
            let spy = spy(true, valid, matched, None);
            let mut errors = vec![ValidationError::Metadata("sentinel".into())];
            observe(&program, &mut errors, 0, &spy, &spy);
            assert_eq!(*spy.events.borrow(), expected);
            assert_eq!(errors.len(), if valid && matched { 1 } else { 2 });
        }
    }

    #[test]
    fn panic_retains_prefix_and_copy_until_factory_or_matching_unwinds() {
        for event in ["cap", "validate", "matches", "mismatch", "invalid"] {
            let mut program = program();
            let mut reference = reference(&program);
            if event == "invalid" {
                reference.version = 99;
            }
            program.functions[0].budget_reference = Some(reference);
            let spy = spy(true, true, false, Some(event));
            let mut errors = vec![ValidationError::Metadata("sentinel".into())];
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observe(&program, &mut errors, 0, &spy, &spy);
            }))
            .is_err());
            assert_eq!(errors, [ValidationError::Metadata("sentinel".into())]);
            if matches!(event, "matches" | "mismatch") {
                assert_eq!(spy.events.borrow().last(), Some(&"drop"));
            }
        }
        let program = program();
        let spy = spy(true, true, true, None);
        let mut errors = vec![ValidationError::Metadata("sentinel".into())];
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observe(&program, &mut errors, program.functions.len(), &spy, &spy);
        }))
        .is_err());
        assert!(spy.events.borrow().is_empty());
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn production_copy_clears_every_reference_and_preserves_other_fields() {
        let mut original = program();
        original.functions.push(original.functions[0].clone());
        let mut reference = reference(&original);
        reference.code = vec![crate::CsilInstr::PushNa, crate::CsilInstr::Return];
        reference.source_map.truncate(2);
        reference.max_stack = 777;
        for function in &mut original.functions {
            function.budget_reference = Some(reference.clone());
        }
        let before = original.clone();
        let copy = reference_program(&original, 0, &reference);
        let mut expected = before.clone();
        for function in &mut expected.functions {
            function.budget_reference = None;
        }
        expected.functions[0].code = reference.code.clone();
        expected.functions[0].source_map = reference.source_map.clone();
        expected.functions[0].max_stack = reference.max_stack;
        assert_eq!(copy, expected);
        assert_eq!(original, before);
        let (valid, actual_copy) = ProductionChecks.reference_validates(&original, 0, &reference);
        assert_eq!(actual_copy, expected);
        assert_eq!(valid, super::super::validate(&expected).is_ok());
        assert_eq!(original, before);
    }

    #[test]
    fn production_outputs_match_verbatim_old_observer() {
        use super::super::{budget_scan::Reference as _, BudgetErrors};
        for mode in 0..9 {
            let mut program = program();
            let mut reference = reference(&program);
            let mut index = 0;
            match mode {
                1 => reference.version = 99,
                2 => reference.source_map.clear(),
                3 => reference.code = vec![crate::CsilInstr::Pop; reference.source_map.len()],
                4 => reference.max_stack = 0,
                5 => {
                    program.functions.push(program.functions[0].clone());
                    index = 1;
                }
                6 => reference.code.reserve(
                    crate::MAX_BUDGET_REFERENCE_BYTES / std::mem::size_of::<crate::CsilInstr>() + 1,
                ),
                7 => {
                    reference.version = 99;
                    reference.source_map.clear();
                }
                _ => {}
            }
            if mode != 8 {
                program.functions[index].budget_reference = Some(reference);
            }
            let mut old = vec![ValidationError::Metadata("sentinel χ".into())];
            let mut new = vec![ValidationError::Metadata("sentinel χ".into())];
            BudgetErrors {
                program: &program,
                errors: &mut old,
            }
            .observe(index);
            super::super::budget_state::Reference::observe(
                &super::super::BudgetContext {
                    program: &program,
                    checks: &ProductionChecks,
                },
                &mut new,
                index,
            );
            assert_eq!(new, old, "mode {mode}");
            assert_eq!(new[0], ValidationError::Metadata("sentinel χ".into()));
        }
    }
}
