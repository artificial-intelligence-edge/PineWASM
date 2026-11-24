//! TA call-site records with immutable context and explicit mutable state.
use super::{instruction_offset, ValidationError};
use crate::{CsilInstr, CsilProgram};

pub(super) trait TaUser<U, E> {
    fn observe(&self, users: &mut U, errors: &mut E, function: usize, index: usize, state: u32);
}
pub(super) fn ta_users<C: TaUser<U, E>, U, E>(
    program: &CsilProgram,
    users: &mut U,
    errors: &mut E,
    context: &C,
) {
    let mut function = 0;
    while function < program.functions.len() {
        let code = &program.functions[function].code;
        let mut offset = 0;
        while offset < code.len() {
            if let CsilInstr::CallTa { state } = &code[offset] {
                context.observe(users, errors, function, offset, *state);
            }
            offset += 1;
        }
        function += 1;
    }
}
pub(super) type Users = Vec<Option<(String, u32)>>;
/// The users allocation is released here, before the public function stage.
pub(super) fn run<C: TaUser<Users, E>, E>(program: &CsilProgram, errors: &mut E, context: &C) {
    let mut users = vec![None; program.ta_states.len()];
    ta_users(program, &mut users, errors, context);
}
struct ProgramContext<'a> {
    program: &'a CsilProgram,
}
impl TaUser<Users, Vec<ValidationError>> for ProgramContext<'_> {
    fn observe(
        &self,
        users: &mut Users,
        errors: &mut Vec<ValidationError>,
        function: usize,
        index: usize,
        state: u32,
    ) {
        let func = &self.program.functions[function];
        let offset = instruction_offset(index);
        if let Some(user) = users.get_mut(state as usize) {
            match user {
                None => *user = Some((func.name.clone(), offset)),
                Some((first_func, first_offset)) => {
                    errors.push(ValidationError::AtInstruction {
                        function: func.name.clone(),
                        offset,
                        message: format!(
                            "ta state {state} is already used by `{first_func}` at \
                                     {first_offset:04}; each state id belongs to exactly one \
                                     call site"
                        ),
                    });
                }
            }
        }
        // Out-of-range states are reported by the structural pass.
    }
}
pub(super) fn check(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    run(program, errors, &ProgramContext { program });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_hir::{TaFunc, TaStateDecl};
    fn compile() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"users seam\")\nplot(close)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    struct Never;
    impl TaUser<Users, Vec<String>> for Never {
        fn observe(&self, _: &mut Users, _: &mut Vec<String>, _: usize, _: usize, _: u32) {
            panic!("unexpected TA-user callback");
        }
    }
    #[test]
    fn real_empty_table_and_excluded_rows_preserve_error_allocation() {
        let program = compile();
        assert!(program.ta_states.is_empty());
        let mut errors = Vec::with_capacity(9);
        errors.push("prior χ\0diagnostic".into());
        let pointer = errors.as_ptr();
        let capacity = errors.capacity();
        run(&program, &mut errors, &Never);
        assert_eq!(errors, ["prior χ\0diagnostic"]);
        assert_eq!(errors.as_ptr(), pointer);
        assert_eq!(errors.capacity(), capacity);
    }
    struct Observe {
        panic_at: Option<usize>,
    }
    impl TaUser<Users, Vec<(usize, usize, u32, usize)>> for Observe {
        fn observe(
            &self,
            users: &mut Users,
            errors: &mut Vec<(usize, usize, u32, usize)>,
            function: usize,
            offset: usize,
            state: u32,
        ) {
            assert_ne!(
                Some(errors.len()),
                self.panic_at,
                "TA-user callback failure"
            );
            errors.push((function, offset, state, users.len()));
            if let Some(user) = users.get_mut(state as usize) {
                *user = Some((
                    "changed".into(),
                    u32::try_from(offset).expect("bounded test offset"),
                ));
            }
        }
    }
    #[test]
    fn two_mutable_states_dead_tails_and_panic_prefix_are_preserved() {
        let mut program = compile();
        program.ta_states = vec![
            TaStateDecl {
                func: TaFunc::Sma,
                params: vec![14]
            };
            2
        ];
        program.functions[0].code = vec![
            CsilInstr::Return,
            CsilInstr::CallTa { state: 0 },
            CsilInstr::CallTa { state: u32::MAX },
        ];
        let mut second = program.functions[0].clone();
        second.code = vec![CsilInstr::CallTa { state: 1 }];
        program.functions.push(second);
        let sentinel = (99, 99, 99, 99);
        let mut errors = vec![sentinel];
        run(&program, &mut errors, &Observe { panic_at: None });
        assert_eq!(
            errors,
            [sentinel, (0, 1, 0, 2), (0, 2, u32::MAX, 2), (1, 0, 1, 2)]
        );
        let mut users = vec![None; 2];
        let mut errors = vec![sentinel];
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ta_users(
                &program,
                &mut users,
                &mut errors,
                &Observe { panic_at: Some(2) },
            );
        }));
        assert!(failed.is_err());
        assert_eq!(errors, [sentinel, (0, 1, 0, 2)]);
        assert_eq!(users, [Some(("changed".into(), 1)), None]);
    }
}
