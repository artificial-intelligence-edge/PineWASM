//! TA binding orchestration with immutable context and explicit diagnostic state.

/// Exact binding-vector count used by the production stage context.
pub(super) fn count(program: &crate::CsilProgram) -> usize {
    program.ta_input_bindings.len()
}
pub(super) trait Stages<Secondary, Slots, Targets, Errors> {
    fn new_secondary(&self) -> Secondary;
    fn scan_secondary(&self, map: Secondary) -> Secondary;
    fn finish_secondary(&self, map: &Secondary, errors: &mut Errors);
    fn collect_slots(&self) -> Slots;
    fn scan_slots(&self, map: Slots) -> Slots;
    fn new_targets(&self) -> Targets;
    fn binding_count(&self) -> usize;
    fn binding(
        &self,
        secondary: &Secondary,
        slots: &Slots,
        targets: &mut Targets,
        errors: &mut Errors,
        index: usize,
    );
}
pub(super) fn run<C, Secondary, Slots, Targets, Errors>(context: &C, errors: &mut Errors)
where
    C: Stages<Secondary, Slots, Targets, Errors>,
{
    let secondary = context.scan_secondary(context.new_secondary());
    context.finish_secondary(&secondary, errors);
    let slots = context.scan_slots(context.collect_slots());
    let mut targets = context.new_targets();
    let count = context.binding_count();
    let mut index = 0;
    while index < count {
        context.binding(&secondary, &slots, &mut targets, errors, index);
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Map<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
    impl Drop for Map<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push(self.1);
        }
    }
    struct Context<'a> {
        trace: &'a RefCell<Vec<&'static str>>,
        panic_row: bool,
    }
    impl<'a> Stages<Map<'a>, Map<'a>, Map<'a>, Vec<usize>> for Context<'a> {
        fn new_secondary(&self) -> Map<'a> {
            self.trace.borrow_mut().push("new-secondary");
            Map(self.trace, "drop-secondary")
        }
        fn scan_secondary(&self, map: Map<'a>) -> Map<'a> {
            self.trace.borrow_mut().push("scan-secondary");
            map
        }
        fn finish_secondary(&self, _: &Map<'a>, errors: &mut Vec<usize>) {
            self.trace.borrow_mut().push("finish-secondary");
            errors.push(10);
        }
        fn collect_slots(&self) -> Map<'a> {
            self.trace.borrow_mut().push("collect-slots");
            Map(self.trace, "drop-slots")
        }
        fn scan_slots(&self, map: Map<'a>) -> Map<'a> {
            self.trace.borrow_mut().push("scan-slots");
            map
        }
        fn new_targets(&self) -> Map<'a> {
            self.trace.borrow_mut().push("new-targets");
            Map(self.trace, "drop-targets")
        }
        fn binding_count(&self) -> usize {
            self.trace.borrow_mut().push("count");
            2
        }
        fn binding(
            &self,
            _: &Map<'a>,
            _: &Map<'a>,
            _: &mut Map<'a>,
            errors: &mut Vec<usize>,
            index: usize,
        ) {
            self.trace
                .borrow_mut()
                .push(if index == 0 { "row-0" } else { "row-1" });
            errors.push(index);
            assert!(!(self.panic_row && index == 0), "row callback failure");
        }
    }
    #[test]
    fn phase_order_count_once_and_reverse_drop() {
        let trace = RefCell::new(Vec::new());
        let mut errors = vec![99];
        run(
            &Context {
                trace: &trace,
                panic_row: false,
            },
            &mut errors,
        );
        assert_eq!(errors, [99, 10, 0, 1]);
        assert_eq!(
            *trace.borrow(),
            [
                "new-secondary",
                "scan-secondary",
                "finish-secondary",
                "collect-slots",
                "scan-slots",
                "new-targets",
                "count",
                "row-0",
                "row-1",
                "drop-targets",
                "drop-slots",
                "drop-secondary"
            ]
        );
    }
    #[test]
    fn callback_panic_retains_prefix_and_stops_later_rows() {
        let trace = RefCell::new(Vec::new());
        let mut errors = vec![99];
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(
                &Context {
                    trace: &trace,
                    panic_row: true,
                },
                &mut errors,
            );
        }));
        assert!(outcome.is_err());
        assert_eq!(errors, [99, 10, 0]);
        assert!(!trace.borrow().contains(&"row-1"));
        assert_eq!(
            &trace.borrow()[8..],
            ["drop-targets", "drop-slots", "drop-secondary"]
        );
    }
}
