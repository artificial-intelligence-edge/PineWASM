//! Tests for host-supplied strategy properties overrides.

use pinewasm_compiler::{
    compile_and_run, CompileOptions, MarketData, RunOptions, StrategyRunOverrides,
};
use pinewasm_mir::Value;
use serde_json::json;

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

fn run(
    source: &str,
    bars: &[(f64, f64, f64, f64)],
    options: &RunOptions,
) -> pinewasm_compiler::StrategyOutcome {
    let result = compile_and_run(source, &data(bars), CompileOptions::default(), options);
    assert!(
        result.diagnostics.is_empty(),
        "diagnostics: {:?}",
        result.diagnostics
    );
    assert!(
        result.runtime_errors.is_empty(),
        "runtime errors: {:?}",
        result.runtime_errors
    );
    result.strategy.expect("strategy outcome")
}

#[test]
fn supported_execution_overrides_change_fills_and_report_consistently() {
    let source = "//@version=6\n\
        strategy(\"S\", initial_capital=10000, default_qty_value=1)\n\
        if bar_index == 0\n    strategy.entry(\"L\", strategy.long)\n\
        if bar_index == 1\n    strategy.close_all()\n\
        plot(close)\n";
    let bars = [(9.0, 10.0, 8.0, 10.0), (20.0, 21.0, 19.0, 20.0)];
    let options = RunOptions {
        strategy: StrategyRunOverrides {
            initial_capital: Some(20_000.0),
            default_qty_value: Some(2.0),
            commission_type: Some("cash_per_contract".to_string()),
            commission_value: Some(1.0),
            slippage: Some(1),
            process_orders_on_close: Some(true),
            ..StrategyRunOverrides::default()
        },
        ..RunOptions::default()
    };

    let outcome = run(source, &bars, &options);
    assert_eq!(outcome.initial_capital, 20_000.0);
    assert_eq!(outcome.trades.len(), 1);
    let trade = &outcome.trades[0];
    assert_eq!(trade.entry_bar, 0);
    assert_eq!(trade.exit_bar, 1);
    assert_eq!(trade.entry_price, 10.01);
    assert!((trade.exit_price - 19.99).abs() < 1e-12);
    assert_eq!(trade.qty, 2.0);
    assert_eq!(trade.commission, 4.0);
    let expected_pnl = (19.99 - 10.01) * 2.0 - 4.0;
    assert!((trade.pnl - expected_pnl).abs() < 1e-9);
    assert!((outcome.net_profit - expected_pnl).abs() < 1e-9);
    assert!((outcome.equity.last().unwrap() - (20_000.0 + expected_pnl)).abs() < 1e-9);
}

#[test]
fn quantity_mode_and_pyramiding_overrides_change_open_exposure() {
    let source = "//@version=6\n\
        strategy(\"S\", initial_capital=10000, default_qty_value=1, pyramiding=0)\n\
        if bar_index == 0\n    strategy.entry(\"A\", strategy.long)\n\
        if bar_index == 1\n    strategy.entry(\"B\", strategy.long)\n\
        plot(close)\n";
    let bars = [
        (9.0, 9.0, 9.0, 9.0),
        (10.0, 10.0, 10.0, 10.0),
        (20.0, 20.0, 20.0, 20.0),
    ];
    let declared = run(source, &bars, &RunOptions::default());
    assert_eq!(declared.position.size, 1.0);

    let options = RunOptions {
        strategy: StrategyRunOverrides {
            default_qty_type: Some("cash".to_string()),
            default_qty_value: Some(100.0),
            // Pyramiding values 0 and 1 both permit a single same-direction
            // entry. A value of 2 permits the two entries in this fixture.
            pyramiding: Some(2),
            ..StrategyRunOverrides::default()
        },
        ..RunOptions::default()
    };
    let overridden = run(source, &bars, &options);
    // 100 cash / 10 on the first fill + 100 cash / 20 on the second fill.
    assert_eq!(overridden.position.size, 15.0);
}

#[test]
fn calculated_bars_override_limits_strategy_history() {
    let source = "//@version=6\n\
        strategy(\"S\", calc_bars_count=0)\n\
        if bar_index == 0\n    strategy.entry(\"L\", strategy.long)\n\
        plot(close)\n";
    let bars = [
        (10.0, 10.0, 10.0, 10.0),
        (11.0, 11.0, 11.0, 11.0),
        (12.0, 12.0, 12.0, 12.0),
        (13.0, 13.0, 13.0, 13.0),
    ];
    let declared = run(source, &bars, &RunOptions::default());
    assert_eq!(declared.position.size, 1.0);

    let overridden = run(
        source,
        &bars,
        &RunOptions {
            strategy: StrategyRunOverrides {
                calc_bars_count: Some(2),
                ..StrategyRunOverrides::default()
            },
            ..RunOptions::default()
        },
    );
    assert_eq!(overridden.position.size, 0.0);
    assert!(overridden.trades.is_empty());
}

#[test]
fn invalid_wire_scalars_and_enum_strings_fall_back_to_declaration() {
    let source = "//@version=6\n\
        strategy(\"S\", initial_capital=10000, default_qty_type=\"cash\", \
          default_qty_value=100, commission_type=\"cash_per_order\", \
          commission_value=2, process_orders_on_close=true)\n\
        if bar_index == 0\n    strategy.entry(\"L\", strategy.long)\n\
        if bar_index == 1\n    strategy.close_all()\n\
        plot(close)\n";
    let bars = [(10.0, 10.0, 10.0, 10.0), (20.0, 20.0, 20.0, 20.0)];
    let declared = run(source, &bars, &RunOptions::default());

    // This is the same camelCase object an HTTP/wasm host supplies. The
    // request itself remains well-formed; each invalid property is ignored.
    let invalid: RunOptions = serde_json::from_value(json!({
        "strategy": {
            "initialCapital": -1,
            "defaultQtyType": "lots",
            "defaultQtyValue": 0,
            "commissionType": "tiered",
            "commissionValue": -1,
            "marginLong": -5,
            "marginShort": -10
        }
    }))
    .expect("strategy override wire object deserializes");
    let fallback = run(source, &bars, &invalid);

    assert_eq!(fallback, declared);
}

#[test]
fn host_override_enables_history_tick_execution() {
    let source = "//@version=6\n\
        strategy(\"S\")\n\
        varip int executions = 0\n\
        executions += 1\n\
        plot(executions)\n";
    let bars = [(10.0, 12.0, 8.0, 11.0)];

    let declared = compile_and_run(
        source,
        &data(&bars),
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        declared.diagnostics.is_empty(),
        "{:?}",
        declared.diagnostics
    );
    assert!(
        declared.runtime_errors.is_empty(),
        "{:?}",
        declared.runtime_errors
    );
    assert_eq!(declared.plots[0].values, vec![Value::Float(1.0)]);

    let options = RunOptions {
        strategy: StrategyRunOverrides {
            calc_on_every_history_tick: Some(true),
            ..StrategyRunOverrides::default()
        },
        ..RunOptions::default()
    };
    let overridden = compile_and_run(source, &data(&bars), CompileOptions::default(), &options);
    assert!(
        overridden.diagnostics.is_empty(),
        "{:?}",
        overridden.diagnostics
    );
    assert!(
        overridden.runtime_errors.is_empty(),
        "{:?}",
        overridden.runtime_errors
    );
    assert_eq!(overridden.plots[0].values, vec![Value::Float(4.0)]);
}
