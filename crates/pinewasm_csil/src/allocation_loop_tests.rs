use super::*;
use pinewasm_hir::TaInputTarget;
fn original(code: &Vec<CsilInstr>) -> Option<usize> {
    fn htf_boxes(expr: &HtfExpr) -> Option<usize> {
        let mut bytes = 0usize;
        let mut pending = vec![expr];
        while let Some(node) = pending.pop() {
            match node {
                HtfExpr::BoolResult(inner) => {
                    bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                    pending.push(inner);
                }
                HtfExpr::HistoryInput { offset, source, .. } => {
                    bytes = bytes
                        .checked_add(offset.allocated_bytes()?)?
                        .checked_add(std::mem::size_of::<HtfExpr>())?;
                    pending.push(source);
                }
                HtfExpr::History { source, .. }
                | HtfExpr::HistoryRealtime { source, .. }
                | HtfExpr::Neg(source)
                | HtfExpr::Ta { source, .. }
                | HtfExpr::TaInput { source, .. } => {
                    bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                    pending.push(source);
                }
                HtfExpr::EnumSelect {
                    when_equal,
                    otherwise,
                    ..
                } => {
                    bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                    pending.push(when_equal);
                    if let Some(otherwise) = otherwise {
                        bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                        pending.push(otherwise);
                    }
                }
                HtfExpr::Binary { lhs, rhs, .. } => {
                    bytes = bytes.checked_add(2 * std::mem::size_of::<HtfExpr>())?;
                    pending.push(lhs);
                    pending.push(rhs);
                }
                HtfExpr::Const(_)
                | HtfExpr::Source(_)
                | HtfExpr::InputSource { .. }
                | HtfExpr::BarstateIslast
                | HtfExpr::BarstateIsRealtime => {}
            }
            if bytes > MAX_BUDGET_REFERENCE_BYTES {
                return None;
            }
        }
        Some(bytes)
    }
    let mut bytes = code
        .capacity()
        .checked_mul(std::mem::size_of::<CsilInstr>())?;
    for instr in code {
        let extra = match instr {
            CsilInstr::ReadSecurity {
                selector,
                timeframe_expr,
                ..
            } => selector
                .as_ref()
                .map_or(0, String::capacity)
                .checked_add(timeframe_expr.as_ref().map_or(0, String::capacity))?,
            CsilInstr::ReadRequest { selector, .. }
            | CsilInstr::BuildFootprint { selector }
            | CsilInstr::ReadSecurityLowerTf { selector } => selector.capacity(),
            CsilInstr::SetCell { attrs } => attrs
                .capacity()
                .checked_mul(std::mem::size_of::<CellAttr>())?,
            CsilInstr::ReadSecurityExpr {
                expr,
                timeframe_expr,
                ..
            } => {
                htf_boxes(expr)?.checked_add(timeframe_expr.as_ref().map_or(0, String::capacity))?
            }
            _ => 0,
        };
        bytes = bytes.checked_add(extra)?;
    }
    Some(bytes)
}

#[test]
fn allocation_loop_capacity_and_payload_differential() {
    let mut selector = String::with_capacity(97);
    selector.push_str("security");
    let cases = [
        CsilInstr::PushI64(3),
        CsilInstr::BuildFootprint {
            selector: selector.clone(),
        },
        CsilInstr::BuildFootprint {
            selector: selector.clone(),
        },
        CsilInstr::ReadSecurityLowerTf { selector },
    ];
    let extra_cases = [
        CsilInstr::ReadSecurity {
            selector: None,
            source: BuiltinSeries::Close,
            htf_seconds: 60,
            timeframe_expr: None,
            calendar_months: None,
            gaps_on: false,
            lookahead_on: false,
        },
        CsilInstr::ReadSecurity {
            selector: Some(String::with_capacity(31)),
            source: BuiltinSeries::Close,
            htf_seconds: 60,
            timeframe_expr: Some(String::with_capacity(43)),
            calendar_months: None,
            gaps_on: false,
            lookahead_on: false,
        },
        CsilInstr::SetCell {
            attrs: Vec::with_capacity(19),
        },
    ];
    for instr in extra_cases {
        let code = vec![instr];
        assert_eq!(csil_code_allocated_bytes(&code), original(&code));
    }
    let selector = String::with_capacity(31);
    let timeframe = String::with_capacity(43);
    assert!(selector.capacity() >= 31 && timeframe.capacity() >= 43);
    let expected = selector.capacity() + timeframe.capacity();
    assert_eq!(
        instruction_bytes(
            &CsilInstr::ReadSecurity {
                selector: Some(selector),
                source: BuiltinSeries::Close,
                htf_seconds: 60,
                timeframe_expr: Some(timeframe),
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            },
            0
        ),
        Some(expected)
    );
    let attrs = Vec::with_capacity(19);
    assert!(attrs.capacity() >= 19);
    let expected = attrs.capacity() * std::mem::size_of::<CellAttr>();
    assert_eq!(
        instruction_bytes(&CsilInstr::SetCell { attrs }, 0),
        Some(expected)
    );
    let reserved = Vec::with_capacity(19);
    assert!(reserved.capacity() >= 19);
    assert_eq!(
        csil_code_allocated_bytes(&reserved),
        Some(reserved.capacity() * std::mem::size_of::<CsilInstr>())
    );
    let outer_large = vec![CsilInstr::BuildFootprint {
        selector: String::with_capacity(MAX_BUDGET_REFERENCE_BYTES + 1),
    }];
    assert_eq!(
        csil_code_allocated_bytes(&outer_large),
        original(&outer_large)
    );
    assert!(csil_code_allocated_bytes(&outer_large).unwrap() > MAX_BUDGET_REFERENCE_BYTES);
    for capacity in [0, 1, 4, 17, 101] {
        for len in 0..=40 {
            let mut code = Vec::with_capacity(capacity);
            for i in 0..len {
                code.push(cases[i % cases.len()].clone());
            }
            assert_eq!(csil_code_allocated_bytes(&code), original(&code));
        }
    }
}
#[test]
fn allocation_loop_nested_htf_differential() {
    for depth in 0..=64 {
        let mut expr = HtfExpr::Const(1.0);
        for i in 0..depth {
            expr = match i % 3 {
                0 => HtfExpr::Neg(Box::new(expr)),
                1 => HtfExpr::EnumSelect {
                    input: 0,
                    index: i,
                    when_equal: Box::new(expr),
                    otherwise: (i % 2 == 0).then(|| Box::new(HtfExpr::Const(3.0))),
                },
                _ => HtfExpr::Binary {
                    op: pinewasm_hir::BinOp::Add,
                    int_result: false,
                    lhs: Box::new(HtfExpr::Const(2.0)),
                    rhs: Box::new(expr),
                },
            };
        }
        // Compare exact reserved child-box bytes against the direct walk defined below.
        fn original_walk(expr: &HtfExpr) -> Option<usize> {
            let mut bytes = 0usize;
            let mut pending = vec![expr];
            while let Some(node) = pending.pop() {
                match node {
                    HtfExpr::Neg(x) => {
                        bytes = bytes.checked_add(std::mem::size_of::<HtfExpr>())?;
                        pending.push(x);
                    }
                    HtfExpr::EnumSelect {
                        when_equal,
                        otherwise,
                        ..
                    } => {
                        let boxes = 1 + usize::from(otherwise.is_some());
                        bytes = bytes.checked_add(boxes * std::mem::size_of::<HtfExpr>())?;
                        pending.extend(
                            std::iter::once(when_equal.as_ref()).chain(otherwise.as_deref()),
                        );
                    }
                    HtfExpr::Binary { lhs, rhs, .. } => {
                        bytes = bytes.checked_add(2 * std::mem::size_of::<HtfExpr>())?;
                        pending.push(lhs);
                        pending.push(rhs);
                    }
                    _ => {}
                }
                if bytes > MAX_BUDGET_REFERENCE_BYTES {
                    return None;
                }
            }
            Some(bytes)
        }
        assert_eq!(htf_boxes(&expr), original_walk(&expr));
        let code = vec![CsilInstr::ReadSecurityExpr {
            expr: expr.clone(),
            htf_seconds: 60,
            timeframe_expr: Some(String::with_capacity(23)),
            calendar_months: None,
            gaps_on: false,
            lookahead_on: false,
        }];
        assert_eq!(csil_code_allocated_bytes(&code), original(&code));
    }
}
#[test]
fn allocation_loop_all_htf_variants_differential() {
    let leaf = || Box::new(HtfExpr::Const(1.0));
    let variants = vec![
        HtfExpr::BoolResult(leaf()),
        HtfExpr::Const(1.0),
        HtfExpr::Source(BuiltinSeries::Close),
        HtfExpr::InputSource { input: 0 },
        HtfExpr::BarstateIslast,
        HtfExpr::BarstateIsRealtime,
        HtfExpr::History {
            history_slot: 0,
            offset: 2,
            source: leaf(),
        },
        HtfExpr::HistoryInput {
            history_slot: 0,
            offset: pinewasm_hir::HtfHistoryOffset::Neg(Box::new(
                pinewasm_hir::HtfHistoryOffset::Int(2),
            )),
            source: leaf(),
        },
        HtfExpr::HistoryRealtime {
            history_slot: 0,
            history_offset: 2,
            realtime_offset: 1,
            source: leaf(),
        },
        HtfExpr::Neg(leaf()),
        HtfExpr::Binary {
            op: pinewasm_hir::BinOp::Add,
            int_result: false,
            lhs: leaf(),
            rhs: leaf(),
        },
        HtfExpr::Ta {
            func: TaFunc::Sma,
            ta_slot: 0,
            length: 3,
            source: leaf(),
        },
        HtfExpr::TaInput {
            func: TaFunc::Sma,
            ta_slot: 0,
            input: 0,
            binding_target: TaInputTarget::Sma { slot: 0 },
            default_length: 3,
            source: leaf(),
        },
    ];
    for expr in variants {
        for reserve in [0, 17, 101] {
            let mut code = Vec::with_capacity(reserve);
            code.push(CsilInstr::ReadSecurityExpr {
                expr: expr.clone(),
                htf_seconds: 60,
                timeframe_expr: Some(String::with_capacity(29)),
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            });
            assert_eq!(csil_code_allocated_bytes(&code), original(&code));
        }
    }
    use pinewasm_hir::HtfHistoryOffset;
    let offsets = [
        HtfHistoryOffset::CastInt(Box::new(HtfHistoryOffset::Float(2.5))),
        HtfHistoryOffset::CastFloat(Box::new(HtfHistoryOffset::Int(2))),
        HtfHistoryOffset::Binary {
            op: pinewasm_hir::BinOp::Add,
            lhs: Box::new(HtfHistoryOffset::Int(1)),
            rhs: Box::new(HtfHistoryOffset::Int(2)),
        },
    ];
    for offset in offsets {
        let code = vec![CsilInstr::ReadSecurityExpr {
            expr: HtfExpr::HistoryInput {
                history_slot: 0,
                offset,
                source: leaf(),
            },
            htf_seconds: 60,
            timeframe_expr: None,
            calendar_months: None,
            gaps_on: false,
            lookahead_on: false,
        }];
        assert_eq!(csil_code_allocated_bytes(&code), original(&code));
    }
    let mut pending = Vec::new();
    let lhs = HtfExpr::Const(1.0);
    let rhs = HtfExpr::Const(2.0);
    let pair = HtfExpr::Binary {
        op: pinewasm_hir::BinOp::Add,
        int_result: false,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    };
    assert_eq!(
        htf_step(&pair, &mut pending, 0),
        Some(2 * std::mem::size_of::<HtfExpr>())
    );
    assert!(matches!(pending.pop(), Some(HtfExpr::Const(x)) if *x == 2.0));
    assert!(matches!(pending.pop(), Some(HtfExpr::Const(x)) if *x == 1.0));
    let request = vec![CsilInstr::ReadRequest {
        func: RequestFunc::Dividends,
        selector: String::with_capacity(51),
    }];
    assert_eq!(csil_code_allocated_bytes(&request), original(&request));
}
#[test]
fn allocation_loop_overflow_limit_and_failure_push_order() {
    let leaf = HtfExpr::Const(1.0);
    let branch = HtfExpr::Neg(Box::new(leaf.clone()));
    let mut pending = Vec::new();
    assert_eq!(htf_step(&branch, &mut pending, usize::MAX), None);
    assert!(pending.is_empty(), "checked overflow precedes child push");
    assert_eq!(
        htf_step(&leaf, &mut pending, MAX_BUDGET_REFERENCE_BYTES),
        Some(MAX_BUDGET_REFERENCE_BYTES)
    );
    assert_eq!(
        htf_step(&leaf, &mut pending, MAX_BUDGET_REFERENCE_BYTES + 1),
        None
    );
    assert_eq!(
        htf_step(&branch, &mut pending, MAX_BUDGET_REFERENCE_BYTES),
        None
    );
    assert_eq!(
        pending.len(),
        1,
        "limit rejection follows successful arithmetic and child push"
    );
    let instr = CsilInstr::BuildFootprint {
        selector: String::with_capacity(3),
    };
    assert_eq!(instruction_bytes(&instr, usize::MAX), None);
    assert_eq!(
        instruction_bytes(&CsilInstr::PushI64(0), usize::MAX),
        Some(usize::MAX)
    );
}
