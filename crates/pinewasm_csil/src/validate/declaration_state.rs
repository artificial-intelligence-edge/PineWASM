//! Declaration visits with immutable context and the caller's explicit state.
use crate::CsilProgram;

pub(super) trait TaDeclaration<S> {
    fn observe(&self, state: &mut S, index: usize);
}
pub(super) trait InputDeclaration<S> {
    fn observe(&self, state: &mut S, index: usize);
}

pub(super) fn ta_states<C: TaDeclaration<S>, S>(program: &CsilProgram, state: &mut S, context: &C) {
    let mut index = 0;
    while index < program.ta_states.len() {
        context.observe(state, index);
        index += 1;
    }
}

pub(super) fn inputs<C: InputDeclaration<S>, S>(program: &CsilProgram, state: &mut S, context: &C) {
    let mut index = 0;
    while index < program.inputs.len() {
        context.observe(state, index);
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_hir::{TaFunc, TaStateDecl};

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    struct Never;
    impl TaDeclaration<Vec<String>> for Never {
        fn observe(&self, _: &mut Vec<String>, _: usize) {
            panic!("unexpected TA visit");
        }
    }
    impl InputDeclaration<Vec<String>> for Never {
        fn observe(&self, _: &mut Vec<String>, _: usize) {
            panic!("unexpected input visit");
        }
    }
    #[test]
    fn empty_actual_tables_preserve_original_state_without_callback() {
        let program = compile("//@version=6\nindicator(\"empty declarations\")\nplot(close)");
        assert!(program.ta_states.is_empty());
        assert!(program.inputs.is_empty());
        let mut state = Vec::with_capacity(11);
        state.push("prior χ\0diagnostic".to_string());
        let pointer = state.as_ptr();
        let capacity = state.capacity();
        ta_states(&program, &mut state, &Never);
        inputs(&program, &mut state, &Never);
        assert_eq!(state, ["prior χ\0diagnostic"]);
        assert_eq!(state.as_ptr(), pointer);
        assert_eq!(state.capacity(), capacity);
    }
    struct Visit {
        panic_at: Option<usize>,
    }
    impl TaDeclaration<Vec<usize>> for Visit {
        fn observe(&self, state: &mut Vec<usize>, index: usize) {
            assert_ne!(Some(index), self.panic_at, "TA callback failure");
            state.push(index);
        }
    }
    impl InputDeclaration<Vec<usize>> for Visit {
        fn observe(&self, state: &mut Vec<usize>, index: usize) {
            assert_ne!(Some(index), self.panic_at, "input callback failure");
            state.push(index);
        }
    }
    #[test]
    fn actual_nonempty_inventory_order_and_panic_prefix_are_preserved() {
        let mut program = compile("//@version=6\nindicator(\"declarations\")\na = input.bool(true, \"a\")\nb = input.int(14, \"b\")\nplot(close)");
        // Real compiled input inventory; explicit TA rows exercise the generic
        // visitation interface instead of a compiled TA table.
        assert_eq!(program.inputs.len(), 2);
        program.ta_states = vec![
            TaStateDecl {
                func: TaFunc::Sma,
                params: vec![14]
            };
            3
        ];
        let mut ta = vec![99];
        ta_states(&program, &mut ta, &Visit { panic_at: None });
        assert_eq!(ta, [99, 0, 1, 2]);
        let mut input = vec![99];
        inputs(&program, &mut input, &Visit { panic_at: None });
        assert_eq!(input, [99, 0, 1]);
        for ta_scan in [false, true] {
            let mut state = vec![99];
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if ta_scan {
                    ta_states(&program, &mut state, &Visit { panic_at: Some(1) });
                } else {
                    inputs(&program, &mut state, &Visit { panic_at: Some(1) });
                }
            }));
            assert!(failed.is_err());
            assert_eq!(state, [99, 0]);
        }
    }
}
