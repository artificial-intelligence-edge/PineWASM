use super::*;
use crate::validate::*;
/// Test-only copy of the validator body, used as the expected value in `differential`.
fn expected_validate_function(
    program: &CsilProgram,
    func: &crate::CsilFunction,
    errors: &mut Vec<ValidationError>,
) {
    let name = func.name.clone();
    let code_len = func.code.len();

    if code_len == 0 {
        errors.push(ValidationError::Structure {
            function: name,
            message: "function body is empty".to_string(),
        });
        return;
    }
    if func.source_map.len() != code_len {
        errors.push(ValidationError::Structure {
            function: name.clone(),
            message: format!(
                "source map has {} entries for {} instructions",
                func.source_map.len(),
                code_len
            ),
        });
    }

    // Structural bounds for every instruction, reachable or not.
    struct DiagnosticSink<'a> {
        name: &'a str,
        errors: &'a mut Vec<ValidationError>,
    }
    impl function_structure::StructureIssueSink for DiagnosticSink<'_> {
        fn emit(&mut self, offset: usize, issue: function_structure::StructureIssue) {
            let offset = instruction_offset(offset);
            let message = issue.message();
            self.errors.push(ValidationError::AtInstruction {
                function: self.name.to_owned(),
                offset,
                message,
            });
        }
    }
    let structural_ok = function_structure::structural_issues(
        &mut DiagnosticSink {
            name: &name,
            errors,
        },
        program,
        func,
    );
    // Over-declaration is rejected: hosts size buffers from this field, so an
    // adversarial program must not be able to request a huge allocation the
    // code never uses. Checked HERE, before the abstract interpretation
    // below, because that pass allocates one stack snapshot per pc and is
    // quadratic in code length — a program that is going to be rejected on a
    // field readable up front should not first be paid for.
    if func.max_stack > MAX_DECLARED_STACK {
        errors.push(ValidationError::Structure {
            function: name,
            message: format!(
                "declared max_stack {} exceeds the limit of {MAX_DECLARED_STACK}",
                func.max_stack
            ),
        });
        return;
    }

    if !structural_ok {
        return; // The stack simulation assumes in-range operands.
    }

    // Preflight stack *heights* before allocating a typed stack snapshot for
    // every instruction. The typed pass below is intentionally precise, but
    // retaining all of its vectors costs the sum of reachable stack heights.
    // A compact artifact can otherwise encode a long push-then-pop triangle
    // and force quadratic allocation before `max_stack` is rechecked.
    if let Err(error) = preflight_stack_heights(program, func, &name) {
        errors.push(error);
        return;
    }

    // Abstract interpretation over all paths: the typed worklist kernel
    // [`typed_stacks`], then the rendering of its outcome.
    let ta_arity = ta_arity_table(&program.ta_states);
    let tables = ProgramTables { program };
    let outcome = typed_stacks(&tables, &ta_arity, &func.code);
    let mut typed_errors = Vec::new();
    // `ReadSecurityExpr` visits in visit order: each one checks its `HtfExpr`
    // (a non-fatal error, reported once per visit, before any fatal one).
    for &pc in &outcome.htf_visits {
        if let CsilInstr::ReadSecurityExpr { expr, .. } = &func.code[pc] {
            if let Err(message) = validate_htf_expr(
                expr,
                count_htf_ta_nodes(expr),
                count_htf_history_nodes(expr),
                &program.inputs,
            ) {
                typed_errors.push(ValidationError::AtInstruction {
                    function: name.clone(),
                    offset: instruction_offset(pc),
                    message,
                });
            }
        }
    }
    let typed_result = match outcome.result {
        Ok(seen) => {
            Ok(u32::try_from(seen).expect("the preflighted abstract stack ceiling fits in u32"))
        }
        Err(error) => {
            typed_errors.push(typed_error(&name, error, program.ta_states.len()));
            Err(())
        }
    };
    #[cfg(test)]
    typed_diff_tests::assert_typed_matches_baseline(
        program,
        func,
        &name,
        &typed_errors,
        typed_result,
    );
    errors.append(&mut typed_errors);
    let Ok(max_stack_seen) = typed_result else {
        return;
    };

    if max_stack_seen > func.max_stack {
        errors.push(ValidationError::Structure {
            function: name.clone(),
            message: format!(
                "declared max_stack {} understates actual usage {}",
                func.max_stack, max_stack_seen
            ),
        });
    }
    // NOTE: the MAX_DECLARED_STACK ceiling is checked before the abstract
    // interpretation above, not here — see the comment at that site.
}
fn compiled() -> crate::CsilProgram {
    let parsed =
        pinewasm_parse::parse("//@version=6\nindicator(\"Sink Unicode χ\")\nx = close\nplot(x)\n");
    assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
    let analyzed = pinewasm_semantics::analyze(&parsed.program);
    assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
    let mut p = crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).unwrap();
    for f in &mut p.functions {
        f.budget_reference = None;
    }
    p
}
fn install(p: &mut crate::CsilProgram, code: Vec<crate::CsilInstr>) {
    let span = p.functions[0].source_map[0];
    p.functions[0].source_map = vec![span; code.len()];
    p.functions[0].code = code;
    p.functions[0].max_stack = 1;
}
fn differential(p: &crate::CsilProgram) -> Vec<ValidationError> {
    let f = &p.functions[0];
    let mut expected = vec![ValidationError::Header("existing accumulator".into())];
    expected_validate_function(p, f, &mut expected);
    let mut observed = vec![ValidationError::Header("existing accumulator".into())];
    validate_function(p, f, &mut observed);
    assert_eq!(observed, expected);
    assert_eq!(f.source_map, p.functions[0].source_map);
    observed.into_iter().skip(1).collect()
}
#[test]
fn exact_diagnostics_for_each_production_phase() {
    use crate::CsilInstr::*;
    let mut p = compiled();
    assert!(differential(&p).is_empty());
    let original = p.clone();
    install(&mut p, vec![]);
    assert!(
        matches!(&differential(&p)[0],ValidationError::Structure{message,..} if message == "function body is empty")
    );
    p = original.clone();
    p.functions[0].source_map.clear();
    assert!(
        matches!(&differential(&p)[0],ValidationError::Structure{message,..} if message.starts_with("source map has 0 entries"))
    );
    p = original.clone();
    install(
        &mut p,
        vec![
            ReadUserSlot {
                slot: u32::MAX,
                offset: 0,
            },
            Pop,
            Return,
        ],
    );
    assert!(
        matches!(&differential(&p)[0],ValidationError::AtInstruction{offset:0,message,..} if message.contains("series slot"))
    );
    p = original.clone();
    install(&mut p, vec![Pop, Return]);
    assert!(
        matches!(&differential(&p)[0],ValidationError::AtInstruction{offset:0,message,..} if message.contains("stack underflow"))
    );
    p = original.clone();
    install(
        &mut p,
        vec![PushBool(true), PushBool(false), IntAdd, Pop, Return],
    );
    assert!(
        matches!(&differential(&p)[0],ValidationError::AtInstruction{offset:2,message,..} if message.contains("operand"))
    );
    p = original.clone();
    p.functions[0].max_stack = 0;
    assert!(
        matches!(&differential(&p)[0],ValidationError::Structure{message,..} if message.contains("understates actual usage"))
    );
    p = original.clone();
    p.functions[0].max_stack = MAX_DECLARED_STACK + 1;
    assert!(
        matches!(&differential(&p)[0],ValidationError::Structure{message,..} if message.contains("exceeds the limit"))
    );
}
#[test]
fn malformed_order_and_unreachable_rows_are_preserved() {
    use crate::CsilInstr::*;
    let mut p = compiled();
    install(
        &mut p,
        vec![
            Return,
            ReadUserSlot {
                slot: u32::MAX,
                offset: 0,
            },
        ],
    );
    p.functions[0].source_map.clear();
    p.functions[0].max_stack = MAX_DECLARED_STACK + 1;
    let errors = differential(&p);
    assert_eq!(errors.len(), 3);
    assert!(
        matches!(&errors[0],ValidationError::Structure{message,..} if message.starts_with("source map"))
    );
    assert!(matches!(
        &errors[1],
        ValidationError::AtInstruction { offset: 1, .. }
    ));
    assert!(
        matches!(&errors[2],ValidationError::Structure{message,..} if message.contains("exceeds the limit"))
    );
}
#[test]
fn happy_core_emits_no_tags() {
    struct RejectSink;
    impl Sink for RejectSink {
        fn emit(&mut self, _: Issue) {
            panic!("unexpected diagnostic emission")
        }
    }
    let p = compiled();
    validate_function_with_sink(&p, &p.functions[0], &mut RejectSink);
}
#[test]
fn htf_diagnostic_precedes_fatal_typed_error() {
    use crate::CsilInstr::*;
    let mut p = compiled();
    install(
        &mut p,
        vec![
            ReadSecurityExpr {
                expr: pinewasm_hir::HtfExpr::InputSource { input: u32::MAX },
                htf_seconds: 60,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            },
            PushBool(true),
            IntAdd,
            Pop,
            Return,
        ],
    );
    let errors = differential(&p);
    assert_eq!(errors.len(), 2);
    assert!(
        matches!(&errors[0],ValidationError::AtInstruction{offset:0,message,..} if message.contains("source input"))
    );
    assert!(
        matches!(&errors[1],ValidationError::AtInstruction{offset:2,message,..} if message.contains("operand"))
    );
}
#[test]
fn fixed_seed_malformed_code_matches_old_wrapper() {
    use crate::CsilInstr::*;
    let base = compiled();
    let mut seed = 0x12345678u32;
    let mut accepted = 0;
    let mut rejected = 0;
    for _ in 0..384 {
        let mut p = base.clone();
        let mut code = Vec::new();
        for _ in 0..8 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            code.push(match (seed >> 24) % 8 {
                0 => ReadBuiltin {
                    source: pinewasm_hir::BuiltinSeries::Close,
                    offset: 0,
                },
                1 => PushBool(true),
                2 => Pop,
                3 => IntAdd,
                4 => Return,
                5 => ReadUserSlot {
                    slot: u32::MAX,
                    offset: 0,
                },
                6 => PlotF64 { plot_slot: 0 },
                _ => PushNa,
            });
        }
        install(&mut p, code);
        p.functions[0].max_stack = 8;
        if differential(&p).is_empty() {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }
    assert!(
        accepted > 0 && rejected > 0,
        "seed set must produce both accepted and rejected programs"
    );
}
#[test]
fn empty_visits_never_call_htf_policy() {
    struct PanicPolicy;
    impl crate::validate::function_operations::HtfPolicy for PanicPolicy {
        fn validate(&self, _: &pinewasm_hir::HtfExpr) -> Result<(), String> {
            panic!("HTF callback must not be visited");
        }
    }
    let p = compiled();
    let tables = ProgramTables { program: &p };
    let mut errors = Vec::new();
    let mut sink = VecSink {
        name: &p.functions[0].name,
        state_count: p.ta_states.len(),
        errors: &mut errors,
    };
    validate_function_with_operations(
        &p,
        &p.functions[0],
        &mut sink,
        &tables,
        &p.ta_states,
        &PanicPolicy,
        &function_operations::ProductionStackNarrowFailure,
    );
    assert!(errors.is_empty());
}
#[test]
fn visited_htf_policy_failure_and_panic_are_not_swallowed() {
    use crate::CsilInstr::*;
    use std::cell::Cell;
    struct FailPolicy(Cell<usize>);
    impl crate::validate::function_operations::HtfPolicy for FailPolicy {
        fn validate(&self, _: &pinewasm_hir::HtfExpr) -> Result<(), String> {
            self.0.set(self.0.get() + 1);
            Err("request.security secondary-context source input 4294967295 is invalid".into())
        }
    }
    struct PanicPolicy;
    impl crate::validate::function_operations::HtfPolicy for PanicPolicy {
        fn validate(&self, _: &pinewasm_hir::HtfExpr) -> Result<(), String> {
            panic!("visited HTF callback");
        }
    }
    let mut p = compiled();
    install(
        &mut p,
        vec![
            ReadSecurityExpr {
                expr: pinewasm_hir::HtfExpr::InputSource { input: u32::MAX },
                htf_seconds: 60,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            },
            Pop,
            Return,
        ],
    );
    let tables = ProgramTables { program: &p };
    let policy = FailPolicy(Cell::new(0));
    let mut errors = Vec::new();
    {
        let mut sink = VecSink {
            name: &p.functions[0].name,
            state_count: p.ta_states.len(),
            errors: &mut errors,
        };
        validate_function_with_operations(
            &p,
            &p.functions[0],
            &mut sink,
            &tables,
            &p.ta_states,
            &policy,
            &function_operations::ProductionStackNarrowFailure,
        );
    }
    assert_eq!(policy.0.get(), 1);
    assert_eq!(errors, differential(&p));
    assert!(
        matches!(&errors[0],ValidationError::AtInstruction{offset:0,message,..} if message.contains("source input"))
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut sink = VecSink {
            name: &p.functions[0].name,
            state_count: p.ta_states.len(),
            errors: &mut errors,
        };
        validate_function_with_operations(
            &p,
            &p.functions[0],
            &mut sink,
            &tables,
            &p.ta_states,
            &PanicPolicy,
            &function_operations::ProductionStackNarrowFailure,
        );
    }));
    assert!(panic.is_err());
}

#[test]
fn stack_narrowing_preserves_original_and_never_calls_failure_when_fit() {
    struct PanicFailure;
    impl function_operations::StackNarrowFailure for PanicFailure {
        fn on_overflow(&self, _seen: usize) -> u32 {
            panic!("unexpected overflow callback")
        }
    }
    for seen in [0usize, 1, 65_536, u32::MAX as usize] {
        let expected =
            u32::try_from(seen).expect("the preflighted abstract stack ceiling fits in u32");
        assert_eq!(narrow_preflighted_stack(seen, &PanicFailure), expected);
        assert_eq!(
            narrow_preflighted_stack(seen, &function_operations::ProductionStackNarrowFailure),
            expected
        );
    }
    #[cfg(target_pointer_width = "64")]
    {
        let seen = u32::MAX as usize + 1;
        let original = std::panic::catch_unwind(|| {
            u32::try_from(seen).expect("the preflighted abstract stack ceiling fits in u32")
        })
        .unwrap_err();
        let actual = std::panic::catch_unwind(|| {
            narrow_preflighted_stack(seen, &function_operations::ProductionStackNarrowFailure)
        })
        .unwrap_err();
        assert!(actual.is::<String>());
        assert!(original.is::<String>());
        assert_eq!(
            actual.downcast_ref::<String>(),
            original.downcast_ref::<String>()
        );
    }
}
