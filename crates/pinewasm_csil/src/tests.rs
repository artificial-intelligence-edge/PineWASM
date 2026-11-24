use crate::{
    collection_result_ty, dump_csil, lower_mir, validate, AbstractTy, CollectionFunc, CsilInstr,
    CsilProgram, F64Bits, ValidationError, MAX_VALIDATION_ABSTRACT_CELLS, MAX_WINDOW_LENGTH,
};
use pinewasm_hir::{InputValue, OrderKind};
use pinewasm_parse::parse;
use pinewasm_semantics::analyze;
use pinewasm_span::has_errors;

fn compile_to_csil(source: &str) -> CsilProgram {
    let parsed = parse(source);
    assert!(
        !has_errors(&parsed.diagnostics),
        "parse errors: {:?}",
        parsed.diagnostics
    );
    let analysis = analyze(&parsed.program);
    assert!(
        !has_errors(&analysis.diagnostics),
        "semantic errors: {:?}",
        analysis.diagnostics
    );
    let mir = pinewasm_mir::lower(&analysis.hir);
    let program = lower_mir(&mir);
    // Every program compiled here also checks the Aeneas-extractable passes
    // against baseline implementations kept in the test suite.
    assert_eq!(
        program,
        crate::lower::pass_reference_tests::lower_mir_reference(&mir),
        "optimized CSIL differs from the baseline pipeline"
    );
    // The test-suite baseline stackification lowers it identically (optimized or
    // not, errors and panics included).
    crate::lower::stackify_reference_tests::assert_matches_reference(&mir);
    // Every such program also lowers identically under in-process translation
    // validation of the pair-erasure passes (the checker accepts every pass).
    assert_eq!(
        crate::lower_mir_validated(&mir),
        program,
        "translation validation rejected a pass output"
    );
    program.expect("lowering must succeed for valid programs")
}

const CORE: &str = "//@version=6\nindicator(\"Example\")\nx = close - close[1]\nplot(x)\n";

#[test]
fn matrix_structural_line_calls_push_reference_values() {
    use crate::CollectionFunc as F;

    for func in [
        F::MatrixRow,
        F::MatrixCol,
        F::MatrixRemoveRow,
        F::MatrixRemoveCol,
    ] {
        assert_eq!(collection_result_ty(func), AbstractTy::Ref, "{func:?}");
    }
}

#[test]
fn core_example_csil_snapshot() {
    let program = compile_to_csil(CORE);
    validate(&program).expect("core example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn if_else_csil_snapshot() {
    let program = compile_to_csil(
        "indicator(\"Branch\")\nvalue = 0.0\nif close > open\n    value := close\nelse\n    value := open\nplot(value)\n",
    );
    validate(&program).expect("branch example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn int_arithmetic_csil_snapshot() {
    // Int literals and `%` (INT_REM), int division (`7 / 2` lowers to
    // PINE_INT_DIV with a fractional runtime value), and explicit widening to
    // float in both branches.
    let program = compile_to_csil(
        "indicator(\"Ints\")\nm = bar_index % 2\nq = 7 / 2\nvalue = 0.0\nif m == 0\n    value := q\nelse\n    value := q + 0.5\nplot(value)\n",
    );
    validate(&program).expect("int example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn lazy_bool_csil_snapshot() {
    // `and`/`or` lower lazily: each lhs is DUP'd and a JUMP_IF_EXACT_*
    // (true for `or`, false for `and`) skips the rhs instructions plus the
    // combining BOOL_* when the lhs alone decides, leaving the retained lhs
    // copy as the result. Nested guards close innermost-first.
    let program = compile_to_csil(
        "indicator(\"Lazy\")\nsignal = close > open and (close > close[1] or volume > volume[1])\nplot(signal ? 1.0 : 0.0)\n",
    );
    validate(&program).expect("lazy bool example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn var_counter_csil_snapshot() {
    // The persistent slot is dumped with a `var` marker and the declaration
    // lowers to JUMP_IF_SLOT_WRITTEN over the initializer.
    let program = compile_to_csil("indicator(\"Var\")\nvar n = 0\nn := n + 1\nplot(n)\n");
    validate(&program).expect("var example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn compound_history_csil_snapshot() {
    // The hidden slot backing `(close - open)[1]` shows up in the slot
    // table as `%h0` (non-persistent), and the site lowers to the existing
    // STORE_USER_SLOT_CURRENT / READ_USER_SLOT pair — no new opcodes.
    let program = compile_to_csil("indicator(\"Compound\")\nx = (close - open)[1]\nplot(x)\n");
    validate(&program).expect("compound history example must validate");
    assert_eq!(program.series_slots[0].name, "%h0");
    assert!(!program.series_slots[0].persistent);
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn ta_change_sma_csil_snapshot() {
    // ta.change lowers to STORE_USER_SLOT_CURRENT + two READ_USER_SLOTs +
    // F64_SUB; ta.sma to STORE_USER_SLOT_CURRENT + WINDOW_MEAN. One hidden
    // slot per call site.
    let program =
        compile_to_csil("indicator(\"TA\")\nplot(ta.change(close))\nplot(ta.sma(close, 2))\n");
    validate(&program).expect("ta example must validate");
    assert_eq!(program.series_slots[0].name, "%h0");
    assert_eq!(program.series_slots[1].name, "%h1");
    assert!(!program.series_slots[0].persistent);
    assert!(!program.series_slots[1].persistent);
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn ta_program_round_trips_through_json() {
    let program = compile_to_csil("indicator(\"TA\")\nplot(ta.sma(close, 100000))\n");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "WINDOW_MEAN operands must survive JSON");
}

#[test]
fn var_program_round_trips_through_json() {
    let program = compile_to_csil("indicator(\"VarRT\")\nvar n = 0\nn := n + 1\nplot(n)\n");
    assert!(program.series_slots[0].persistent);
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "SlotDef persistence must survive JSON");
}

#[test]
fn int_program_round_trips_through_json() {
    let program =
        compile_to_csil("indicator(\"RT\")\nx = bar_index % 2 + 9223372036854775807\nplot(x)\n");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "PUSH_I64 operands must survive JSON exactly");
}

#[test]
fn complete_strategy_order_surface_lowers_validates_and_round_trips() {
    let program = compile_to_csil(
        r#"//@version=6
strategy("Orders")
direction = close > open ? strategy.long : strategy.short
strategy.entry("entry", direction, 2, 101.0, 99.0, "entry_oca", strategy.oca.cancel, "entry comment", "entry alert", true)
strategy.order("order", strategy.short, 3, 98.0, 102.0, "order_oca", strategy.oca.reduce, "order comment", "order alert", false)
strategy.exit("exit", "entry", 1, 50, 10, 110.0, 5, 95.0, 105.0, 3, 2, "exit_oca", "exit comment", "profit comment", "loss comment", "trail comment", "exit alert", "profit alert", "loss alert", "trail alert", true)
strategy.close("entry", "close comment", 1, 50, "close alert", true, true)
strategy.close_all("all comment", "all alert", true, true)
"#,
    );

    validate(&program).expect("the complete strategy order surface must validate");

    let expected = [
        (OrderKind::Entry, 10),
        (OrderKind::Order, 10),
        (OrderKind::Exit, 21),
        (OrderKind::Close, 7),
        (OrderKind::CloseAll, 4),
    ];
    let layouts = program
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .filter_map(|instruction| match instruction {
            CsilInstr::Order { kind } => Some((*kind, kind.operand_count())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(layouts, expected);

    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize complete order program");
    assert_eq!(program, back, "order instructions must survive CSIL JSON");
}

#[test]
fn risk_orders_lower_to_an_unconditional_csil_prelude() {
    let program = compile_to_csil(
        r#"//@version=6
strategy("Global risk")
if false
    cap = 3.0
    strategy.risk.max_position_size(cap)
    strategy.risk.allow_entry_in(strategy.direction.long)
    strategy.risk.max_drawdown(25, strategy.percent_of_equity)
    strategy.risk.max_intraday_loss(100, strategy.cash)
    strategy.risk.max_intraday_filled_orders(4)
    strategy.risk.max_cons_loss_days(2)
strategy.entry("L", strategy.long, qty = 10)
"#,
    );
    validate(&program).expect("the hoisted risk prelude must validate");

    let code = &program.main().code;
    let risk_orders = code
        .iter()
        .enumerate()
        .filter_map(|(pc, instruction)| match instruction {
            CsilInstr::Order { kind } if kind.is_risk() => Some((pc, *kind)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        risk_orders
            .iter()
            .map(|(_, kind)| *kind)
            .collect::<Vec<_>>(),
        vec![
            OrderKind::RiskMaxPositionSize,
            OrderKind::RiskAllowEntryIn,
            OrderKind::RiskMaxDrawdown,
            OrderKind::RiskMaxIntradayLoss,
            OrderKind::RiskMaxIntradayFilledOrders,
            OrderKind::RiskMaxConsLossDays,
        ]
    );
    let entry_pc = code
        .iter()
        .position(|instruction| {
            matches!(
                instruction,
                CsilInstr::Order {
                    kind: OrderKind::Entry
                }
            )
        })
        .expect("ordinary entry order");
    assert!(
        risk_orders.iter().all(|(pc, _)| *pc < entry_pc),
        "all global risk registrations must precede ordinary order effects"
    );

    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize risk-prelude program");
    assert_eq!(program, back, "the hoisted risk CSIL must survive JSON");
}

#[test]
fn validator_rejects_strategy_order_operand_type_confusion() {
    let mut program = compile_to_csil(
        "//@version=6\nstrategy(\"Orders\")\nstrategy.entry(\"entry\", strategy.long)\n",
    );
    let main = &mut program.functions[0];
    let order_index = main
        .code
        .iter()
        .position(|instruction| {
            matches!(
                instruction,
                CsilInstr::Order {
                    kind: OrderKind::Entry
                }
            )
        })
        .expect("entry instruction");
    let direction_index = order_index - 9;
    assert!(matches!(main.code[direction_index], CsilInstr::PushStr(_)));
    main.code[direction_index] = CsilInstr::PushF64(F64Bits::new(1.0));

    let errors = validate(&program).expect_err("numeric strategy_direction must be rejected");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("operand 9 must be Str")),
        "{errors:?}"
    );
}

#[test]
fn validator_requires_integer_strategy_risk_counts() {
    for member in ["max_intraday_filled_orders", "max_cons_loss_days"] {
        let mut program = compile_to_csil(&format!(
            "//@version=6\nstrategy(\"Risk count\")\nstrategy.risk.{member}(2)\n"
        ));
        let main = &mut program.functions[0];
        let order_index = main
            .code
            .iter()
            .position(|instruction| {
                matches!(
                    instruction,
                    CsilInstr::Order {
                        kind: OrderKind::RiskMaxIntradayFilledOrders
                            | OrderKind::RiskMaxConsLossDays
                    }
                )
            })
            .expect("risk instruction");
        let count_index = order_index - 2;
        assert!(matches!(main.code[count_index], CsilInstr::PushI64(2)));
        main.code[count_index] = CsilInstr::PushF64(F64Bits::new(2.0));

        let errors = validate(&program).expect_err("floating risk count must be rejected");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("operand 2 must be Int")),
            "{member}: {errors:?}"
        );
    }
}

#[test]
fn source_map_covers_every_instruction() {
    let program = compile_to_csil(CORE);
    let main = program.main();
    assert_eq!(main.code.len(), main.source_map.len());
    // Spans must be within the source text.
    let source_len = CORE.len() as u32;
    for span in &main.source_map {
        assert!(span.end <= source_len);
    }
}

#[test]
fn json_round_trip_preserves_program() {
    let program = compile_to_csil(CORE);
    let json = program.to_json();
    assert!(json.contains("\"il_version\":2"));
    assert!(!json.contains("\"csil_version\""));
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back);

    let canonical_alias = json.replace("\"il_version\"", "\"csil_version\"");
    let back = CsilProgram::from_json(&canonical_alias).expect("deserialize canonical alias");
    assert_eq!(program, back);
}

#[test]
fn boolean_security_expression_round_trips_and_validates() {
    let program = compile_to_csil(
        "indicator(\"Requested bool\")\nlastBar = request.security(syminfo.tickerid, \"1D\", barstate.islast, lookahead=barmerge.lookahead_on)\nplotshape(lastBar)\n",
    );
    validate(&program).expect("boolean security expression validates");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("boolean security expression deserializes");
    assert_eq!(back, program);
    validate(&back).expect("round-tripped boolean security expression validates");
}

#[test]
fn frozen_csil_v2_json_remains_readable_and_keeps_its_wire_shape() {
    let json = include_str!("fixtures/csil_v2_wire_compatibility.json").trim();
    assert!(json.contains("\"il_version\": 2"));
    assert!(!json.contains("\"csil_version\""));

    let program = CsilProgram::from_json(json).expect("frozen CSIL v2 artifact must decode");
    validate(&program).expect("frozen CSIL v2 artifact must validate");
    assert_eq!(program.header.csil_version, 2);

    let frozen: serde_json::Value = serde_json::from_str(json).unwrap();
    let current: serde_json::Value = serde_json::from_str(&program.to_json()).unwrap();
    assert_eq!(current, frozen);
}

#[test]
fn header_is_versioned() {
    let program = compile_to_csil(CORE);
    assert_eq!(program.header.magic, "CSIL");
    // Version 2 expands the strategy ORDER layouts, so v1 artifacts must not
    // be decoded using the new operand schemas.
    assert_eq!(program.header.csil_version, 2);
    assert_eq!(program.header.language_version, 6);
}

// -- validator rejection tests ------------------------------------------------

fn corrupt(mutator: impl FnOnce(&mut CsilProgram)) -> Vec<ValidationError> {
    let mut program = compile_to_csil(CORE);
    mutator(&mut program);
    validate(&program).expect_err("corrupted program must fail validation")
}

#[test]
fn validator_rejects_bad_magic() {
    let errors = corrupt(|p| p.header.magic = "NOPE".into());
    assert!(matches!(errors[0], ValidationError::Header(_)));
}

#[test]
fn validator_rejects_legacy_and_unknown_versions() {
    for version in [1, 99] {
        let errors = corrupt(|p| p.header.csil_version = version);
        assert!(matches!(errors[0], ValidationError::Header(_)));
    }
}

#[test]
fn validator_rejects_language_and_runtime_abi_skew() {
    let errors = corrupt(|p| p.header.language_version += 1);
    assert!(matches!(errors[0], ValidationError::Header(_)));

    let errors = corrupt(|p| p.header.runtime_abi_version = crate::INDEXED_SCAN_ABI_VERSION + 1);
    assert!(matches!(errors[0], ValidationError::Header(_)));
}

#[test]
fn validator_rejects_out_of_range_series_slot_offset() {
    // A literal `ReadUserSlot` offset is never checked at runtime, and it
    // sizes the slot ring in both execution tiers. Unbounded, it is an
    // allocation an adversarial program chooses; past 2^31 it overflows the
    // sizing arithmetic outright (debug panic / release wrap).
    for offset in [MAX_WINDOW_LENGTH + 1, 1_000_000, 4_000_000_000, u32::MAX] {
        let errors = corrupt(|p| {
            p.functions[0]
                .code
                .insert(0, CsilInstr::ReadUserSlot { slot: 0, offset });
        });
        assert!(
            errors.iter().any(|e| e.to_string().contains("offset")),
            "offset {offset} must be rejected, got {errors:?}"
        );
    }
}

#[test]
fn validator_accepts_series_slot_offset_at_the_limit() {
    // The bound is inclusive, and it is MAX_WINDOW_LENGTH rather than
    // anything tighter precisely so it forbids no ring size that
    // `WindowMean` already permits.
    let mut program = compile_to_csil(CORE);
    program.functions[0].code.insert(
        0,
        CsilInstr::ReadUserSlot {
            slot: 0,
            offset: MAX_WINDOW_LENGTH,
        },
    );
    let errors = validate(&program).err().unwrap_or_default();
    assert!(
        !errors.iter().any(|e| e.to_string().contains("offset")),
        "offset at the limit must not be rejected: {errors:?}"
    );
}

#[test]
fn validator_rejects_stack_underflow() {
    let errors = corrupt(|p| p.functions[0].code.insert(0, CsilInstr::F64Add));
    assert!(errors.iter().any(|e| e.to_string().contains("underflow")));
}

#[test]
fn validator_rejects_out_of_range_jump() {
    let first_out_of_range = u32::try_from(compile_to_csil(CORE).functions[0].code.len() + 1)
        .expect("fixture length including the inserted jump fits u32");
    for target in [first_out_of_range, 100, u32::MAX] {
        for instruction in [
            CsilInstr::Jump { target },
            CsilInstr::JumpIfFalse { target },
            CsilInstr::JumpIfExact { on: true, target },
            CsilInstr::JumpIfSlotWritten { slot: 0, target },
        ] {
            let description = format!("{instruction:?}");
            let errors = corrupt(|p| {
                p.functions[0].code.insert(0, instruction);
                p.functions[0]
                    .source_map
                    .insert(0, pinewasm_span::Span::dummy());
            });
            assert!(
                errors.iter().any(|e| e.to_string().contains("jump target")),
                "out-of-range {description} must report its target: {errors:?}"
            );
        }
    }
}

#[test]
fn validator_rejects_out_of_range_slots() {
    let errors = corrupt(|p| {
        p.functions[0].code.insert(
            0,
            CsilInstr::ReadUserSlot {
                slot: 42,
                offset: 0,
            },
        );
    });
    assert!(errors
        .iter()
        .any(|e| e.to_string().contains("series slot 42")));

    let errors = corrupt(|p| {
        // Replace the plot with an out-of-range plot slot.
        for instr in &mut p.functions[0].code {
            if let CsilInstr::PlotF64 { plot_slot } | CsilInstr::PlotStyledF64 { plot_slot, .. } =
                instr
            {
                *plot_slot = 9;
            }
        }
    });
    assert!(errors.iter().any(|e| e.to_string().contains("plot slot 9")));
}

#[test]
fn validator_rejects_missing_return() {
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        f.code.pop();
        f.source_map.pop();
    });
    assert!(errors
        .iter()
        .any(|e| e.to_string().contains("falls off the end")));
}

#[test]
fn validator_rejects_type_confusion() {
    let errors = corrupt(|p| {
        // Replace the plot operand explicitly; coalescing can remove the
        // temporary slot that used to feed it.
        let f = &mut p.functions[0];
        let pc = f
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::PlotStyledF64 { color_count: 1, .. }))
            .unwrap();
        // The default colour is the final operand. Mutate the numeric value
        // below it, so this remains a price-type rejection test.
        assert!(matches!(f.code[pc - 1], CsilInstr::PushColor { .. }));
        let pc = pc - 1;
        f.code
            .splice(pc..pc, [CsilInstr::Pop, CsilInstr::PushBool(true)]);
        f.source_map
            .splice(pc..pc, [pinewasm_span::Span::dummy(); 2]);
    });
    assert!(errors.iter().any(|e| e.to_string().contains("must be F64")));
}

#[test]
fn validator_rejects_out_of_range_htf_ta_slot() {
    // Regression: a crafted ReadSecurityExpr whose HtfExpr::Ta.ta_slot exceeds
    // the tree's ta-node count must be rejected by the validator, never reach
    // the VM (where it would overflow / index out of bounds / attempt a giant
    // allocation). The analyzer only ever produces dense 0..N slots.
    use pinewasm_hir::HtfExpr;

    fn corrupt_ta_slot(expr: &mut HtfExpr, slot: u32) {
        match expr {
            HtfExpr::BoolResult(inner) => corrupt_ta_slot(inner, slot),
            HtfExpr::EnumSelect {
                when_equal,
                otherwise,
                ..
            } => {
                corrupt_ta_slot(when_equal, slot);
                if let Some(otherwise) = otherwise {
                    corrupt_ta_slot(otherwise, slot);
                }
            }
            HtfExpr::Ta {
                ta_slot, source, ..
            }
            | HtfExpr::TaInput {
                ta_slot, source, ..
            } => {
                *ta_slot = slot;
                corrupt_ta_slot(source, slot);
            }
            HtfExpr::Neg(inner) => corrupt_ta_slot(inner, slot),
            HtfExpr::History { source, .. }
            | HtfExpr::HistoryInput { source, .. }
            | HtfExpr::HistoryRealtime { source, .. } => {
                corrupt_ta_slot(source, slot);
            }
            HtfExpr::Binary { lhs, rhs, .. } => {
                corrupt_ta_slot(lhs, slot);
                corrupt_ta_slot(rhs, slot);
            }
            HtfExpr::Const(_)
            | HtfExpr::Source(_)
            | HtfExpr::InputSource { .. }
            | HtfExpr::BarstateIslast
            | HtfExpr::BarstateIsRealtime => {}
        }
    }

    let mut program = compile_to_csil(
        "//@version=6\nindicator(\"S\")\nplot(request.security(syminfo.tickerid, \"1W\", ta.sma(close, 2)))\n",
    );
    validate(&program).expect("valid compound security must validate");

    let mut mutated = false;
    for instr in &mut program.functions[0].code {
        if let CsilInstr::ReadSecurityExpr { expr, .. } = instr {
            corrupt_ta_slot(expr, u32::MAX);
            mutated = true;
        }
    }
    assert!(
        mutated,
        "expected a ReadSecurityExpr instruction to corrupt"
    );

    let errors = validate(&program).expect_err("out-of-range ta_slot must be rejected");
    assert!(
        errors.iter().any(|e| e.to_string().contains("ta_slot")),
        "expected a ta_slot range error, got {errors:?}"
    );
}

#[test]
fn validator_rejects_out_of_range_realtime_history_slot() {
    let mut program = compile_to_csil(
        "//@version=6\nindicator(\"S\")\nplot(request.security(syminfo.tickerid, \"1W\", close[barstate.isrealtime ? 1 : 0]))\n",
    );
    validate(&program).expect("compiler-produced realtime history must validate");

    let mut mutated = false;
    for instr in &mut program.functions[0].code {
        if let CsilInstr::ReadSecurityExpr {
            expr: pinewasm_hir::HtfExpr::HistoryRealtime { history_slot, .. },
            ..
        } = instr
        {
            *history_slot = u32::MAX;
            mutated = true;
        }
    }
    assert!(mutated, "expected the dynamic security history node");
    let errors = validate(&program).expect_err("invalid history slot must be rejected");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("history_slot")),
        "expected a history_slot range error, got {errors:?}"
    );
}

#[test]
fn validator_rejects_int_where_f64_required() {
    // PLOT of an int (compiled programs normally coerce through INT_TO_F64).
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        let pc = f
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::PlotStyledF64 { color_count: 1, .. }))
            .unwrap();
        assert!(matches!(f.code[pc - 1], CsilInstr::PushColor { .. }));
        let pc = pc - 1;
        f.code
            .splice(pc..pc, [CsilInstr::Pop, CsilInstr::PushI64(1)]);
        f.source_map
            .splice(pc..pc, [pinewasm_span::Span::dummy(); 2]);
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );

    // Float arithmetic on an int operand is also rejected.
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        for instr in &mut f.code {
            if matches!(instr, CsilInstr::ReadBuiltin { .. }) {
                *instr = CsilInstr::PushI64(1);
            }
        }
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_f64_where_int_required() {
    // INT_TO_F64 of a float (and by extension the INT_* ops) must not
    // validate.
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        f.code.insert(0, CsilInstr::PushF64(F64Bits::new(1.0)));
        f.code.insert(1, CsilInstr::IntToF64);
        f.code.insert(2, CsilInstr::Pop);
        for _ in 0..3 {
            f.source_map.insert(0, pinewasm_span::Span::dummy());
        }
        f.max_stack = f.max_stack.max(3);
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be Int, found F64")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_accepts_int_operands_for_comparisons() {
    // CMP_* is numeric-polymorphic: Int-Int, F64-F64, and (defensively)
    // mixed pairs all validate.
    let mut program = compile_to_csil(CORE);
    let f = &mut program.functions[0];
    f.budget_reference = None;
    let mut prefix = vec![
        CsilInstr::PushI64(1),
        CsilInstr::PushI64(2),
        CsilInstr::CmpLt,
        CsilInstr::Pop,
        CsilInstr::PushI64(1),
        CsilInstr::PushF64(F64Bits::new(2.0)),
        CsilInstr::CmpGe,
        CsilInstr::Pop,
    ];
    for _ in 0..prefix.len() {
        f.source_map.insert(0, pinewasm_span::Span::dummy());
    }
    prefix.append(&mut f.code);
    f.code = prefix;
    validate(&program).expect("numeric comparisons must validate");
}

#[test]
fn validator_rejects_unbalanced_return() {
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        let return_index = f.code.len() - 1;
        f.code
            .insert(return_index, CsilInstr::PushF64(F64Bits::new(1.0)));
        f.source_map
            .insert(return_index, pinewasm_span::Span::dummy());
    });
    assert!(errors
        .iter()
        .any(|e| e.to_string().contains("empty at RETURN")));
}

#[test]
fn validator_rejects_short_source_map() {
    let errors = corrupt(|p| {
        p.functions[0].source_map.pop();
    });
    assert!(errors.iter().any(|e| e.to_string().contains("source map")));
}

#[test]
fn validator_rejects_understated_max_stack() {
    let errors = corrupt(|p| p.functions[0].max_stack = 0);
    assert!(errors.iter().any(|e| e.to_string().contains("understates")));
}

#[test]
fn validator_rejects_non_finite_constants() {
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        f.code.insert(0, CsilInstr::PushF64(F64Bits::new(f64::NAN)));
        f.source_map.insert(0, pinewasm_span::Span::dummy());
        f.code.insert(1, CsilInstr::Pop);
        f.source_map.insert(1, pinewasm_span::Span::dummy());
    });
    assert!(errors.iter().any(|e| e.to_string().contains("finite")));
}

#[test]
fn validator_accepts_dup() {
    let mut program = compile_to_csil(CORE);
    let f = &mut program.functions[0];
    f.budget_reference = None;
    // read; DUP; POP; rest — balanced and typed.
    f.code.insert(1, CsilInstr::Dup);
    f.source_map.insert(1, pinewasm_span::Span::dummy());
    f.code.insert(2, CsilInstr::Pop);
    f.source_map.insert(2, pinewasm_span::Span::dummy());
    f.max_stack += 1;
    validate(&program).expect("dup/pop is balanced");
}

#[test]
fn validator_checks_jump_if_slot_written_bounds() {
    // Out-of-range slot.
    let errors = corrupt(|p| {
        p.functions[0].code.insert(
            0,
            CsilInstr::JumpIfSlotWritten {
                slot: 42,
                target: 0,
            },
        );
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("series slot 42")),
        "errors: {errors:?}"
    );

    // Out-of-range jump target.
    let errors = corrupt(|p| {
        p.functions[0].code.insert(
            0,
            CsilInstr::JumpIfSlotWritten {
                slot: 0,
                target: u32::MAX,
            },
        );
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors.iter().any(|e| e.to_string().contains("jump target")),
        "errors: {errors:?}"
    );
}

#[test]
fn jump_if_slot_written_has_no_stack_effect_and_merges_paths() {
    // Both successor paths of JUMP_IF_SLOT_WRITTEN must merge like
    // JUMP_IF_FALSE; inserting one at offset 0 (skipping the first read)
    // must fail the merge because the two paths disagree on stack height.
    let errors = corrupt(|p| {
        p.functions[0]
            .code
            .insert(0, CsilInstr::JumpIfSlotWritten { slot: 0, target: 2 });
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("inconsistent stack heights")),
        "errors: {errors:?}"
    );

    // Jumping to the same height (offset 1 → the read it falls through to
    // is skipped-and-replaced by nothing) — target the *next* instruction:
    // both paths identical, so the program still validates.
    let mut program = compile_to_csil(CORE);
    let f = &mut program.functions[0];
    f.budget_reference = None;
    f.code
        .insert(0, CsilInstr::JumpIfSlotWritten { slot: 0, target: 1 });
    f.source_map.insert(0, pinewasm_span::Span::dummy());
    validate(&program).expect("no-op slot-written jump must validate");
}

#[test]
fn validator_rejects_window_mean_bad_length_and_slot() {
    // Start from a real WINDOW_MEAN program and corrupt its operands.
    let compile_ta = || compile_to_csil("indicator(\"TA\")\nplot(ta.sma(close, 2))\n");
    let corrupt_ta = |mutator: fn(&mut CsilProgram)| {
        let mut program = compile_ta();
        mutator(&mut program);
        validate(&program).expect_err("corrupted WINDOW_MEAN must fail validation")
    };

    // The pristine program validates.
    validate(&compile_ta()).expect("ta.sma program must validate");

    // Length 0.
    let errors = corrupt_ta(|p| {
        for instr in &mut p.functions[0].code {
            if let CsilInstr::WindowMean { length, .. } = instr {
                *length = 0;
            }
        }
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("window length 0 out of range")),
        "errors: {errors:?}"
    );

    // Length above the cap.
    let errors = corrupt_ta(|p| {
        for instr in &mut p.functions[0].code {
            if let CsilInstr::WindowMean { length, .. } = instr {
                *length = 100_001;
            }
        }
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("window length 100001 out of range")),
        "errors: {errors:?}"
    );

    // Slot out of range.
    let errors = corrupt_ta(|p| {
        for instr in &mut p.functions[0].code {
            if let CsilInstr::WindowMean { slot, .. } = instr {
                *slot = 42;
            }
        }
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("series slot 42")),
        "errors: {errors:?}"
    );
}

#[test]
fn dynamic_window_mean_validates_length_type_and_retains_maximum_history() {
    let source = "indicator(\"TA\")\nplot(ta.sma(close, bar_index + 1))\n";
    let program = compile_to_csil(source);
    validate(&program).expect("dynamic SMA should validate");
    assert!(program.history_depth() >= MAX_WINDOW_LENGTH - 1);
    let pc = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, CsilInstr::WindowMeanDynamic { .. }))
        .expect("dynamic window instruction");

    let mut bad_slot = program.clone();
    bad_slot.functions[0].code[pc] = CsilInstr::WindowMeanDynamic { slot: 42 };
    let errors = validate(&bad_slot).expect_err("invalid slot must fail");
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("series slot 42")));

    let mut bad_length = program;
    bad_length.functions[0].code[pc - 1] = CsilInstr::CmpEq;
    let errors = validate(&bad_length).expect_err("string length must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("numeric")),
        "{errors:?}"
    );
}

#[test]
fn dynamic_extremum_validates_length_and_retains_history() {
    let program = compile_to_csil("indicator(\"TA\")\nplot(ta.highest(close, bar_index + 1))\n");
    validate(&program).expect("dynamic highest should validate");
    assert!(program.history_depth() >= MAX_WINDOW_LENGTH - 1);
    let pc = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, CsilInstr::WindowExtremumDynamic { .. }))
        .expect("dynamic extremum instruction");
    let mut bad_slot = program.clone();
    bad_slot.functions[0].code[pc] = CsilInstr::WindowExtremumDynamic {
        slot: 42,
        highest: true,
    };
    assert!(validate(&bad_slot).is_err());
    let mut bad_length = program;
    bad_length.functions[0].code[pc - 1] = CsilInstr::CmpEq;
    assert!(validate(&bad_length).is_err());
}

#[test]
fn budget_reference_receives_nondefault_ta_specialization() {
    let mut program =
        compile_to_csil("indicator(\"TA\")\nlength = input.int(2)\nplot(ta.sma(close, length))\n");
    let binding = program.ta_input_bindings[0];
    let main = &mut program.functions[0];
    main.budget_reference = Some(crate::BudgetReference {
        version: 1,
        code: main.code.clone(),
        source_map: main.source_map.clone(),
        max_stack: main.max_stack,
    });
    validate(&program).expect("baseline validates before specialization");
    program.set_ta_inputs(&std::collections::HashMap::from([(binding.target, 5)]));
    for code in [
        &program.main().code,
        &program.main().budget_reference.as_ref().unwrap().code,
    ] {
        assert!(code
            .iter()
            .any(|instr| matches!(instr, CsilInstr::WindowMean { length: 5, .. })));
    }
}

#[test]
fn integer_input_specializes_float_ta_parameter_after_implicit_cast() {
    let program = compile_to_csil(
        "//@version=6\nindicator(\"Integer supertrend factor\")\nfactor = input.int(3, \"Factor\")\n[st, direction] = ta.supertrend(factor, 2)\nplot(st)\nplot(direction)\n",
    );
    validate(&program).expect("int input is valid for a float TA parameter");
    let binding = program
        .ta_input_bindings
        .iter()
        .find(|binding| binding.input == 0)
        .expect("factor input has a TA binding");
    assert_eq!(
        program.resolve_ta_input(binding, None),
        Some(3.0f64.to_bits() as i64)
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Int(4))),
        Some(4.0f64.to_bits() as i64)
    );
    // A wrongly typed host override follows ordinary input semantics and
    // falls back to the declaration's integer default.
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Float(9.0))),
        Some(3.0f64.to_bits() as i64)
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Int(0))),
        Some(0.0f64.to_bits() as i64)
    );
}

#[test]
fn integer_input_specializes_percent_ta_parameter_after_implicit_cast() {
    let program = compile_to_csil(
        "//@version=6\nindicator(\"Integer percentile\")\np = input.int(50, \"Percentile\", minval=0, maxval=100)\nplot(ta.percentile_nearest_rank(close, 5, p))\n",
    );
    validate(&program).expect("int input is valid for a float percentage parameter");
    let binding = program
        .ta_input_bindings
        .iter()
        .find(|binding| binding.input == 0)
        .expect("percentile input has a TA binding");
    assert_eq!(
        program.resolve_ta_input(binding, None),
        Some(50.0f64.to_bits() as i64)
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Int(75))),
        Some(75.0f64.to_bits() as i64)
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Float(80.0))),
        Some(50.0f64.to_bits() as i64),
        "a wrongly typed override falls back to the integer declaration default"
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Int(101))),
        None,
        "percent TA parameters remain bounded after implicit conversion"
    );
}

#[test]
fn program_without_deletion_keeps_legacy_json_shape() {
    let program = compile_to_csil("indicator(\"plain\")\nplot(close)\n");
    assert!(program.main().budget_reference.is_none());
    let json = program.to_json();
    assert!(!json.contains("budget_reference"));
    assert_eq!(CsilProgram::from_json(&json).unwrap(), program);
}

#[test]
fn window_mean_pushes_f64_for_the_abstract_interpreter() {
    // WINDOW_MEAN feeding float arithmetic validates; feeding an INT_* op
    // must not.
    let mut program = compile_to_csil("indicator(\"TA\")\nplot(ta.sma(close, 2))\n");
    let f = &mut program.functions[0];
    // WINDOW_MEAN ; INT_NEG would require Int on top of stack.
    let wm_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::WindowMean { .. }))
        .expect("program contains WINDOW_MEAN");
    f.code.insert(wm_index + 1, CsilInstr::IntNeg);
    f.source_map
        .insert(wm_index + 1, pinewasm_span::Span::dummy());
    let errors = validate(&program).expect_err("Int op over an F64 push must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be Int, found F64")),
        "errors: {errors:?}"
    );
}

// -- CALL_PURE ------------------------------------------------------------------

#[test]
fn pure_builtins_csil_snapshot() {
    // One CALL_PURE opcode covers every pure builtin: the omitted nz
    // replacement and math.random bounds are materialized as ordinary
    // pushes, and constants lower to PUSH_F64.
    let program = compile_to_csil(
        "indicator(\"Pure\")\nplot(math.sqrt(close))\nplot(nz(close[1]))\nplot(math.random())\nplot(math.pi)\n",
    );
    validate(&program).expect("pure example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn pure_program_round_trips_through_json() {
    let program = compile_to_csil(
        "indicator(\"Pure\")\nplot(math.min(1, 2, close))\nplot(math.round(close, 2))\nplot(nz(close[1], -1.0))\n",
    );
    validate(&program).expect("pure program must validate");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserializes");
    assert_eq!(back, program);
    validate(&back).expect("round-tripped program must validate");
}

#[test]
fn validator_rejects_call_pure_argc_out_of_bounds() {
    let compile_pure = || compile_to_csil("indicator(\"P\")\nplot(math.sqrt(close))\n");
    validate(&compile_pure()).expect("pure program must validate");

    // math.sqrt takes exactly one argument.
    let mut program = compile_pure();
    for instr in &mut program.functions[0].code {
        if let CsilInstr::CallPure { argc, .. } = instr {
            *argc = 2;
        }
    }
    let errors = validate(&program).expect_err("bad argc must fail validation");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("argc 2 out of range (1..=1)")),
        "errors: {errors:?}"
    );

    // nz always carries its replacement at the CSIL level: argc 1 is invalid
    // (there is no typed default to fall back to at runtime).
    let mut program = compile_to_csil("indicator(\"P\")\nplot(nz(close[1]))\n");
    for instr in &mut program.functions[0].code {
        if let CsilInstr::CallPure { argc, .. } = instr {
            *argc = 1;
        }
    }
    let errors = validate(&program).expect_err("nz argc 1 must fail validation");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("argc 1 out of range (2..=2)")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_call_pure_operand_type_confusion() {
    // math.sqrt requires an F64 operand: feed it an int.
    let mut program = compile_to_csil("indicator(\"P\")\nplot(math.sqrt(close))\n");
    let f = &mut program.functions[0];
    let read_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::ReadBuiltin { .. }))
        .expect("program reads close");
    f.code[read_index] = CsilInstr::PushI64(4);
    let errors = validate(&program).expect_err("Int operand for math.sqrt must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_mismatched_nz_operands() {
    // nz(source, replacement) is relational, like equality: statically
    // known mismatched types must fail.
    let mut program = compile_to_csil("indicator(\"P\")\nplot(nz(close[1], 1.5))\n");
    let f = &mut program.functions[0];
    let push_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::PushF64(_)))
        .expect("program pushes the replacement");
    f.code[push_index] = CsilInstr::PushColor {
        r: 255,
        g: 0,
        b: 0,
        t: crate::F64Bits::new(0.0),
    };
    let errors = validate(&program).expect_err("Color replacement for F64 source must fail");
    assert!(
        errors.iter().any(|e| e
            .to_string()
            .contains("NZ operands must have matching types")),
        "errors: {errors:?}"
    );
}

#[test]
fn call_pure_result_types_feed_the_abstract_interpreter() {
    // math.floor pushes Int: float arithmetic over it must fail.
    let mut program = compile_to_csil("indicator(\"P\")\nplot(math.floor(close) % 2)\n");
    validate(&program).expect("floor program must validate (int result)");
    let f = &mut program.functions[0];
    let call_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::CallPure { .. }))
        .expect("program contains CALL_PURE");
    f.code.insert(call_index + 1, CsilInstr::F64Neg);
    f.source_map
        .insert(call_index + 1, pinewasm_span::Span::dummy());
    let errors = validate(&program).expect_err("F64 op over an Int push must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );

    // na() pushes Bool and accepts any operand type (PUSH_NA is Any).
    let program =
        compile_to_csil("indicator(\"P\")\nif na(close[1])\n    alert(\"missing\")\nplot(close)\n");
    validate(&program).expect("na() condition must validate as Bool");
}

// -- Ternary branch lowering ------------------------------------------------------

#[test]
fn ternary_csil_snapshot() {
    // Lazy ternaries lower to explicit control flow; each arm leaves one
    // value for the shared continuation.
    let program = compile_to_csil("indicator(\"Ternary\")\nplot(close > open ? close : open)\n");
    validate(&program).expect("ternary example must validate");
    assert!(
        program.functions[0]
            .code
            .iter()
            .any(|i| matches!(i, CsilInstr::JumpIfFalse { .. })),
        "expected a conditional branch for the ternary"
    );
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn ternary_program_round_trips_through_json() {
    let program = compile_to_csil("indicator(\"TernaryRT\")\nplot(close > open ? close : na)\n");
    validate(&program).expect("ternary program must validate");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "ternary branch code must survive JSON");
    validate(&back).expect("round-tripped program must validate");
}

#[test]
fn validator_rejects_select_on_non_bool_condition() {
    // Build a corrupt SELECT whose condition operand is a float: push three
    // F64s and select. The condition (third from the top) must be Bool.
    let errors = corrupt(|p| {
        let f = &mut p.functions[0];
        let prefix = [
            CsilInstr::PushF64(F64Bits::new(0.0)),
            CsilInstr::PushF64(F64Bits::new(1.0)),
            CsilInstr::PushF64(F64Bits::new(2.0)),
            CsilInstr::Select,
            CsilInstr::Pop,
        ];
        for (i, instr) in prefix.into_iter().enumerate() {
            f.code.insert(i, instr);
            f.source_map.insert(i, pinewasm_span::Span::dummy());
        }
        f.max_stack = f.max_stack.max(3);
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("operand 3 must be Bool, found F64")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_select_underflow() {
    let errors = corrupt(|p| {
        p.functions[0].code.insert(0, CsilInstr::Select);
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors.iter().any(|e| e.to_string().contains("underflow")),
        "errors: {errors:?}"
    );
}

#[test]
fn ternary_branch_join_is_checked_by_typed_consumer() {
    // Branch values are carried through a hidden slot at the join. Inject an
    // Int operation before the F64 plot and ensure the consumer rejects its
    // Int result.
    let mut program = compile_to_csil("indicator(\"S\")\nplot(close > open ? close : open)\n");
    validate(&program).expect("pristine branch program must validate");
    let f = &mut program.functions[0];
    let plot_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::PlotStyledF64 { color_count: 1, .. }))
        .expect("program contains a styled numeric plot");
    assert!(matches!(
        f.code[plot_index - 1],
        CsilInstr::PushColor { .. }
    ));
    let plot_index = plot_index - 1;
    f.code.insert(plot_index, CsilInstr::IntNeg);
    f.source_map
        .insert(plot_index, pinewasm_span::Span::dummy());
    let errors = validate(&program).expect_err("F64 plot must reject the injected Int result");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );

    // Mismatched branch types merge to Any, which the existing dynamic
    // consumer accepts; the VM re-checks the selected value at runtime.
    let mut program = compile_to_csil("indicator(\"S\")\nplot(close > open ? close : open)\n");
    let f = &mut program.functions[0];
    let else_target = f
        .code
        .iter()
        .find_map(|i| match i {
            CsilInstr::JumpIfFalse { target } => Some(*target as usize),
            _ => None,
        })
        .expect("program contains the ternary false edge");
    // The false arm starts with the else-value read. Replace it with an int
    // while keeping that arm's stack effect unchanged.
    f.code[else_target] = CsilInstr::PushI64(1);
    // The edit no longer matches the stored budget baseline.
    f.budget_reference = None;
    validate(&program).expect("mismatched branch types merge to Any");
}

#[test]
fn validator_rejects_overdeclared_max_stack() {
    // Hosts size buffers from max_stack; a huge declared value with tiny
    // actual usage must not validate.
    let errors = corrupt(|p| p.functions[0].max_stack = 100_000_000);
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("exceeds the limit")),
        "errors: {errors:?}"
    );
}

fn stack_triangle(depth: usize) -> CsilProgram {
    let mut program = compile_to_csil("indicator(\"Stack shape\")\n");
    let function = &mut program.functions[0];
    function.code = std::iter::repeat_n(CsilInstr::PushNa, depth)
        .chain(std::iter::repeat_n(CsilInstr::Pop, depth))
        .chain(std::iter::once(CsilInstr::Return))
        .collect();
    function.source_map = vec![pinewasm_span::Span::dummy(); function.code.len()];
    function.max_stack = depth as u32;
    program
}

#[test]
fn validator_bounds_abstract_stack_snapshot_complexity() {
    // For a push-N/pop-N triangle, the sum of retained stack heights is N².
    // These adjacent depths straddle the cell budget exactly without
    // requiring a large serialized fixture.
    let mut accepted_depth = 0usize;
    while (accepted_depth + 1)
        .checked_pow(2)
        .is_some_and(|cells| cells <= MAX_VALIDATION_ABSTRACT_CELLS)
    {
        accepted_depth += 1;
    }
    assert!(accepted_depth.pow(2) <= MAX_VALIDATION_ABSTRACT_CELLS);
    assert!((accepted_depth + 1).pow(2) > MAX_VALIDATION_ABSTRACT_CELLS);
    validate(&stack_triangle(accepted_depth)).expect("just-under-limit triangle must validate");

    let errors = validate(&stack_triangle(accepted_depth + 1))
        .expect_err("one-step-larger triangle must fail before typed snapshots allocate");
    assert!(errors.iter().any(|error| {
        error
            .to_string()
            .contains("abstract stack analysis exceeds the 8000000-cell complexity limit")
    }));
}

// -- CALL_TA (the VM-native ta engine) --------------------------------------------

#[test]
fn ta_engine_csil_snapshot() {
    // Engine calls lower to CALL_TA over a TA_STATE table (const params
    // baked in the declaration); no hidden slots are involved. The bool
    // result of crossover feeds JUMP_IF_FALSE directly.
    let program = compile_to_csil(
        "indicator(\"Engine\")\nplot(ta.ema(close, 9))\nif ta.crossover(close, open)\n    alert(\"cross\")\n",
    );
    validate(&program).expect("engine example must validate");
    assert!(program.series_slots.is_empty(), "no hidden slots expected");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn ta_engine_program_round_trips_through_json() {
    // Pins TaStateDecl serde, including alma's f64-bit-pattern params.
    let program = compile_to_csil(
        "indicator(\"EngineRT\")\nplot(ta.alma(close, 9, 0.85, 6))\nplot(ta.valuewhen(close > open, close, 2))\n",
    );
    validate(&program).expect("engine program must validate");
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "ta_states must survive JSON exactly");
    validate(&back).expect("round-tripped program must validate");
}

#[test]
fn validator_rejects_call_ta_state_out_of_range() {
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.cum(close))\n");
    for instr in &mut program.functions[0].code {
        if let CsilInstr::CallTa { state } = instr {
            *state = 42;
        }
    }
    let errors = validate(&program).expect_err("out-of-range state must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("ta state 42 out of range")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_duplicate_call_ta_state_use() {
    // Two call sites, then both rewritten to state 0: per-call-site state
    // must not be shared.
    let mut program =
        compile_to_csil("indicator(\"T\")\nplot(ta.cum(close))\nplot(ta.cum(close))\n");
    validate(&program).expect("pristine two-site program must validate");
    for instr in &mut program.functions[0].code {
        if let CsilInstr::CallTa { state } = instr {
            *state = 0;
        }
    }
    let errors = validate(&program).expect_err("duplicate state use must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("ta state 0 is already used")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_call_ta_operand_type_confusion() {
    // A numeric source operand must be F64: feed ta.ema an int.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.ema(close, 2))\n");
    let f = &mut program.functions[0];
    let read_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::ReadBuiltin { .. }))
        .expect("program reads close");
    f.code[read_index] = CsilInstr::PushI64(4);
    let errors = validate(&program).expect_err("Int operand for CALL_TA must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );

    // A condition operand must be Bool: replace barssince's comparison
    // with float arithmetic (same 2→1 stack shape, wrong type).
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.barssince(close > open))\n");
    validate(&program).expect("pristine barssince program must validate");
    let f = &mut program.functions[0];
    for instr in &mut f.code {
        if matches!(instr, CsilInstr::CmpGt) {
            *instr = CsilInstr::F64Add;
        }
    }
    let errors = validate(&program).expect_err("F64 condition for CALL_TA must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be Bool, found F64")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_malformed_ta_state_declarations() {
    let compile_engine = || compile_to_csil("indicator(\"T\")\nplot(ta.ema(close, 2))\n");

    // A slot-based function in the engine table.
    let mut program = compile_engine();
    program.ta_states[0].func = crate::TaFunc::Sma;
    let errors = validate(&program).expect_err("slot-based func in ta_states must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("slot-based, not an engine function")),
        "errors: {errors:?}"
    );

    // Window length out of range.
    let mut program = compile_engine();
    program.ta_states[0].params[0] = 0;
    let errors = validate(&program).expect_err("length 0 must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("window length 0 out of range")),
        "errors: {errors:?}"
    );

    // Wrong param count.
    let mut program = compile_engine();
    program.ta_states[0].params.push(7);
    let errors = validate(&program).expect_err("extra param must fail");
    assert!(
        errors.iter().any(|e| e
            .to_string()
            .contains("takes 1 const parameter(s), found 2")),
        "errors: {errors:?}"
    );

    // alma's sigma bits must decode to a positive finite float.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.alma(close, 9, 0.85, 6))\n");
    program.ta_states[0].params[2] = i64::from_ne_bytes(f64::NAN.to_bits().to_ne_bytes());
    let errors = validate(&program).expect_err("NaN sigma bits must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("does not decode to a finite float")),
        "errors: {errors:?}"
    );
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.alma(close, 9, 0.85, 6))\n");
    program.ta_states[0].params[2] = i64::from_ne_bytes((-1.0f64).to_bits().to_ne_bytes());
    let errors = validate(&program).expect_err("negative sigma must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be greater than zero")),
        "errors: {errors:?}"
    );

    // Boolean flags must be 0 or 1.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.tr(true))\n");
    program.ta_states[0].params[0] = 2;
    let errors = validate(&program).expect_err("flag 2 must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("flag must be 0 or 1")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_reinterprets_signed_float_payload_bits_exactly() {
    // ALMA's offset permits negative finite values. Its sign bit makes the
    // wire payload a negative i64, which must be reinterpreted—not converted
    // numerically—before the FloatBits rule checks it.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.alma(close, 9, 0.85, 6))\n");
    program.ta_states[0].params[1] = i64::from_ne_bytes((-0.85f64).to_bits().to_ne_bytes());
    validate(&program).expect("negative finite FloatBits payload must retain its exact bits");
}

#[test]
fn validator_checks_float_ta_input_binding_type_and_default_bits() {
    let program = compile_to_csil(
        "indicator(\"float binding\")\nmult = input.float(2.0)\n[mid, upper, lower] = ta.bb(close, 3, mult)\nplot(upper)\n",
    );
    validate(&program).expect("compiler's float TA binding is valid");
    assert_eq!(program.ta_input_bindings.len(), 1);

    let mut wrong_kind = program.clone();
    wrong_kind.inputs[0].kind = crate::InputKind::Int;
    assert!(validate(&wrong_kind).is_err());

    let mut wrong_default = program.clone();
    wrong_default.ta_states[0].params[1] = 3.0f64.to_bits() as i64;
    assert!(validate(&wrong_default).is_err());

    let mut wrong_target = program;
    wrong_target.ta_input_bindings[0].target =
        pinewasm_hir::TaInputTarget::State { state: 0, param: 0 };
    assert!(validate(&wrong_target).is_err());
}

#[test]
fn call_ta_result_types_feed_the_abstract_interpreter() {
    // highestbars pushes Int: the compiled plot goes through INT_TO_F64
    // (validates), and float arithmetic directly over the push must fail.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.highestbars(close, 2))\n");
    validate(&program).expect("int-result engine program must validate");
    let f = &mut program.functions[0];
    let call_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::CallTa { .. }))
        .expect("program contains CALL_TA");
    f.code.insert(call_index + 1, CsilInstr::F64Neg);
    f.source_map
        .insert(call_index + 1, pinewasm_span::Span::dummy());
    let errors = validate(&program).expect_err("F64 op over an Int push must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Int")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_out_of_range_percentile_percentage_bits() {
    // The percentage param of the percentile functions must decode to a
    // finite float within 0..=100; adversarial bit patterns fail.
    for bits in [
        i64::from_ne_bytes((200.0f64).to_bits().to_ne_bytes()),
        i64::from_ne_bytes((-1.0f64).to_bits().to_ne_bytes()),
        i64::from_ne_bytes(f64::INFINITY.to_bits().to_ne_bytes()),
    ] {
        let mut program =
            compile_to_csil("indicator(\"T\")\nplot(ta.percentile_nearest_rank(close, 5, 50))\n");
        program.ta_states[0].params[1] = bits;
        let errors = validate(&program).expect_err("bad percentage bits must fail");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("finite float in 0..=100")),
            "errors: {errors:?}"
        );
    }
}

#[test]
fn call_ta_bool_results_feed_the_abstract_interpreter() {
    // ta.rising pushes Bool: the compiled ternary plot validates, and
    // float arithmetic directly over the push must fail.
    let mut program = compile_to_csil("indicator(\"T\")\nplot(ta.rising(close, 2) ? 1 : 0)\n");
    validate(&program).expect("bool-result engine program must validate");
    let f = &mut program.functions[0];
    let call_index = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::CallTa { .. }))
        .expect("program contains CALL_TA");
    f.code.insert(call_index + 1, CsilInstr::F64Neg);
    f.source_map
        .insert(call_index + 1, pinewasm_span::Span::dummy());
    let errors = validate(&program).expect_err("F64 op over a Bool push must fail");
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("must be F64, found Bool")),
        "errors: {errors:?}"
    );
}

#[test]
fn for_loop_csil_validates() {
    let program = compile_to_csil(
        "indicator(\"L\")\nvar s = 0\ns := 0\nfor i = 1 to 10\n    s := s + i\nplot(s)\n",
    );
    validate(&program).expect("for loop must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn nested_loops_validator_worklist_converges() {
    // Nested loops mean back-edges into merge points that already have a
    // state; the abstract-interpreter worklist must converge (empty stacks at
    // every block boundary). If it did not, validate would loop forever or
    // report a merge-height error.
    let program = compile_to_csil(
        "indicator(\"N\")\nvar s = 0\ns := 0\nfor i = 1 to 3\n    for j = 1 to 3\n        s := s + j\nplot(s)\n",
    );
    validate(&program).expect("nested loops must validate and the worklist must converge");
}

#[test]
fn while_loop_with_break_continue_validates() {
    let program = compile_to_csil(
        "indicator(\"W\")\nvar i = 0\ni := 0\nwhile i < 10\n    if i == 5\n        break\n    i := i + 1\n    continue\nplot(i)\n",
    );
    validate(&program).expect("while with break/continue must validate");
}

#[test]
fn tuple_destructure_csil_snapshot() {
    // CALL_TA pushes three results (max_stack 3); the stores pop them in
    // reverse. TA_STATE records ta.macd with its three baked lengths.
    let program = compile_to_csil(
        "//@version=6\nindicator(\"T\")\n[m, s, h] = ta.macd(close, 3, 7, 4)\nplot(m)\nplot(h)\n",
    );
    validate(&program).expect("tuple destructure must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn tuple_call_ta_pushes_three_and_validates() {
    // The multi-push CALL_TA (arity 3 from ta_states) is accepted by the
    // abstract interpreter, which pushes one value per tuple element; the
    // resulting max_stack reflects the three-value push.
    let program = compile_to_csil(
        "//@version=6\nindicator(\"T\")\n[a, b, c] = ta.dmi(14, 14)\nplot(a)\nplot(b)\nplot(c)\n",
    );
    validate(&program).expect("tuple CALL_TA must validate");
    let main = &program.functions[0];
    // ta.dmi pops nothing and pushes three, so the peak stack is 3.
    assert_eq!(main.max_stack, 3);
    // Exactly one CALL_TA for the single tuple state.
    let call_ta_count = main
        .code
        .iter()
        .filter(|i| matches!(i, CsilInstr::CallTa { .. }))
        .count();
    assert_eq!(call_ta_count, 1);
    assert_eq!(program.ta_states.len(), 1);
}

// ---------------------------------------------------------------------------
// Strings — StrConcat opcode and str.* CallPure lowering/validation.
// ---------------------------------------------------------------------------

#[test]
fn string_concat_lowers_to_str_concat_opcode() {
    let program = compile_to_csil("indicator(\"S\")\ns = \"a\" + \"b\"\nplot(str.length(s))\n");
    validate(&program).expect("string concat must validate");
    assert!(
        program.main().code.contains(&CsilInstr::StrConcat),
        "expected a STR_CONCAT opcode, got: {:?}",
        program.main().code
    );
}

#[test]
fn str_functions_lower_and_validate() {
    // A representative program exercising every result kind: string, int,
    // bool, float. It must lower and pass validation (correct operand types
    // on the abstract stack).
    let program = compile_to_csil(
        "indicator(\"S\")\n\
         s = str.upper(str.substring(\"abcdef\", 1, 4))\n\
         n = str.length(s) + str.pos(s, \"C\")\n\
         b = str.contains(s, \"BC\")\n\
         f = str.tonumber(str.tostring(n))\n\
         plot(b ? f : n)\n",
    );
    validate(&program).expect("str.* program must validate");
}

#[test]
fn str_replace_materializes_default_occurrence() {
    // The 3-argument surface form gains a 4th (occurrence = 0) argument at
    // the CSIL level: CALL_PURE str.replace with argc 4.
    let program = compile_to_csil(
        "indicator(\"S\")\ns = str.replace(\"a.a\", \".\", \"-\")\nplot(str.length(s))\n",
    );
    let has_replace4 = program.main().code.iter().any(|i| {
        matches!(
            i,
            CsilInstr::CallPure {
                func: crate::PureFunc::StrReplace,
                argc: 4
            }
        )
    });
    assert!(has_replace4, "str.replace must lower with argc 4");
    validate(&program).expect("must validate");
}

#[test]
fn str_program_round_trips_through_json() {
    let program = compile_to_csil(
        "indicator(\"S\")\ns = \"x=\" + str.tostring(close)\nplot(str.length(s))\n",
    );
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("deserialize");
    assert_eq!(program, back, "STR_CONCAT and str.* must survive JSON");
}

#[test]
fn validator_rejects_str_concat_on_non_strings() {
    // Corrupt a validated string program so STR_CONCAT sees numeric operands.
    let mut program = compile_to_csil("indicator(\"S\")\ns = \"a\" + \"b\"\nplot(str.length(s))\n");
    // Replace the two PUSH_STR operands feeding the first STR_CONCAT with
    // numeric pushes; the abstract stack must reject the mismatch.
    let f = &mut program.functions[0];
    let concat_at = f
        .code
        .iter()
        .position(|i| matches!(i, CsilInstr::StrConcat))
        .expect("has STR_CONCAT");
    f.code[concat_at - 1] = CsilInstr::PushF64(F64Bits::new(1.0));
    f.code[concat_at - 2] = CsilInstr::PushF64(F64Bits::new(2.0));
    let errors = validate(&program).expect_err("must reject numeric STR_CONCAT operands");
    assert!(matches!(errors[0], ValidationError::AtInstruction { .. }));
}

#[test]
fn validator_rejects_str_length_on_non_string() {
    let mut program = compile_to_csil("indicator(\"S\")\nplot(str.length(\"abc\"))\n");
    let f = &mut program.functions[0];
    // Turn the PUSH_STR feeding str.length into an int push.
    let call_at = f
        .code
        .iter()
        .position(|i| {
            matches!(
                i,
                CsilInstr::CallPure {
                    func: crate::PureFunc::StrLength,
                    ..
                }
            )
        })
        .expect("has str.length");
    f.code[call_at - 1] = CsilInstr::PushI64(3);
    let errors = validate(&program).expect_err("str.length needs a string operand");
    assert!(matches!(errors[0], ValidationError::AtInstruction { .. }));
}

// -- color -------------------------------------------------------------------

#[test]
fn color_program_csil_snapshot() {
    let program = compile_to_csil(
        "//@version=6\nindicator(\"Colors\")\nc = color.red\nd = #00FF0080\nplot(c == d ? 1.0 : 0.0)\n",
    );
    validate(&program).expect("color example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn color_literal_lowers_to_push_color() {
    let program = compile_to_csil("indicator(\"C\")\nc = #12AB34\nplot(close, color=c)\n");
    validate(&program).expect("valid");
    let code = &program.functions[0].code;
    assert!(
        code.iter().any(|i| matches!(
            i,
            CsilInstr::PushColor {
                r: 0x12,
                g: 0xAB,
                b: 0x34,
                t
            } if t.get() == 0.0
        )),
        "expected PUSH_COLOR #12AB34, got {code:?}"
    );
}

#[test]
fn color_literal_with_alpha_lowers_transparency() {
    // alpha 0x80 = 128 retains its exact fractional percentage.
    let program = compile_to_csil("indicator(\"C\")\nc = #00FF0080\nplot(close, color=c)\n");
    let code = &program.functions[0].code;
    assert!(
        code.iter()
            .any(|i| matches!(i, CsilInstr::PushColor { t, .. } if t.get() == 100.0 * (1.0 - 128.0 / 255.0))),
        "expected PUSH_COLOR preserving alpha 128, got {code:?}"
    );
}

#[test]
fn validator_rejects_color_transparency_over_100() {
    let errors = corrupt(|p| {
        p.functions[0].code.insert(
            0,
            CsilInstr::PushColor {
                r: 0,
                g: 0,
                b: 0,
                t: crate::F64Bits::new(101.0),
            },
        );
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("transparency 101 out of range")),
        "errors: {errors:?}"
    );
}

#[test]
fn color_functions_lower_and_validate() {
    // Dynamic color.new/rgb/from_gradient/accessors lower to CALL_PURE and the
    // resulting program validates (abstract stack types line up).
    let program = compile_to_csil(
        "//@version=6\nindicator(\"C\")\n\
         a = color.rgb(close, 0, 0, 40)\n\
         b = color.new(a, 50)\n\
         g = color.from_gradient(50, 0, 100, a, b)\n\
         plot(color.r(g) + color.t(b))\n",
    );
    validate(&program).expect("color functions must validate");
    let funcs = |f: crate::PureFunc| {
        program.functions[0]
            .code
            .iter()
            .any(|i| matches!(i, CsilInstr::CallPure { func, .. } if *func == f))
    };
    assert!(funcs(crate::PureFunc::ColorRgb), "expected color.rgb");
    assert!(funcs(crate::PureFunc::ColorNew), "expected color.new");
    assert!(
        funcs(crate::PureFunc::ColorFromGradient),
        "expected color.from_gradient"
    );
    assert!(funcs(crate::PureFunc::ColorRFloat), "expected color.r");
    assert!(funcs(crate::PureFunc::ColorTFloat), "expected color.t");
}

#[test]
fn color_rgb_materializes_default_transparency() {
    // The omitted transp default (0) is materialized so the CSIL form is always
    // four arguments when the call has a dynamic operand.
    let program =
        compile_to_csil("indicator(\"C\")\nc = color.rgb(close, 2, 3)\nplot(color.r(c))\n");
    validate(&program).expect("valid");
    assert!(
        program.functions[0].code.iter().any(|i| matches!(
            i,
            CsilInstr::CallPure {
                func: crate::PureFunc::ColorRgb,
                argc: 4
            }
        )),
        "expected CALL_PURE color.rgb argc=4"
    );
}

#[test]
fn validator_rejects_color_accessor_of_non_color() {
    // Feeding color.r a float operand instead of a color is a validator
    // error (abstract stack type mismatch).
    let errors = corrupt(|p| {
        p.functions[0].code.insert(
            0,
            CsilInstr::CallPure {
                func: crate::PureFunc::ColorR,
                argc: 1,
            },
        );
        // The instruction above pops from an empty/ill-typed stack; the
        // validator flags the underflow or the operand-type mismatch.
    });
    assert!(!errors.is_empty(), "expected a validation error");
}

#[test]
fn input_program_csil_snapshot() {
    let program = compile_to_csil(
        "//@version=6\nindicator(\"Inputs\")\nlen = input.int(14, \"Length\", 1, 100, 1)\nsrc = input.source(close, \"Source\")\nplot(src + len)\n",
    );
    validate(&program).expect("input example must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn read_input_lowers_and_carries_metadata() {
    let program = compile_to_csil("indicator(\"I\")\nn = input.int(7)\nplot(close + n)\n");
    validate(&program).expect("valid");
    assert_eq!(program.inputs.len(), 1);
    assert_eq!(program.inputs[0].kind, crate::InputKind::Int);
    assert!(
        program.functions[0]
            .code
            .iter()
            .any(|i| matches!(i, CsilInstr::ReadInput { input: 0 })),
        "expected READ_INPUT 0"
    );
}

#[test]
fn validator_rejects_read_input_out_of_range() {
    // A READ_INPUT referencing a non-existent input id.
    let errors = corrupt(|p| {
        p.functions[0]
            .code
            .insert(0, CsilInstr::ReadInput { input: 0 });
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("input 0 out of range")),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_input_defval_kind_mismatch() {
    let program = compile_to_csil("indicator(\"I\")\nn = input.int(7)\nplot(close + n)\n");
    let mut bad = program.clone();
    // Corrupt the declared default to a string while the kind stays Int.
    bad.inputs[0].defval = crate::InputValue::String("nope".to_string());
    let errors = validate(&bad).expect_err("kind mismatch must fail validation");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::Input { .. })),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_reconstructs_compiler_input_constraint_invariants() {
    let ranged = compile_to_csil(
        "indicator(\"I\")\nn = input.float(2.0, minval=1.0, maxval=3.0, step=0.5)\nplot(n)\n",
    );
    validate(&ranged).expect("compiler-produced numeric range must validate");

    let mut non_finite = ranged.clone();
    non_finite.inputs[0].constraints.min = Some(crate::InputValue::Float(f64::NAN));
    let errors = validate(&non_finite).expect_err("non-finite constraint must fail validation");
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("minval float must be finite")));

    let mut outside_range = ranged.clone();
    outside_range.inputs[0].constraints.max = Some(crate::InputValue::Float(1.0));
    let errors = validate(&outside_range).expect_err("out-of-range default must fail validation");
    assert!(errors
        .iter()
        .any(|error| error.to_string().contains("defval is above maxval")));

    let mut mixed_forms = ranged;
    mixed_forms.inputs[0].constraints.options = vec![crate::InputValue::Float(2.0)];
    let errors = validate(&mixed_forms).expect_err("range and options must remain exclusive");
    assert!(errors.iter().any(|error| error
        .to_string()
        .contains("bounds/step and options are mutually exclusive")));

    let mut numeric_options =
        compile_to_csil("indicator(\"I\")\nn = input.int(2, options=[1, 2, 3])\nplot(n)\n");
    numeric_options.inputs[0].defval = crate::InputValue::Int(4);
    let errors = validate(&numeric_options).expect_err("numeric default outside options must fail");
    assert!(errors.iter().any(|error| error
        .to_string()
        .contains("options do not contain the declared default")));

    let mut non_finite_option = compile_to_csil(
        "indicator(\"I\")\nn = input.float(2.0, options=[1.0, 2.0, 3.0])\nplot(n)\n",
    );
    non_finite_option.inputs[0].constraints.options[0] = crate::InputValue::Float(f64::INFINITY);
    let errors = validate(&non_finite_option).expect_err("non-finite option must fail validation");
    assert!(errors.iter().any(|error| error
        .to_string()
        .contains("options entry float must be finite")));

    let mut string_options = compile_to_csil(
        "indicator(\"I\")\ns = input.string(\"a\", options=[\"a\", \"b\"])\nplot(s == \"a\" ? 1 : 0)\n",
    );
    string_options.inputs[0].defval = crate::InputValue::String("c".to_string());
    let errors = validate(&string_options).expect_err("string default outside options must fail");
    assert!(errors.iter().any(|error| error
        .to_string()
        .contains("options do not contain the declared default")));
}

#[test]
fn validator_rejects_non_price_input_source_default() {
    let program = compile_to_csil("indicator(\"I\")\nsrc = input.source(close)\nplot(src)\n");
    let mut bad = program.clone();
    bad.inputs[0].defval = crate::InputValue::Source(pinewasm_hir::BuiltinSeries::Volume);
    let errors = validate(&bad).expect_err("volume is not a valid input.source choice");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ValidationError::Input { .. })),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_rejects_host_only_external_source_in_program_metadata() {
    let program = compile_to_csil("indicator(\"I\")\nsrc = input.source(close)\nplot(src)\n");
    let mut bad = program.clone();
    bad.inputs[0].defval = crate::InputValue::ExternalSource(pinewasm_hir::ExternalSourceRef {
        script_id: "other-script".to_string(),
        plot_slot: 0,
        plot_label: "Plot".to_string(),
    });
    let errors = validate(&bad).expect_err("external sources are host overrides, not defaults");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ValidationError::Input { .. })),
        "errors: {errors:?}"
    );
}

#[test]
fn validator_accepts_and_checks_input_active_dependencies() {
    let program = compile_to_csil(
        "indicator(\"I\")\nenabled = input.bool(true)\nn = input.int(7, active=not enabled)\nplot(n)\n",
    );
    validate(&program).expect("prior bool active dependency must validate");
    assert_eq!(program.inputs[1].active.dependencies, vec![0]);
    assert_eq!(
        program.inputs[1].active.direct,
        Some(crate::InputActiveDirect {
            input: 0,
            inverted: true
        })
    );

    let mut bad = program.clone();
    bad.inputs[1].active.dependencies = vec![1];
    bad.inputs[1].active.direct = Some(crate::InputActiveDirect {
        input: 1,
        inverted: false,
    });
    let errors = validate(&bad).expect_err("self/future active dependency must fail validation");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("active dependency 1 must reference a prior input")),
        "errors: {errors:?}"
    );
}

// ---- CALL_COLLECTION ------------------------------------------------------

const ARRAY_CORE: &str =
    "indicator(\"C\")\na = array.new_float(0)\narray.push(a, 1.0)\nplot(array.size(a))\n";

#[test]
fn array_core_csil_snapshot() {
    let program = compile_to_csil(ARRAY_CORE);
    validate(&program).expect("array core must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn validator_rejects_bad_collection_argc() {
    use crate::CollectionFunc;
    let errors = corrupt(|p| {
        // array.new_float requires argc 2; 5 is out of range.
        p.functions[0].code.insert(
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArrayNewFloat,
                argc: 5,
            },
        );
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("out of range")),
        "{errors:?}"
    );
}

#[test]
fn validator_rejects_non_ref_receiver() {
    use crate::CollectionFunc;
    // array.size wants a Ref receiver; a pushed int on top is the wrong type.
    let errors = corrupt(|p| {
        let code = &mut p.functions[0].code;
        code.insert(0, CsilInstr::PushI64(7));
        code.insert(
            1,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArraySize,
                argc: 1,
            },
        );
        code.insert(2, CsilInstr::Pop);
        for i in 0..3 {
            p.functions[0]
                .source_map
                .insert(i, pinewasm_span::Span::dummy());
        }
    });
    assert!(
        errors.iter().any(|e| e.to_string().contains("must be")),
        "{errors:?}"
    );
}

// ---- map.* CALL_COLLECTION -----------------------------------------------

const MAP_CORE: &str =
    "indicator(\"C\")\nm = map.new_string_float()\np = map.put(m, \"a\", 1.0)\nplot(map.size(m))\n";

#[test]
fn map_core_csil_snapshot() {
    let program = compile_to_csil(MAP_CORE);
    validate(&program).expect("map core must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn validator_rejects_bad_map_argc() {
    use crate::CollectionFunc;
    let errors = corrupt(|p| {
        // map.put requires argc 3; 1 is out of range.
        p.functions[0].code.insert(
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::MapPut,
                argc: 1,
            },
        );
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("out of range")),
        "{errors:?}"
    );
}

// ---- matrix.* CALL_COLLECTION --------------------------------------------

const MATRIX_CORE: &str =
    "indicator(\"M\")\nm = matrix.new_float(2, 2, 0.0)\nmatrix.set(m, 0, 0, 1.0)\nplot(matrix.get(m, 0, 0))\n";

#[test]
fn matrix_core_csil_snapshot() {
    let program = compile_to_csil(MATRIX_CORE);
    validate(&program).expect("matrix core must validate");
    insta::assert_snapshot!(dump_csil(&program));
}

#[test]
fn validator_rejects_bad_matrix_argc() {
    use crate::CollectionFunc;
    let errors = corrupt(|p| {
        // matrix.new requires argc 4; 2 is out of range.
        p.functions[0].code.insert(
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::MatrixNew,
                argc: 2,
            },
        );
        p.functions[0]
            .source_map
            .insert(0, pinewasm_span::Span::dummy());
    });
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("out of range")),
        "{errors:?}"
    );
}

#[test]
fn validator_rejects_non_ref_matrix_receiver() {
    use crate::CollectionFunc;
    // matrix.rows wants a Ref receiver; a pushed int is the wrong type.
    let errors = corrupt(|p| {
        let code = &mut p.functions[0].code;
        code.insert(0, CsilInstr::PushI64(7));
        code.insert(
            1,
            CsilInstr::CallCollection {
                func: CollectionFunc::MatrixRows,
                argc: 1,
            },
        );
        code.insert(2, CsilInstr::Pop);
        for i in 0..3 {
            p.functions[0]
                .source_map
                .insert(i, pinewasm_span::Span::dummy());
        }
    });
    assert!(
        errors.iter().any(|e| e.to_string().contains("must be")),
        "{errors:?}"
    );
}

#[test]
fn matrix_program_round_trips_through_json() {
    let program = compile_to_csil(MATRIX_CORE);
    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("round-trip");
    assert_eq!(program, back, "matrix CALL_COLLECTION must survive JSON");
}

#[test]
fn matrix_structural_array_results_round_trip_through_json() {
    let source = "indicator(\"M\")\n\
                  m = matrix.new<int>(2, 2, 7)\n\
                  row = m.row(0)\n\
                  removed = matrix.remove_col(m, 1)\n\
                  plot(row.get(0) + removed.get(0))\n";
    let program = compile_to_csil(source);
    validate(&program).expect("matrix structural array results must validate");
    let calls: Vec<CollectionFunc> = program.functions[0]
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            CsilInstr::CallCollection { func, .. } => Some(*func),
            _ => None,
        })
        .collect();
    assert!(calls.contains(&CollectionFunc::MatrixRow), "{calls:?}");
    assert!(
        calls.contains(&CollectionFunc::MatrixRemoveCol),
        "{calls:?}"
    );

    let json = program.to_json();
    let back = CsilProgram::from_json(&json).expect("round-trip");
    assert_eq!(
        program, back,
        "matrix array-returning calls must survive JSON"
    );
    validate(&back).expect("round-tripped structural array results must validate");
}

#[test]
fn unused_literal_stores_are_removed_but_input_metadata_survives() {
    let program = compile_to_csil(concat!(
        "indicator('Dead constants')\n",
        "group = 'Configuration'\nn = input.int(3, 'Length', group=group)\n",
        "unused = 7.0\nplot(close * n)\n",
    ));
    validate(&program).unwrap();
    let group = program
        .series_slots
        .iter()
        .position(|slot| slot.name == "group")
        .unwrap() as u32;
    let unused = program
        .series_slots
        .iter()
        .position(|slot| slot.name == "unused")
        .unwrap() as u32;
    assert!(!program
        .main()
        .code
        .iter()
        .any(|instruction| matches!(instruction,
        CsilInstr::StoreUserSlotCurrent { slot } if *slot == group || *slot == unused)));
    assert_eq!(program.inputs[0].group.as_deref(), Some("Configuration"));
    assert!(program
        .main()
        .code
        .iter()
        .any(|instruction| matches!(instruction, CsilInstr::ReadInput { .. })));
    assert_eq!(program.main().code.len(), program.main().source_map.len());
}

#[test]
fn observable_constants_and_nonliteral_unused_producers_are_preserved() {
    let program = compile_to_csil(concat!(
        "indicator('Preserve observers')\nx = 2.0\nvar count = 0\n",
        "count += 1\na = array.new_float(1, close)\nn = input.int(3, 'Length')\n",
        "plot(x[1])\nplot(count)\n",
    ));
    validate(&program).unwrap();
    for name in ["x", "count", "a", "n"] {
        let slot = program
            .series_slots
            .iter()
            .position(|slot| slot.name == name)
            .unwrap() as u32;
        assert!(
            program
                .main()
                .code
                .iter()
                .any(|instruction| matches!(instruction,
            CsilInstr::StoreUserSlotCurrent { slot: target } if *target == slot)),
            "{name}"
        );
    }
}

#[test]
fn dead_literal_transfer_remapping_preserves_loop_and_lazy_branch_stacks() {
    let program = compile_to_csil(concat!(
        "indicator('Remap branches')\nvar count = 0\n",
        "for i = 0 to 2\n    unused = 'loop'\n    if i > 0 and close > 0\n",
        "        dead = 9.0\n        count += i\n    else\n        other = 'else'\n",
        "plot(count)\n",
    ));
    validate(&program).unwrap();
    assert!(program
        .main()
        .code
        .iter()
        .any(|instruction| matches!(instruction, CsilInstr::JumpIfExact { .. })));
    assert_eq!(program.main().code.len(), program.main().source_map.len());
}

#[test]
fn input_history_round_trips_and_rejects_malformed_bindings() {
    use pinewasm_hir::HtfExpr;
    let program = compile_to_csil("//@version=6\nindicator(\"S\")\nn = input.int(1)\nplot(request.security(syminfo.tickerid, \"1W\", close[n]))\n");
    validate(&program).expect("input history must validate");
    let restored = CsilProgram::from_json(&program.to_json()).expect("input history JSON");
    validate(&restored).expect("restored input history must validate");
    assert_eq!(program.to_json(), restored.to_json());
    for mutation in 0..5 {
        let mut corrupt = program.clone();
        let mut found = false;
        for instr in &mut corrupt.functions[0].code {
            if let CsilInstr::ReadSecurityExpr {
                expr:
                    HtfExpr::HistoryInput {
                        history_slot,
                        offset,
                        ..
                    },
                ..
            } = instr
            {
                match mutation {
                    0 => *history_slot = u32::MAX,
                    1 => {
                        *offset = pinewasm_hir::HtfHistoryOffset::InputInt {
                            input: u32::MAX,
                            default: 1,
                        }
                    }
                    2 => {
                        *offset = pinewasm_hir::HtfHistoryOffset::InputInt {
                            input: 0,
                            default: 2,
                        }
                    }
                    3 => {
                        *offset = pinewasm_hir::HtfHistoryOffset::Binary {
                            op: pinewasm_hir::BinOp::And,
                            lhs: Box::new(pinewasm_hir::HtfHistoryOffset::Int(1)),
                            rhs: Box::new(pinewasm_hir::HtfHistoryOffset::Int(2)),
                        }
                    }
                    _ => *offset = pinewasm_hir::HtfHistoryOffset::Float(f64::NAN),
                }
                found = true;
            }
        }
        assert!(found);
        assert!(
            validate(&corrupt).is_err(),
            "malformed input history {mutation}"
        );
    }
}

/// The diamond: `PushBool; JumpIfFalse 4; <fallthrough>; Jump 5;
/// <target>; PushF64; CmpEq; Pop; Return`.
fn typed_diamond(fallthrough: CsilInstr, target: CsilInstr) -> CsilProgram {
    let mut program = compile_to_csil("indicator(\"Typed diamond\")\n");
    let function = &mut program.functions[0];
    function.code = vec![
        CsilInstr::PushBool(true),
        CsilInstr::JumpIfFalse { target: 4 },
        fallthrough,
        CsilInstr::Jump { target: 5 },
        target,
        CsilInstr::PushF64(F64Bits::new(2.0)),
        CsilInstr::CmpEq,
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    function.source_map = vec![pinewasm_span::Span::dummy(); function.code.len()];
    function.max_stack = 2;
    program
}

#[test]
fn validator_typed_worklist_verdict_depends_on_visit_order() {
    // The worklist visits the fall-through arm first and checks `CmpEq` on
    // the stack recorded at that visit: `Int` on the fall-through arm is
    // rejected before the `F64` target arm widens the join at pc 5 to `Any`.
    let errors = validate(&typed_diamond(
        CsilInstr::PushI64(1),
        CsilInstr::PushF64(F64Bits::new(1.0)),
    ))
    .expect_err("Int on the first-visited arm is rejected");
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    assert_eq!(
        errors[0].to_string(),
        "function `main` at 0006: equality operands must have matching types, found F64 and Int"
    );
    // The mirrored arm assignment (same joined stack `[Any, F64]` at pc 6)
    // is accepted: the first check sees `F64` and the recheck sees `Any`.
    validate(&typed_diamond(
        CsilInstr::PushF64(F64Bits::new(1.0)),
        CsilInstr::PushI64(1),
    ))
    .expect("F64 on the first-visited arm is accepted");
}

#[path = "fraction_tests.rs"]
mod fraction_tests;

#[path = "fraction_table_tests.rs"]
mod fraction_table_tests;
