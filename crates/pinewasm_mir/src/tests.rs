use crate::{dump_mir, dump_mir_annotated, lower, MirBody, MirInstr, Value};
use pinewasm_hir::OrderKind;
use pinewasm_parse::parse;
use pinewasm_semantics::analyze;
use pinewasm_span::has_errors;

fn lower_source(source: &str) -> String {
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
    dump_mir(&lower(&analysis.hir))
}

#[test]
fn complete_strategy_order_operands_survive_mir_lowering_and_json() {
    let source = "strategy(\"S\")\n\
        strategy.entry(\"E\", strategy.long, qty=1, limit=2, stop=3, oca_name=\"g\", \
            oca_type=strategy.oca.cancel, comment=\"c\", alert_message=\"a\", disable_alert=true)\n\
        strategy.order(\"O\", strategy.short, qty=1, oca_name=\"g\", \
            oca_type=strategy.oca.reduce, comment=\"c\", alert_message=\"a\", disable_alert=true)\n\
        strategy.exit(\"X\", \"E\", qty=1, qty_percent=50, profit=2, limit=3, loss=4, \
            stop=5, trail_price=6, trail_points=7, trail_offset=8, oca_name=\"g\", \
            comment=\"c\", comment_profit=\"p\", comment_loss=\"l\", comment_trailing=\"t\", \
            alert_message=\"a\", alert_profit=\"ap\", alert_loss=\"al\", \
            alert_trailing=\"at\", disable_alert=true)\n\
        strategy.close(\"E\", comment=\"c\", qty=1, qty_percent=50, alert_message=\"a\", \
            immediately=true, disable_alert=true)\n\
        strategy.close_all(comment=\"c\", alert_message=\"a\", immediately=true, disable_alert=true)\n";
    let parsed = parse(source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
    let analysis = analyze(&parsed.program);
    assert!(
        !has_errors(&analysis.diagnostics),
        "{:?}",
        analysis.diagnostics
    );
    let mir = lower(&analysis.hir);

    let orders: Vec<(OrderKind, usize)> = mir
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter_map(|instr| match instr {
            MirInstr::Order { kind, operands, .. } => Some((*kind, operands.len())),
            _ => None,
        })
        .collect();
    assert_eq!(
        orders,
        vec![
            (OrderKind::Entry, 10),
            (OrderKind::Order, 10),
            (OrderKind::Exit, 21),
            (OrderKind::Close, 7),
            (OrderKind::CloseAll, 4),
        ]
    );
    for (kind, count) in orders {
        assert_eq!(usize::from(kind.operand_count()), count);
    }

    let json = serde_json::to_string(&mir).expect("serialize MIR");
    let back: MirBody = serde_json::from_str(&json).expect("deserialize MIR");
    let operand_layouts = |body: &MirBody| {
        body.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|instr| match instr {
                MirInstr::Order { kind, operands, .. } => Some((*kind, operands.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        operand_layouts(&back),
        operand_layouts(&mir),
        "every order operand ValueId must survive MIR JSON"
    );
}

#[test]
fn risk_directives_are_hoisted_once_before_body_with_local_simple_values() {
    let source = r#"//@version=6
strategy("Global risk")
if false
    local_cap = 3.0
    strategy.risk.max_position_size(local_cap)
    strategy.risk.allow_entry_in(strategy.direction.long)
    strategy.risk.max_drawdown(25, strategy.percent_of_equity, "drawdown")
    strategy.risk.max_intraday_loss(100, strategy.cash, "intraday")
    strategy.risk.max_intraday_filled_orders(4, "fills")
    strategy.risk.max_cons_loss_days(2, "days")
strategy.entry("L", strategy.long, qty = 10)
"#;
    let parsed = parse(source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
    let analysis = analyze(&parsed.program);
    assert!(
        !has_errors(&analysis.diagnostics),
        "{:?}",
        analysis.diagnostics
    );
    let mir = lower(&analysis.hir);

    let entry = &mir.blocks[mir.entry.0 as usize];
    let entry_risks = entry
        .instrs
        .iter()
        .filter_map(|instr| match instr {
            MirInstr::Order { kind, .. } if kind.is_risk() => Some(*kind),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        entry_risks,
        vec![
            OrderKind::RiskMaxPositionSize,
            OrderKind::RiskAllowEntryIn,
            OrderKind::RiskMaxDrawdown,
            OrderKind::RiskMaxIntradayLoss,
            OrderKind::RiskMaxIntradayFilledOrders,
            OrderKind::RiskMaxConsLossDays,
        ],
        "all risk rules must execute in source order in the entry prelude"
    );

    let all_risks = mir
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter(|instr| matches!(instr, MirInstr::Order { kind, .. } if kind.is_risk()))
        .count();
    assert_eq!(
        all_risks, 6,
        "the original branch-local risk opcodes must be suppressed"
    );

    let cap_operand = entry
        .instrs
        .iter()
        .find_map(|instr| match instr {
            MirInstr::Order {
                kind: OrderKind::RiskMaxPositionSize,
                operands,
                ..
            } => Some(operands[0]),
            _ => None,
        })
        .expect("hoisted max-position rule");
    assert!(
        entry.instrs.iter().any(|instr| {
            matches!(
                instr,
                MirInstr::Const {
                    dst,
                    value: Value::Float(3.0),
                    ..
                } if *dst == cap_operand
            )
        }),
        "the false-branch local simple value must be substituted into the prelude"
    );
}

#[test]
fn hoisted_risk_value_preserves_simple_if_expression_merges() {
    let source = r#"//@version=6
strategy("Simple risk expression")
use_small = input.bool(false)
cap = if use_small
    3.0
else
    4.0
if false
    strategy.risk.max_position_size(cap)
strategy.entry("L", strategy.long, qty = 10)
"#;
    let parsed = parse(source);
    assert!(!has_errors(&parsed.diagnostics), "{:?}", parsed.diagnostics);
    let analysis = analyze(&parsed.program);
    assert!(
        !has_errors(&analysis.diagnostics),
        "{:?}",
        analysis.diagnostics
    );
    let mir = lower(&analysis.hir);
    let entry = &mir.blocks[mir.entry.0 as usize];
    let cap_operand = entry
        .instrs
        .iter()
        .find_map(|instr| match instr {
            MirInstr::Order {
                kind: OrderKind::RiskMaxPositionSize,
                operands,
                ..
            } => Some(operands[0]),
            _ => None,
        })
        .expect("hoisted max-position rule");
    assert!(
        entry
            .instrs
            .iter()
            .any(|instr| { matches!(instr, MirInstr::Select { dst, .. } if *dst == cap_operand) }),
        "the input-controlled simple if expression must become a prelude select"
    );
}

#[test]
fn annotated_dump_adds_span_comments_to_instructions() {
    let parsed = parse("//@version=6\nindicator(\"S\")\nx = close - close[1]\nplot(x)\n");
    let analysis = analyze(&parsed.program);
    let mir = lower(&analysis.hir);
    let plain = dump_mir(&mir);
    let annotated = dump_mir_annotated(&mir);

    // The plain dump has no span comments; the annotated one tags every
    // instruction and the terminator with a `; start..end` source map.
    assert!(!plain.contains(" ; "), "plain dump must stay span-free");
    let body: Vec<&str> = annotated
        .lines()
        .filter(|l| {
            l.starts_with("  ")
                && !l.starts_with("  slot")
                && !l.starts_with("  plot ")
                && !l.starts_with("  input")
                && !l.trim_start().starts_with("color ")
                && !l.starts_with("  ta_state")
        })
        .collect();
    assert!(!body.is_empty());
    for line in &body {
        assert!(
            line.contains(" ; ") && line.rsplit(" ; ").next().unwrap().contains(".."),
            "instruction/terminator line missing span: {line:?}"
        );
    }
    // Stripping the `  ; span` comments must reproduce the plain dump exactly.
    let stripped: String = annotated
        .lines()
        .map(|l| match l.rsplit_once("  ; ") {
            Some((code, span)) if span.contains("..") => code,
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(format!("{stripped}\n"), plain);
}

#[test]
fn core_example_mir_snapshot() {
    insta::assert_snapshot!(lower_source(
        "//@version=6\nindicator(\"Example\")\nx = close - close[1]\nplot(x)\n"
    ));
}

#[test]
fn if_else_mir_snapshot() {
    insta::assert_snapshot!(lower_source(
        "indicator(\"Branch\")\nvalue = 0.0\nif close > open\n    value := close\nelse\n    value := open\nplot(value)\n"
    ));
}

#[test]
fn int_arithmetic_mir_snapshot() {
    // Int constants become `const <i64>`; `%`, negation, and `/` retain the
    // static int flavor. The mixed addition to `0.5` widens `x` to float.
    insta::assert_snapshot!(lower_source(
        "indicator(\"Ints\")\nx = 7 % 3 + -4 / 2\ny = x + 0.5\nplot(x)\n"
    ));
}

#[test]
fn var_counter_mir_snapshot() {
    // A persistent declaration lowers to an initialize-once diamond
    // (branch_if_slot_written); the `:=` reassignment is a plain store.
    insta::assert_snapshot!(lower_source(
        "indicator(\"Var\")\nvar n = 0\nn := n + 1\nplot(n)\n"
    ));
}

#[test]
fn compound_history_mir_snapshot() {
    // `(close - open)[1]` materializes through a hidden slot: evaluate the
    // expression, store it into the current bar's cell of `%h0`, then read
    // the slot at offset 1. No new instruction kinds are involved.
    insta::assert_snapshot!(lower_source(
        "indicator(\"Compound\")\nx = (close - open)[1]\nplot(x)\n"
    ));
}

#[test]
fn compound_history_lowers_to_hidden_slot_store_then_read() {
    let dump = lower_source("indicator(\"CH\")\nx = (close - open)[1]\nplot(x)\n");
    // The hidden slot is slot 0 (allocated while analyzing the value
    // expression, before `x` is declared) and is not persistent.
    assert!(dump.contains("slot 0 `%h0`"), "dump:\n{dump}");
    assert!(!dump.contains("`%h0` var"), "dump:\n{dump}");
    assert!(dump.contains("store_user_current slot0"), "dump:\n{dump}");
    assert!(dump.contains("read_user slot0[1]"), "dump:\n{dump}");
}

#[test]
fn ta_change_sma_mir_snapshot() {
    // ta.change lowers to store + two reads + a float subtraction (no new
    // instruction); ta.sma lowers to store + window_mean. Each call site
    // owns its own hidden slot.
    insta::assert_snapshot!(lower_source(
        "indicator(\"TA\")\nplot(ta.change(close))\nplot(ta.sma(close, 2))\n"
    ));
}

#[test]
fn ta_change_lowers_to_store_and_offset_reads() {
    let dump = lower_source("indicator(\"TA\")\nplot(ta.change(close, 2))\n");
    assert!(dump.contains("slot 0 `%h0`"), "dump:\n{dump}");
    assert!(dump.contains("store_user_current slot0"), "dump:\n{dump}");
    assert!(dump.contains("read_user slot0[0]"), "dump:\n{dump}");
    assert!(dump.contains("read_user slot0[2]"), "dump:\n{dump}");
    assert!(dump.contains("binary `-`"), "dump:\n{dump}");
    assert!(!dump.contains("window_mean"), "dump:\n{dump}");
}

#[test]
fn ta_sma_lowers_to_store_and_window_mean() {
    let dump = lower_source("indicator(\"TA\")\nplot(ta.sma(close - open, 3))\n");
    assert!(dump.contains("store_user_current slot0"), "dump:\n{dump}");
    assert!(dump.contains("window_mean slot0 len=3"), "dump:\n{dump}");
    // The hidden slot is never persistent.
    assert!(!dump.contains("`%h0` var"), "dump:\n{dump}");
}

#[test]
fn pure_builtins_mir_snapshot() {
    // Pure calls become one call_pure instruction consuming argument
    // registers in order; the na literal is a plain `const na`; constants
    // lower to float constants.
    insta::assert_snapshot!(lower_source(
        "indicator(\"Pure\")\nplot(math.min(1, 2, close))\nplot(nz(close + na))\nplot(math.pi)\n"
    ));
}

#[test]
fn pure_call_lowers_to_call_pure_over_argument_registers() {
    let dump = lower_source("indicator(\"P\")\nplot(math.pow(close, 2))\n");
    assert!(
        dump.contains("call_pure math.pow"),
        "expected a call_pure instruction:\n{dump}"
    );
}

#[test]
fn ternary_mir_snapshot() {
    // Dynamic ternary arms lower to a branch diamond so only the selected arm
    // executes at runtime.
    insta::assert_snapshot!(lower_source(
        "indicator(\"Ternary\")\nplot(close > open ? close : open)\n"
    ));
}

#[test]
fn ternary_with_dynamic_arms_lowers_to_branch_diamond() {
    let dump = lower_source("indicator(\"T\")\nx = close > open ? close : open\nplot(x)\n");
    assert_eq!(block_header_count(&dump), 4, "unexpected blocks:\n{dump}");
    assert!(dump.contains("v2 = binary `>` v0, v1"), "dump:\n{dump}");
    assert!(dump.contains("branch v2, bb1, bb2"), "dump:\n{dump}");
    assert!(
        dump.contains("store_user_current slot1, v3"),
        "dump:\n{dump}"
    );
    assert!(
        dump.contains("store_user_current slot1, v4"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("v5 = read_user slot1[0]"), "dump:\n{dump}");
}

#[test]
fn ternary_with_constant_arms_keeps_select() {
    // Literal arms and unary operators on literals cannot fail, so eager
    // `Select` stays observationally equivalent and keeps artifacts stable.
    for source in [
        "indicator(\"T\")\nplot(close > open ? 1 : 0)\n",
        "indicator(\"T\")\nplot(close > open ? 1 : -1)\n",
    ] {
        let dump = lower_source(source);
        assert_eq!(block_header_count(&dump), 1, "unexpected blocks:\n{dump}");
        assert!(dump.contains("select"), "dump:\n{dump}");
        assert!(!dump.contains("store_user_current"), "dump:\n{dump}");
    }
}

fn block_header_count(dump: &str) -> usize {
    dump.lines()
        .filter(|l| l.starts_with("bb") && l.ends_with(':'))
        .count()
}

#[test]
fn straight_line_mir_has_single_block() {
    let dump = lower_source("indicator(\"S\")\nx = close * 2\nplot(x)\n");
    assert_eq!(block_header_count(&dump), 1, "unexpected blocks:\n{dump}");
    assert!(dump.trim_end().ends_with("return"));
}

#[test]
fn if_lowering_produces_diamond() {
    let dump = lower_source(
        "indicator(\"D\")\nvalue = open\nif close > open\n    value := close\nplot(value)\n",
    );
    // entry + then + else + join = 4 blocks
    assert_eq!(block_header_count(&dump), 4, "expected a diamond:\n{dump}");
    assert!(dump.contains("branch v"));
}

#[test]
fn var_declaration_produces_written_flag_diamond() {
    let dump = lower_source("indicator(\"V\")\nvar seed = close\nplot(seed)\n");
    // entry + unwritten + written + join = 4 blocks.
    assert_eq!(block_header_count(&dump), 4, "expected a diamond:\n{dump}");
    assert!(
        dump.contains("branch_if_slot_written slot0"),
        "dump:\n{dump}"
    );
    // The slot is marked persistent in the metadata.
    assert!(dump.contains("slot 0 `seed` var"), "dump:\n{dump}");
    // The initializer is evaluated only on the unwritten path.
    assert!(dump.contains("read_builtin close[0]"), "dump:\n{dump}");
}

#[test]
fn reassignment_lowers_to_a_plain_store() {
    // Non-var declaration + := reassignment: straight-line code, no diamond.
    let dump = lower_source("indicator(\"R\")\nx = 1\nx := 2\nplot(x)\n");
    assert_eq!(block_header_count(&dump), 1, "unexpected blocks:\n{dump}");
    assert!(!dump.contains("branch_if_slot_written"), "dump:\n{dump}");
    assert!(
        !dump.contains(" var"),
        "non-var slot marked persistent:\n{dump}"
    );
}

#[test]
fn ta_engine_mir_snapshot() {
    // Engine-backed calls lower to one call_ta instruction over argument
    // registers; the const params live in the metadata's ta_states table
    // (no hidden slots, no stores).
    insta::assert_snapshot!(lower_source(
        "indicator(\"Engine\")\nplot(ta.ema(close, 9))\nplot(ta.crossover(close, open) ? 1.0 : 0.0)\n"
    ));
}

#[test]
fn ta_engine_call_lowers_to_call_ta_without_hidden_slots() {
    let dump = lower_source("indicator(\"E\")\nplot(math.sum(close, 3))\n");
    assert!(
        dump.contains("ta_state 0 math.sum params=[3]"),
        "dump:\n{dump}"
    );
    assert!(dump.contains("call_ta state0"), "dump:\n{dump}");
    // No hidden slot machinery: the engine owns the state.
    assert!(!dump.contains("%h0"), "dump:\n{dump}");
    assert!(!dump.contains("store_user_current"), "dump:\n{dump}");
}

#[test]
fn for_loop_mir_snapshot() {
    // A for loop lowers to header/body/step/exit blocks with a back-edge from
    // the step block to the header (zero new instructions).
    insta::assert_snapshot!(lower_source(
        "indicator(\"L\")\nvar s = 0\ns := 0\nfor i = 1 to 10\n    s := s + i\nplot(s)\n"
    ));
}

#[test]
fn while_loop_with_break_continue_mir_snapshot() {
    // `break` jumps to the exit block, `continue` to the header.
    insta::assert_snapshot!(lower_source(
        "indicator(\"W\")\nvar i = 0\ni := 0\nwhile i < 10\n    if i == 5\n        break\n    i := i + 1\n    continue\nplot(i)\n"
    ));
}

#[test]
fn tuple_destructure_mir_snapshot() {
    // A tuple-returning ta.macd destructure: one multi-dst call_ta feeding
    // three stores (popped in reverse so slot0 gets the first-pushed value).
    insta::assert_snapshot!(lower_source(
        "//@version=6\nindicator(\"T\")\n[m, s, h] = ta.macd(close, 3, 7, 4)\nplot(m)\nplot(h)\n"
    ));
}

#[test]
fn array_core_mir_snapshot() {
    insta::assert_snapshot!(lower_source(
        "indicator(\"C\")\na = array.new_float(0)\narray.push(a, 1.0)\nplot(array.size(a))\n"
    ));
}
