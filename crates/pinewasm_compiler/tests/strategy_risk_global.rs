//! End-to-end coverage for globally applied `strategy.risk.*` rules.

use pinewasm_compiler::{
    compile_and_run, CompileOptions, InputValue, MarketData, RunOptions, RuntimeErrorKind,
    StrategyOutcome,
};
use std::collections::HashMap;

fn data(bars: &[(f64, f64, f64, f64)]) -> MarketData {
    MarketData {
        open: bars.iter().map(|bar| bar.0).collect(),
        high: bars.iter().map(|bar| bar.1).collect(),
        low: bars.iter().map(|bar| bar.2).collect(),
        close: bars.iter().map(|bar| bar.3).collect(),
        volume: vec![1.0; bars.len()],
        ..MarketData::default()
    }
}

fn run_strategy(source: &str, bars: &[(f64, f64, f64, f64)]) -> StrategyOutcome {
    run_strategy_with_options(source, bars, &RunOptions::default())
}

fn run_strategy_with_options(
    source: &str,
    bars: &[(f64, f64, f64, f64)],
    options: &RunOptions,
) -> StrategyOutcome {
    let result = compile_and_run(source, &data(bars), CompileOptions::default(), options);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics for {source:?}: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors for {source:?}: {:?}",
        result.runtime_errors
    );
    result.strategy.expect("strategy outcome")
}

fn source(rule: &str, body: &str) -> String {
    format!(
        "//@version=6\n\
         strategy(\"Global risk\", initial_capital=10000, default_qty_value=1)\n\
         {rule}\n\
         {body}\n\
         plot(strategy.position_size)\n"
    )
}

fn assert_expected_effect(name: &str, outcome: &StrategyOutcome) {
    match name {
        "max_position_size" => assert_eq!(outcome.position.size, 3.0),
        "allow_entry_in" => {
            assert_eq!(outcome.position.size, 0.0);
            assert!(outcome.trades.is_empty());
        }
        "max_drawdown" => assert_eq!(
            outcome
                .trades
                .first()
                .and_then(|trade| trade.exit_id.as_deref()),
            Some("risk.max_drawdown")
        ),
        "max_intraday_loss" => assert_eq!(
            outcome
                .trades
                .first()
                .and_then(|trade| trade.exit_id.as_deref()),
            Some("risk.max_intraday_loss")
        ),
        "max_intraday_filled_orders" => assert_eq!(
            outcome
                .trades
                .first()
                .and_then(|trade| trade.exit_id.as_deref()),
            Some("risk.max_intraday_filled_orders")
        ),
        "max_cons_loss_days" => assert_eq!(
            outcome
                .trades
                .first()
                .and_then(|trade| trade.exit_id.as_deref()),
            Some("risk.max_cons_loss_days")
        ),
        other => panic!("unknown risk fixture {other}"),
    }
}

#[test]
fn all_risk_commands_apply_from_false_local_scopes() {
    struct Case {
        name: &'static str,
        top_level_rule: &'static str,
        hidden_rule: &'static str,
        body: &'static str,
        bars: &'static [(f64, f64, f64, f64)],
    }

    const FLAT: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
    ];
    const DRAWDOWN: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (8.0, 8.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    const INTRADAY_LOSS: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    const LOSS_DAYS: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 9.0, 9.0),
        (9.0, 9.0, 8.0, 8.0),
        (8.0, 8.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
    ];

    let cases = [
        Case {
            name: "max_position_size",
            top_level_rule: "cap = 3.0\nstrategy.risk.max_position_size(cap)",
            hidden_rule: "if false\n    cap = 3.0\n    strategy.risk.max_position_size(cap)",
            body: "if bar_index == 0\n    strategy.entry(\"L\", strategy.long, qty=10)",
            bars: FLAT,
        },
        Case {
            name: "allow_entry_in",
            top_level_rule: "strategy.risk.allow_entry_in(strategy.direction.long)",
            hidden_rule: "if false\n    strategy.risk.allow_entry_in(strategy.direction.long)",
            body: "if bar_index == 0\n    strategy.entry(\"S\", strategy.short)",
            bars: FLAT,
        },
        Case {
            name: "max_drawdown",
            top_level_rule: "loss = 1.0\nstrategy.risk.max_drawdown(loss, strategy.cash)",
            hidden_rule: "if false\n    loss = 1.0\n    strategy.risk.max_drawdown(loss, strategy.cash)",
            body: "if bar_index == 0\n    strategy.entry(\"L\", strategy.long)\nif bar_index >= 2\n    strategy.entry(\"Again\", strategy.long)",
            bars: DRAWDOWN,
        },
        Case {
            name: "max_intraday_loss",
            top_level_rule: "loss = 1.0\nstrategy.risk.max_intraday_loss(loss, strategy.cash)",
            hidden_rule: "if false\n    loss = 1.0\n    strategy.risk.max_intraday_loss(loss, strategy.cash)",
            body: "strategy.entry(\"L\", strategy.long)",
            bars: INTRADAY_LOSS,
        },
        Case {
            name: "max_intraday_filled_orders",
            top_level_rule: "count = 1\nstrategy.risk.max_intraday_filled_orders(count)",
            hidden_rule: "if false\n    count = 1\n    strategy.risk.max_intraday_filled_orders(count)",
            body: "strategy.entry(\"L\", strategy.long)",
            bars: FLAT,
        },
        Case {
            name: "max_cons_loss_days",
            top_level_rule: "days = 2\nstrategy.risk.max_cons_loss_days(days)",
            hidden_rule: "if false\n    days = 2\n    strategy.risk.max_cons_loss_days(days)",
            body: "if bar_index == 0 or bar_index >= 3\n    strategy.entry(\"L\", strategy.long)",
            bars: LOSS_DAYS,
        },
    ];

    for case in cases {
        let top_level = run_strategy(&source(case.top_level_rule, case.body), case.bars);
        let hidden = run_strategy(&source(case.hidden_rule, case.body), case.bars);
        assert_eq!(
            hidden, top_level,
            "{} must have identical broker behavior from a false local scope",
            case.name
        );
        assert_expected_effect(case.name, &hidden);
    }
}

#[test]
fn risk_rule_reached_through_a_function_call_is_global_too() {
    let source = r#"//@version=6
strategy("Function risk", initial_capital=10000)
register_cap(cap) =>
    strategy.risk.max_position_size(cap)
    cap
if false
    ignored = register_cap(3.0)
if bar_index == 0
    strategy.entry("L", strategy.long, qty=10)
plot(strategy.position_size)
"#;
    let outcome = run_strategy(source, &[(10.0, 10.0, 10.0, 10.0); 3]);
    assert_eq!(
        outcome.position.size, 3.0,
        "an inlined function-local rule must apply even when its call is branch-hidden"
    );
}

#[test]
fn hoisted_risk_arguments_still_read_runtime_input_overrides() {
    let source = r#"//@version=6
strategy("Input risk", initial_capital=10000)
use_small = input.bool(false)
cap = if use_small
    3.0
else
    4.0
if false
    strategy.risk.max_position_size(cap)
if bar_index == 0
    strategy.entry("L", strategy.long, qty=10)
plot(strategy.position_size)
"#;
    let bars = [(10.0, 10.0, 10.0, 10.0); 3];
    assert_eq!(run_strategy(source, &bars).position.size, 4.0);

    let options = RunOptions {
        inputs: HashMap::from([(0, InputValue::Bool(true))]),
        ..RunOptions::default()
    };
    assert_eq!(
        run_strategy_with_options(source, &bars, &options)
            .position
            .size,
        3.0,
        "the global prelude must evaluate simple input-dependent arguments at run time"
    );
}

#[test]
fn risk_simple_string_inputs_are_resolved_after_overrides() {
    let direction_source = r#"//@version=6
strategy("Input direction", initial_capital=10000)
allowed = input.string("strategy.direction.long")
if false
    strategy.risk.allow_entry_in(allowed)
if bar_index == 0
    strategy.entry("S", strategy.short)
plot(strategy.position_size)
"#;
    let flat = [(10.0, 10.0, 10.0, 10.0); 3];
    assert_eq!(
        run_strategy(direction_source, &flat).position.size,
        0.0,
        "the default long-only input must block a short entry"
    );
    let allow_all = RunOptions {
        inputs: HashMap::from([(0, InputValue::String("strategy.direction.all".to_string()))]),
        ..RunOptions::default()
    };
    assert_eq!(
        run_strategy_with_options(direction_source, &flat, &allow_all)
            .position
            .size,
        -1.0,
        "the global directive must use the host's simple-string override"
    );

    let unit_source = r#"//@version=6
strategy("Input unit", initial_capital=10000, default_qty_value=1)
unit = input.string("strategy.cash")
if false
    strategy.risk.max_drawdown(1, unit)
if bar_index == 0
    strategy.entry("L", strategy.long)
if bar_index >= 2
    strategy.entry("Again", strategy.long)
plot(strategy.position_size)
"#;
    let drawdown = [
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (8.0, 8.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    assert_eq!(
        run_strategy(unit_source, &drawdown)
            .trades
            .first()
            .and_then(|trade| trade.exit_id.as_deref()),
        Some("risk.max_drawdown"),
        "a one-currency-unit drawdown must halt this fixture"
    );
    let percent = RunOptions {
        inputs: HashMap::from([(
            0,
            InputValue::String("strategy.percent_of_equity".to_string()),
        )]),
        ..RunOptions::default()
    };
    assert!(
        run_strategy_with_options(unit_source, &drawdown, &percent)
            .trades
            .iter()
            .all(|trade| trade.exit_id.as_deref() != Some("risk.max_drawdown")),
        "one percent of equity must not halt on the fixture's two-unit drawdown"
    );
}

#[test]
fn invalid_dynamic_risk_tokens_are_runtime_errors() {
    let cases = [
        (
            "allowed = input.string(\"strategy.direction.long\")\nif false\n    strategy.risk.allow_entry_in(allowed)",
            "not-a-direction",
            "strategy.risk.allow_entry_in value",
        ),
        (
            "unit = input.string(\"strategy.cash\")\nif false\n    strategy.risk.max_drawdown(1, unit)",
            "strategy.shares",
            "strategy.risk.max_drawdown type",
        ),
        (
            "unit = input.string(\"strategy.cash\")\nif false\n    strategy.risk.max_intraday_loss(1, unit)",
            "strategy.shares",
            "strategy.risk.max_intraday_loss type",
        ),
    ];
    let bars = [(10.0, 10.0, 10.0, 10.0); 2];

    for (rule, override_value, expected_message) in cases {
        let source =
            format!("//@version=6\nstrategy(\"Invalid dynamic token\")\n{rule}\nplot(close)\n");
        let options = RunOptions {
            inputs: HashMap::from([(0, InputValue::String(override_value.to_string()))]),
            ..RunOptions::default()
        };
        let result = compile_and_run(&source, &data(&bars), CompileOptions::default(), &options);
        assert!(
            result.diagnostics.is_empty(),
            "the simple-string expression is statically valid: {:?}",
            result.diagnostics
        );
        assert_eq!(result.runtime_errors.len(), 1, "{rule}");
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::TypeMismatch
        );
        assert!(
            result.runtime_errors[0].message.contains(expected_message),
            "unexpected runtime error: {:?}",
            result.runtime_errors[0]
        );
        assert!(
            result.runtime_errors[0].span.is_some(),
            "the hoisted directive must retain its source map"
        );
    }
}

#[test]
fn invalid_dynamic_risk_thresholds_are_source_mapped_runtime_errors() {
    let cases = [
        (
            "max_drawdown",
            "strategy.percent_of_equity",
            50.0,
            101.0,
            "between 0 and 100 inclusive",
        ),
        (
            "max_intraday_loss",
            "strategy.percent_of_equity",
            50.0,
            101.0,
            "between 0 and 100 inclusive",
        ),
        (
            "max_drawdown",
            "strategy.cash",
            1.0,
            -1.0,
            "finite non-negative number",
        ),
        (
            "max_intraday_loss",
            "strategy.cash",
            1.0,
            -1.0,
            "finite non-negative number",
        ),
    ];
    let bars = [(10.0, 10.0, 10.0, 10.0); 2];

    for (member, unit, default, override_value, expected_message) in cases {
        let source = format!(
            "//@version=6\nstrategy(\"Invalid dynamic threshold\")\nthreshold = input.float({default})\nif false\n    strategy.risk.{member}(threshold, {unit})\nplot(close)\n"
        );
        let options = RunOptions {
            inputs: HashMap::from([(0, InputValue::Float(override_value))]),
            ..RunOptions::default()
        };
        let result = compile_and_run(&source, &data(&bars), CompileOptions::default(), &options);
        assert!(
            result.diagnostics.is_empty(),
            "the input-derived threshold is statically valid: {:?}",
            result.diagnostics
        );
        assert_eq!(result.runtime_errors.len(), 1, "{member}({override_value})");
        let error = &result.runtime_errors[0];
        assert_eq!(error.kind, RuntimeErrorKind::TypeMismatch);
        assert!(
            error.message.contains(expected_message),
            "unexpected runtime error: {error:?}"
        );
        assert!(
            error.span.is_some(),
            "the hoisted risk call must retain its source map: {error:?}"
        );
    }
}

#[test]
fn later_same_type_risk_calls_cannot_weaken_earlier_rules() {
    struct Case {
        name: &'static str,
        rules: &'static str,
        body: &'static str,
        bars: &'static [(f64, f64, f64, f64)],
    }

    const FLAT: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
    ];
    const LOSS: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 10.0, 10.0),
        (8.0, 8.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    const INTRADAY_LOSS: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    const LOSS_DAYS: &[(f64, f64, f64, f64)] = &[
        (10.0, 10.0, 10.0, 10.0),
        (10.0, 10.0, 9.0, 9.0),
        (9.0, 9.0, 8.0, 8.0),
        (8.0, 8.0, 8.0, 8.0),
        (20.0, 20.0, 20.0, 20.0),
    ];

    let cases = [
        Case {
            name: "max_position_size",
            rules: "strategy.risk.max_position_size(3)\nif false\n    strategy.risk.max_position_size(8)",
            body: "if bar_index == 0\n    strategy.entry(\"L\", strategy.long, qty=10)",
            bars: FLAT,
        },
        Case {
            name: "allow_entry_in",
            rules: "strategy.risk.allow_entry_in(strategy.direction.long)\nif false\n    strategy.risk.allow_entry_in(strategy.direction.all)",
            body: "if bar_index == 0\n    strategy.entry(\"S\", strategy.short)",
            bars: FLAT,
        },
        Case {
            name: "max_drawdown",
            rules: "strategy.risk.max_drawdown(1, strategy.cash, \"strict\")\nif false\n    strategy.risk.max_drawdown(50, strategy.percent_of_equity, \"weak\")",
            body: "if bar_index == 0\n    strategy.entry(\"L\", strategy.long)\nif bar_index >= 2\n    strategy.entry(\"Again\", strategy.long)",
            bars: LOSS,
        },
        Case {
            name: "max_intraday_loss",
            rules: "strategy.risk.max_intraday_loss(1, strategy.cash, \"strict\")\nif false\n    strategy.risk.max_intraday_loss(50, strategy.percent_of_equity, \"weak\")",
            body: "strategy.entry(\"L\", strategy.long)",
            bars: INTRADAY_LOSS,
        },
        Case {
            name: "max_intraday_filled_orders",
            rules: "strategy.risk.max_intraday_filled_orders(1, \"strict\")\nif false\n    strategy.risk.max_intraday_filled_orders(5, \"weak\")",
            body: "strategy.entry(\"L\", strategy.long)",
            bars: FLAT,
        },
        Case {
            name: "max_cons_loss_days",
            rules: "strategy.risk.max_cons_loss_days(1, \"strict\")\nif false\n    strategy.risk.max_cons_loss_days(5, \"weak\")",
            body: "if bar_index == 0 or bar_index >= 3\n    strategy.entry(\"L\", strategy.long)",
            bars: LOSS_DAYS,
        },
    ];

    for case in cases {
        let outcome = run_strategy(&source(case.rules, case.body), case.bars);
        assert_expected_effect(case.name, &outcome);
    }

    let alert_source = source(
        "strategy.risk.max_drawdown(1, strategy.cash, \"strict\")\n\
         strategy.risk.max_drawdown(50, strategy.percent_of_equity, \"weak\")",
        "if bar_index == 0\n    strategy.entry(\"L\", strategy.long)",
    );
    let result = compile_and_run(
        &alert_source,
        &data(LOSS),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert!(
        result.runtime_errors.is_empty(),
        "{:?}",
        result.runtime_errors
    );
    assert_eq!(
        result
            .alerts
            .iter()
            .map(|event| event.message.as_str())
            .collect::<Vec<_>>(),
        vec!["strict"],
        "the normalized strictest mixed-unit rule supplies the halt alert"
    );
}

fn capped_position(definitions: &str, bars: &[(f64, f64, f64, f64)]) -> f64 {
    let source = format!(
        "//@version=6\n\
         strategy(\"Hoisted cap\", initial_capital=100000)\n\
         seed = input.float(1.0)\n\
         flag = input.bool(true)\n\
         {definitions}\n\
         strategy.risk.max_position_size(cap)\n\
         if bar_index == 0\n    strategy.entry(\"L\", strategy.long, qty=1000)\n\
         plot(strategy.position_size)\n"
    );
    run_strategy(&source, bars).position.size
}

#[test]
fn a_hoisted_risk_argument_has_the_value_its_variable_was_assigned() {
    let bars = [(10.0, 10.0, 10.0, 10.0); 3];
    // `cap` was assigned while `base` was 3: reassigning `base` afterwards
    // does not change it.
    assert_eq!(
        capped_position("base = seed * 3.0\ncap = base\nbase := 5.0", &bars),
        3.0
    );
    // Each reassignment reads the value before it.
    assert_eq!(
        capped_position(
            "cap = seed\nif flag\n    cap := cap + seed\nif flag\n    cap := cap + cap",
            &bars
        ),
        4.0
    );
    // Reassignments that read each other: a = 2, b = 3, a = 5.
    assert_eq!(
        capped_position(
            "a = seed\nb = seed\na := a + b\nb := a + b\na := a + b\ncap = a",
            &bars
        ),
        5.0
    );
}

#[test]
fn a_risk_argument_too_large_to_hoist_is_a_compile_error() {
    let mut source = String::from(
        "//@version=6\nstrategy(\"Large cap\")\nseed = input.float(1.0)\ncap = seed\n",
    );
    source.push_str(&"cap := cap + cap\n".repeat(64));
    source.push_str("strategy.risk.max_position_size(cap)\n");
    let result = compile_and_run(
        &source,
        &data(&[(10.0, 10.0, 10.0, 10.0); 3]),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(result.strategy.is_none());
    let codes = result
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(codes, [Some("E0251")]);
}
