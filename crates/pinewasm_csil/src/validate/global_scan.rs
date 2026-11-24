//! Production global observations with owned callback state and immutable code.
use crate::{CsilInstr, CsilProgram};
use pinewasm_hir::{HtfExpr, TaInputTarget};

pub(super) trait Secondary: Sized {
    fn observe(self, expr: &HtfExpr) -> Self;
}
pub(super) trait Binding: Sized {
    fn observe(self, target: TaInputTarget, value: u32) -> Self;
}
#[cfg(test)]
pub(super) trait TaUser: Sized {
    fn observe(self, function: usize, offset: usize, state: u32) -> Self;
}

fn secondary_row<S: Secondary>(row: &CsilInstr, state: S) -> S {
    match row {
        CsilInstr::ReadSecurityExpr { expr, .. } => state.observe(expr),
        _ => state,
    }
}
fn binding_row<S: Binding>(row: &CsilInstr, state: S) -> S {
    match row {
        CsilInstr::WindowMean { slot, length } => {
            state.observe(TaInputTarget::Sma { slot: *slot }, *length)
        }
        CsilInstr::ReadUserSlot { slot, offset } => {
            if *offset != 0 {
                state.observe(TaInputTarget::Change { slot: *slot }, *offset)
            } else {
                state
            }
        }
        _ => state,
    }
}
pub(super) fn secondary<S: Secondary>(program: &CsilProgram, mut state: S) -> S {
    let mut function = 0;
    while function < program.functions.len() {
        let code = &program.functions[function].code;
        let mut offset = 0;
        while offset < code.len() {
            state = secondary_row(&code[offset], state);
            offset += 1;
        }
        function += 1;
    }
    state
}

pub(super) fn bindings<S: Binding>(program: &CsilProgram, mut state: S) -> S {
    let mut function = 0;
    while function < program.functions.len() {
        let code = &program.functions[function].code;
        let mut offset = 0;
        while offset < code.len() {
            state = binding_row(&code[offset], state);
            offset += 1;
        }
        function += 1;
    }
    state
}

#[cfg(test)]
pub(super) fn ta_users<S: TaUser>(program: &CsilProgram, state: S) -> S {
    struct Context;
    impl<S: TaUser> super::ta_user_state::TaUser<Option<S>, ()> for Context {
        fn observe(
            &self,
            state: &mut Option<S>,
            (): &mut (),
            function: usize,
            offset: usize,
            id: u32,
        ) {
            *state = Some(
                state
                    .take()
                    .expect("owned TA-user state")
                    .observe(function, offset, id),
            );
        }
    }
    let mut state = Some(state);
    super::ta_user_state::ta_users(program, &mut state, &mut (), &Context);
    state.expect("owned TA-user state")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, PartialEq)]
    enum Event {
        Secondary(HtfExpr),
        Binding(TaInputTarget, u32),
        User(usize, usize, u32),
    }
    #[derive(Debug, PartialEq)]
    struct Trace {
        sentinel: u64,
        events: Vec<Event>,
    }
    impl Secondary for Trace {
        fn observe(mut self, expr: &HtfExpr) -> Self {
            self.events.push(Event::Secondary(expr.clone()));
            self
        }
    }
    impl Binding for Trace {
        fn observe(mut self, target: TaInputTarget, value: u32) -> Self {
            self.events.push(Event::Binding(target, value));
            self
        }
    }
    impl TaUser for Trace {
        fn observe(mut self, function: usize, offset: usize, state: u32) -> Self {
            self.events.push(Event::User(function, offset, state));
            self
        }
    }
    fn compile() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"scan\")\nplot(close)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    fn trace() -> Trace {
        Trace {
            sentinel: 987654321,
            events: vec![],
        }
    }
    fn baseline_secondary(p: &CsilProgram, mut s: Trace) -> Trace {
        for f in &p.functions {
            for i in &f.code {
                if let CsilInstr::ReadSecurityExpr { expr, .. } = i {
                    s.events.push(Event::Secondary(expr.clone()));
                }
            }
        }
        s
    }
    fn baseline_bindings(p: &CsilProgram, mut s: Trace) -> Trace {
        for f in &p.functions {
            for i in &f.code {
                match i {
                    CsilInstr::WindowMean { slot, length } => s
                        .events
                        .push(Event::Binding(TaInputTarget::Sma { slot: *slot }, *length)),
                    CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => s.events.push(
                        Event::Binding(TaInputTarget::Change { slot: *slot }, *offset),
                    ),
                    _ => {}
                }
            }
        }
        s
    }
    fn baseline_users(p: &CsilProgram, mut s: Trace) -> Trace {
        for (f, func) in p.functions.iter().enumerate() {
            for (pc, i) in func.code.iter().enumerate() {
                if let CsilInstr::CallTa { state } = i {
                    s.events.push(Event::User(f, pc, *state));
                }
            }
        }
        s
    }
    #[test]
    fn complete_ordered_inventory_matches_legacy_including_dead_tails_and_invalid_ids() {
        let mut p = compile();
        let mut f = p.functions[0].clone();
        f.code = vec![
            CsilInstr::Return,
            CsilInstr::WindowMean { slot: 9, length: 0 },
            CsilInstr::ReadUserSlot { slot: 3, offset: 0 },
            CsilInstr::ReadUserSlot {
                slot: 3,
                offset: u32::MAX,
            },
            CsilInstr::CallTa { state: u32::MAX },
            CsilInstr::ReadSecurityExpr {
                expr: HtfExpr::BoolResult(Box::new(HtfExpr::Const(4.5))),
                htf_seconds: -1,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on: true,
                lookahead_on: true,
            },
        ];
        p.functions = vec![f.clone(), f];
        assert_eq!(secondary(&p, trace()), baseline_secondary(&p, trace()));
        assert_eq!(bindings(&p, trace()), baseline_bindings(&p, trace()));
        assert_eq!(ta_users(&p, trace()), baseline_users(&p, trace()));
        assert_eq!(bindings(&p, trace()).events.len(), 4);
        assert_eq!(
            ta_users(&p, trace()).events,
            vec![Event::User(0, 4, u32::MAX), Event::User(1, 4, u32::MAX)]
        );
    }
    #[test]
    fn seeded_inventory_traces_match_independent_iterators() {
        let base = compile();
        let rows = [
            CsilInstr::Return,
            CsilInstr::WindowMean { slot: 2, length: 8 },
            CsilInstr::ReadUserSlot { slot: 2, offset: 0 },
            CsilInstr::ReadUserSlot { slot: 2, offset: 5 },
            CsilInstr::CallTa { state: 0 },
            CsilInstr::CallTa { state: u32::MAX },
            CsilInstr::ReadSecurityExpr {
                expr: HtfExpr::Const(-0.0),
                htf_seconds: 60,
                timeframe_expr: Some("dynamic".into()),
                calendar_months: Some(1),
                gaps_on: false,
                lookahead_on: false,
            },
        ];
        for seed in 0..64usize {
            let mut p = base.clone();
            p.functions = (0..seed % 4)
                .map(|function| {
                    let mut f = base.functions[0].clone();
                    f.name = format!("f{function}");
                    f.code = (0..seed % 11)
                        .map(|offset| rows[(seed + function * 3 + offset * 5) % rows.len()].clone())
                        .collect();
                    f
                })
                .collect();
            assert_eq!(secondary(&p, trace()), baseline_secondary(&p, trace()));
            assert_eq!(bindings(&p, trace()), baseline_bindings(&p, trace()));
            assert_eq!(ta_users(&p, trace()), baseline_users(&p, trace()));
        }
    }
    #[test]
    fn production_user_diagnostics_preserve_first_site_and_order() {
        let mut p = compile();
        p.ta_states = vec![pinewasm_hir::TaStateDecl {
            func: crate::TaFunc::Ema,
            params: vec![3],
        }];
        let mut first = p.functions[0].clone();
        first.name = "first".into();
        first.code = vec![
            CsilInstr::Return,
            CsilInstr::CallTa { state: 0 },
            CsilInstr::CallTa { state: u32::MAX },
        ];
        let mut second = first.clone();
        second.name = "second".into();
        second.code = vec![
            CsilInstr::CallTa { state: 0 },
            CsilInstr::Return,
            CsilInstr::CallTa { state: 0 },
        ];
        p.functions = vec![first, second];
        let errors = super::super::validate(&p).expect_err("malformed program");
        let duplicates: Vec<_> = errors
            .iter()
            .filter_map(|error| match error {
                super::super::ValidationError::AtInstruction {
                    function,
                    offset,
                    message,
                } if message.contains("already used") => {
                    Some((function.as_str(), *offset, message.as_str()))
                }
                _ => None,
            })
            .collect();
        let message = "ta state 0 is already used by `first` at 0001; each state id belongs to exactly one call site";
        assert_eq!(
            duplicates,
            vec![("second", 0, message), ("second", 2, message)]
        );
    }
    struct NoCalls(u64);
    impl Secondary for NoCalls {
        fn observe(self, _: &HtfExpr) -> Self {
            panic!("unexpected secondary callback")
        }
    }
    impl Binding for NoCalls {
        fn observe(self, _: TaInputTarget, _: u32) -> Self {
            panic!("unexpected binding callback")
        }
    }
    impl TaUser for NoCalls {
        fn observe(self, _: usize, _: usize, _: u32) -> Self {
            panic!("unexpected user callback")
        }
    }
    #[test]
    fn excluded_rows_preserve_arbitrary_state_without_invoking_callbacks() {
        let mut p = compile();
        p.functions[0].code = vec![
            CsilInstr::Return,
            CsilInstr::ReadUserSlot {
                slot: u32::MAX,
                offset: 0,
            },
        ];
        assert_eq!(secondary(&p, NoCalls(7)).0, 7);
        assert_eq!(bindings(&p, NoCalls(8)).0, 8);
        assert_eq!(ta_users(&p, NoCalls(9)).0, 9);
        p.functions.clear();
        assert_eq!(secondary(&p, NoCalls(10)).0, 10);
        assert_eq!(bindings(&p, NoCalls(11)).0, 11);
        assert_eq!(ta_users(&p, NoCalls(12)).0, 12);
    }
}
