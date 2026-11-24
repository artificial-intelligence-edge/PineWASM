//! Compiler acceptance tests for the `strategy()` declaration and strategy.* names.
//!
//! This test covers parser -> semantics -> HIR -> MIR -> validated CSIL
//! acceptance. It does not check broker results; those are asserted by the
//! VM tests in `crates/pinewasm_vm/src/tests.rs`.

use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_hir::STRATEGY_DECL_PARAMS;
use pinewasm_semantics::BuiltinRegistry;
use std::collections::BTreeSet;

const DECLARATION_PARAMS: [&str; 34] = [
    "title",
    "shorttitle",
    "overlay",
    "format",
    "precision",
    "scale",
    "pyramiding",
    "calc_on_order_fills",
    "calc_on_every_tick",
    "max_bars_back",
    "backtest_fill_limits_assumption",
    "default_qty_type",
    "default_qty_value",
    "initial_capital",
    "currency",
    "slippage",
    "commission_type",
    "commission_value",
    "process_orders_on_close",
    "close_entries_rule",
    "margin_long",
    "margin_short",
    "explicit_plot_zorder",
    "max_lines_count",
    "max_labels_count",
    "max_boxes_count",
    "calc_bars_count",
    "risk_free_rate",
    "use_bar_magnifier",
    "fill_orders_on_standard_ohlc",
    "max_polylines_count",
    "dynamic_requests",
    "behind_chart",
    "calc_on_every_history_tick",
];

const CALLABLES: [&str; 48] = [
    "strategy",
    "strategy.cancel",
    "strategy.cancel_all",
    "strategy.close",
    "strategy.close_all",
    "strategy.closedtrades.commission",
    "strategy.closedtrades.entry_bar_index",
    "strategy.closedtrades.entry_comment",
    "strategy.closedtrades.entry_id",
    "strategy.closedtrades.entry_price",
    "strategy.closedtrades.entry_time",
    "strategy.closedtrades.exit_bar_index",
    "strategy.closedtrades.exit_comment",
    "strategy.closedtrades.exit_id",
    "strategy.closedtrades.exit_price",
    "strategy.closedtrades.exit_time",
    "strategy.closedtrades.max_drawdown",
    "strategy.closedtrades.max_drawdown_percent",
    "strategy.closedtrades.max_runup",
    "strategy.closedtrades.max_runup_percent",
    "strategy.closedtrades.profit",
    "strategy.closedtrades.profit_percent",
    "strategy.closedtrades.size",
    "strategy.convert_to_account",
    "strategy.convert_to_symbol",
    "strategy.default_entry_qty",
    "strategy.entry",
    "strategy.exit",
    "strategy.opentrades.commission",
    "strategy.opentrades.entry_bar_index",
    "strategy.opentrades.entry_comment",
    "strategy.opentrades.entry_id",
    "strategy.opentrades.entry_price",
    "strategy.opentrades.entry_time",
    "strategy.opentrades.max_drawdown",
    "strategy.opentrades.max_drawdown_percent",
    "strategy.opentrades.max_runup",
    "strategy.opentrades.max_runup_percent",
    "strategy.opentrades.profit",
    "strategy.opentrades.profit_percent",
    "strategy.opentrades.size",
    "strategy.order",
    "strategy.risk.allow_entry_in",
    "strategy.risk.max_cons_loss_days",
    "strategy.risk.max_drawdown",
    "strategy.risk.max_intraday_filled_orders",
    "strategy.risk.max_intraday_loss",
    "strategy.risk.max_position_size",
];

const VARIABLES: [&str; 35] = [
    "strategy.position_size",
    "strategy.position_avg_price",
    "strategy.openprofit",
    "strategy.netprofit",
    "strategy.grossprofit",
    "strategy.grossloss",
    "strategy.equity",
    "strategy.initial_capital",
    "strategy.wintrades",
    "strategy.losstrades",
    "strategy.eventrades",
    "strategy.closedtrades",
    "strategy.opentrades",
    "strategy.max_drawdown",
    "strategy.max_runup",
    "strategy.avg_trade",
    "strategy.avg_winning_trade",
    "strategy.avg_losing_trade",
    "strategy.max_contracts_held_all",
    "strategy.max_contracts_held_long",
    "strategy.max_contracts_held_short",
    "strategy.account_currency",
    "strategy.avg_losing_trade_percent",
    "strategy.avg_trade_percent",
    "strategy.avg_winning_trade_percent",
    "strategy.closedtrades.first_index",
    "strategy.grossloss_percent",
    "strategy.grossprofit_percent",
    "strategy.margin_liquidation_price",
    "strategy.max_drawdown_percent",
    "strategy.max_runup_percent",
    "strategy.netprofit_percent",
    "strategy.openprofit_percent",
    "strategy.opentrades.capital_held",
    "strategy.position_entry_name",
];

const CONSTANTS: [&str; 14] = [
    "strategy.cash",
    "strategy.commission.cash_per_contract",
    "strategy.commission.cash_per_order",
    "strategy.commission.percent",
    "strategy.direction.all",
    "strategy.direction.long",
    "strategy.direction.short",
    "strategy.fixed",
    "strategy.long",
    "strategy.oca.cancel",
    "strategy.oca.none",
    "strategy.oca.reduce",
    "strategy.percent_of_equity",
    "strategy.short",
];

fn complete_surface_source() -> String {
    let mut source = String::from(
        "//@version=6\n\
         strategy(\"Catalog\", shorttitle=\"Cat\", overlay=true, format=format.price, precision=4, scale=scale.right, pyramiding=2, calc_on_order_fills=true, calc_on_every_tick=true, max_bars_back=500, backtest_fill_limits_assumption=1, default_qty_type=strategy.fixed, default_qty_value=1, initial_capital=10000, currency=currency.USD, slippage=1, commission_type=strategy.commission.percent, commission_value=0.1, process_orders_on_close=true, close_entries_rule=\"ANY\", margin_long=100, margin_short=100, explicit_plot_zorder=true, max_lines_count=100, max_labels_count=100, max_boxes_count=100, calc_bars_count=500, risk_free_rate=2.0, use_bar_magnifier=true, fill_orders_on_standard_ohlc=true, max_polylines_count=50, dynamic_requests=true, behind_chart=false, calc_on_every_history_tick=true)\n",
    );

    for (index, name) in VARIABLES.iter().enumerate() {
        source.push_str(&format!("v_{index} = {name}\n"));
    }
    for (index, name) in CONSTANTS.iter().enumerate() {
        source.push_str(&format!("k_{index} = {name}\n"));
    }
    for (index, name) in CALLABLES
        .iter()
        .filter(|name| name.contains("trades."))
        .enumerate()
    {
        source.push_str(&format!("trade_{index} = {name}(0)\n"));
    }

    source.push_str(
        "converted_account = strategy.convert_to_account(close)\n\
         converted_symbol = strategy.convert_to_symbol(close)\n\
         default_qty = strategy.default_entry_qty(close)\n\
         strategy.entry(\"Entry\", strategy.long, qty=1, limit=90, stop=110, oca_name=\"entries\", oca_type=strategy.oca.cancel, comment=\"entry\", alert_message=\"entry alert\", disable_alert=false)\n\
         strategy.order(\"Order\", strategy.short, qty=1, limit=110, stop=90, oca_name=\"orders\", oca_type=strategy.oca.reduce, comment=\"order\", alert_message=\"order alert\", disable_alert=false)\n\
         strategy.exit(\"Exit\", from_entry=\"Entry\", qty=1, qty_percent=100, profit=10, limit=110, loss=10, stop=90, trail_price=105, trail_points=5, trail_offset=1, oca_name=\"exits\", comment=\"exit\", comment_profit=\"profit\", comment_loss=\"loss\", comment_trailing=\"trailing\", alert_message=\"exit alert\", alert_profit=\"profit alert\", alert_loss=\"loss alert\", alert_trailing=\"trailing alert\", disable_alert=false)\n\
         strategy.close(\"Entry\", comment=\"close\", qty=1, qty_percent=100, alert_message=\"close alert\", immediately=true, disable_alert=false)\n\
         strategy.close_all(comment=\"close all\", alert_message=\"close all alert\", immediately=true, disable_alert=false)\n\
         strategy.cancel(\"Entry\")\n\
         strategy.cancel_all()\n\
         strategy.risk.allow_entry_in(strategy.direction.all)\n\
         strategy.risk.max_cons_loss_days(3, \"loss days\")\n\
         strategy.risk.max_drawdown(25, strategy.percent_of_equity, \"drawdown\")\n\
         strategy.risk.max_intraday_filled_orders(10, \"filled orders\")\n\
         strategy.risk.max_intraday_loss(100, strategy.cash, \"intraday loss\")\n\
         strategy.risk.max_position_size(100)\n\
         plot(strategy.position_size)\n",
    );
    source
}

#[test]
fn complete_strategy_surface_compiles_to_validated_csil() {
    assert_eq!(STRATEGY_DECL_PARAMS, DECLARATION_PARAMS);

    let registry = BuiltinRegistry::standard();
    // `strategy()` is a declaration statement rather than a normal function.
    let expected_callables: BTreeSet<_> = CALLABLES[1..].iter().copied().collect();
    let registered_callables: BTreeSet<_> = registry
        .functions()
        .map(|function| function.name)
        .filter(|name| name.starts_with("strategy."))
        .collect();
    assert_eq!(expected_callables.len(), CALLABLES.len() - 1);
    assert_eq!(registered_callables.len(), CALLABLES.len() - 1);
    assert_eq!(registered_callables, expected_callables);

    let expected_variables: BTreeSet<_> = VARIABLES.iter().copied().collect();
    let registered_variables: BTreeSet<_> = registry
        .series()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("strategy."))
        .collect();
    assert_eq!(expected_variables.len(), VARIABLES.len());
    assert_eq!(registered_variables.len(), VARIABLES.len());
    assert_eq!(registered_variables, expected_variables);

    let expected_constants: BTreeSet<_> = CONSTANTS.iter().copied().collect();
    let registered_constants: BTreeSet<_> = registry
        .constants()
        .map(|constant| constant.name)
        .filter(|name| name.starts_with("strategy."))
        .collect();
    assert_eq!(expected_constants.len(), CONSTANTS.len());
    assert_eq!(registered_constants.len(), CONSTANTS.len());
    assert_eq!(registered_constants, expected_constants);

    let source = complete_surface_source();
    for name in CALLABLES {
        assert!(
            source.contains(&format!("{name}(")),
            "source does not call `{name}`"
        );
    }
    for name in VARIABLES {
        assert!(source.contains(name), "source does not read `{name}`");
    }
    for name in CONSTANTS {
        assert!(source.contains(name), "source does not use `{name}`");
    }
    for name in &DECLARATION_PARAMS[1..] {
        assert!(
            source.contains(&format!("{name}=")),
            "declaration does not pass `{name}`"
        );
    }

    let result = compile(&source, CompileOptions::all());
    assert!(
        result.success,
        "strategy catalog failed to compile: {:?}",
        result.diagnostics
    );
    assert!(result.program.is_some(), "validated CSIL program is absent");
    let program = result.program.as_ref().expect("validated CSIL program");
    assert_eq!(program.metadata.title.as_deref(), Some("Cat"));
    assert_eq!(
        program
            .metadata
            .strategy
            .as_ref()
            .map(|strategy| strategy.title.as_str()),
        Some("Catalog")
    );
    assert!(result.ast_debug.is_some());
    assert!(result.hir_debug.is_some());
    assert!(result.mir_debug.is_some());
    assert!(result.csil_text.is_some());
}
