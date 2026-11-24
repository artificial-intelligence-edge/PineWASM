//! Authored, bounded regressions through the public Rust VM APIs.
//!
//! Expectations follow the small scripts and explicit host inputs below.
//! They make no claim about native execution or final rendering behaviour.
//! The input-style check reads the portable binding with a reader restricted
//! to this fixture; consumers and rendering have separate tests.

use pinewasm_compiler::{
    compile, run_program, CompileOptions, IncrementalSession, InputActiveBinaryOp, InputActiveExpr,
    InputKind, InputValue, MarketData, PlotSeries, PreparedProgram, Program, RequestTimeframeBars,
    RunOptions, RunResult, RuntimeErrorKind, Tick, Value, VmProfile,
};
use std::collections::HashMap;

const START: i64 = 1_704_067_200_000;
const QUARTER_HOUR: i64 = 900_000;
const HOUR: i64 = 3_600_000;
const RUN_BUDGET: u64 = 2_000_000;

const NAMED_SOURCE: &str = r#"//@version=6
indicator("Named optional argument")
var events = array.new_int()
f() =>
    array.push(events, 1)
    "abcd"
s = str.substring(source=f(), begin_pos=1)
plot(str.length(s))
plot(array.size(events))
"#;

const STYLE_SOURCE: &str = r#"//@version=6
indicator("Input style alias")
choice = input.string("Line", "Style", options=["Line", "Circles"])
selected = choice
style = switch selected
    "Circles" => plot.style_circles
    => plot.style_line
plot(close, style=style, offset=1)
plot(selected == "Circles" ? close * 2 : close)
"#;

const TABLE_SOURCE: &str = r#"//@version=6
indicator("Persistent table transaction")
var table t = table.new(position.top_right, 1, 1)
var table alias = t
if not barstate.isrealtime
    table.cell(t, 0, 0, "committed")
if barstate.isrealtime and (close == 40 or close == 50)
    n = table.new(position.top_right, 1, 1)
    table.cell(n, 0, 0, "replacement")
plot(na(alias) ? 1 : 0)
plot(array.size(table.all))
"#;

const HOURLY_SOURCE: &str = r#"//@version=6
indicator("Hourly close via request.security")
plot(request.security(syminfo.tickerid, "60", close, gaps=barmerge.gaps_off, lookahead=barmerge.lookahead_off))
"#;

fn options() -> RunOptions {
    RunOptions {
        max_instructions_per_bar: 200_000,
        max_instructions_per_run: RUN_BUDGET,
        max_collection_elements: 1_000,
        max_string_bytes_per_run: 65_536,
        profile: true,
        ..RunOptions::default()
    }
}

fn program(source: &str) -> Program {
    let compiled = compile(source, CompileOptions::default());
    assert!(compiled.success, "{:?}", compiled.diagnostics);
    assert!(compiled.diagnostics.iter().all(|item| !item.is_error()));
    compiled
        .program
        .expect("successful compile carries a program")
}

fn market(close: &[f64], offset: i64) -> MarketData {
    MarketData {
        open: close.to_vec(),
        high: close.iter().map(|value| value + 1.0).collect(),
        low: close.iter().map(|value| value - 1.0).collect(),
        close: close.to_vec(),
        volume: vec![1.0; close.len()],
        time: (0..close.len())
            .map(|index| START + offset + i64::try_from(index).unwrap() * QUARTER_HOUR)
            .collect(),
        timeframe: "15".into(),
        ..MarketData::default()
    }
}

fn values(plot: &PlotSeries) -> Vec<Option<f64>> {
    plot.values
        .iter()
        .map(|value| match value {
            Value::Na => None,
            Value::Int(value) => Some(*value as f64),
            Value::Float(value) if value.is_finite() => Some(*value),
            other => panic!("unexpected numeric plot value: {other:?}"),
        })
        .collect()
}

fn profile_has_charged_work(profile: &VmProfile) -> bool {
    let count = profile.count.iter().sum::<f64>();
    let units = profile.units.iter().sum::<f64>();
    !profile.count.is_empty()
        && profile.count.len() == profile.units.len()
        && profile
            .count
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        && profile
            .units
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        && count.is_finite()
        && count > 0.0
        && units.is_finite()
        && units > 0.0
        && units <= RUN_BUDGET as f64
}

fn assert_vm_run(run: &RunResult, committed: u32, visible: usize) {
    assert!(run.diagnostics.iter().all(|item| !item.is_error()));
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(run.bars_executed, committed);
    assert!(!run.plots.is_empty());
    for plot in &run.plots {
        assert_eq!(plot.values.len(), visible);
    }
    // These public entry points execute the Rust VM. Profiling also excludes
    // empty samples from receiving execution credit.
    let profile = run.profile.as_ref().expect("requested interpreter profile");
    assert!(profile_has_charged_work(profile));
}

#[test]
fn named_optional_substring_evaluates_its_mutating_source_once_per_bar() {
    let data = market(&[1.0, 2.0, 3.0, 4.0], 0);
    for source in [
        NAMED_SOURCE.to_string(),
        NAMED_SOURCE.replace("begin_pos=1)", "begin_pos=1, end_pos=4)"),
    ] {
        let result = run_program(&program(&source), &data, &options());
        assert_vm_run(&result, 4, 4);
        assert_eq!(values(&result.plots[0]), vec![Some(3.0); 4]);
        assert_eq!(
            values(&result.plots[1]),
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)]
        );
    }
}

#[test]
fn helper_mutation_controls_distinguish_zero_one_and_two_calls() {
    let data = market(&[1.0, 2.0, 3.0, 4.0], 0);
    for (source, counts) in [
        (
            NAMED_SOURCE.replace("    array.push(events, 1)\n", ""),
            vec![Some(0.0); 4],
        ),
        (
            NAMED_SOURCE.replace(
                "    array.push(events, 1)",
                "    array.push(events, 1)\n    array.push(events, 1)",
            ),
            vec![Some(2.0), Some(4.0), Some(6.0), Some(8.0)],
        ),
    ] {
        let result = run_program(&program(&source), &data, &options());
        assert_vm_run(&result, 4, 4);
        assert_eq!(values(&result.plots[0]), vec![Some(3.0); 4]);
        assert_eq!(values(&result.plots[1]), counts);
        assert_ne!(
            values(&result.plots[1]),
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)]
        );
    }
}

// This reader accepts only the four node kinds used by this fixture. Every
// unfamiliar operation, field, input id and unresolved value is an error.
fn admitted_style_expression(expression: &InputActiveExpr) -> bool {
    match expression {
        InputActiveExpr::Value {
            value: InputValue::String(value),
        } => matches!(
            value.as_str(),
            "Line" | "Circles" | "plot.style_line" | "plot.style_circles"
        ),
        InputActiveExpr::Input { input: 0 } => true,
        InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Eq,
            lhs,
            rhs,
        } => admitted_style_expression(lhs) && admitted_style_expression(rhs),
        InputActiveExpr::Ternary {
            condition,
            when_true,
            when_false,
        } => {
            admitted_style_expression(condition)
                && admitted_style_expression(when_true)
                && admitted_style_expression(when_false)
        }
        _ => false,
    }
}

fn style_value(
    expression: &InputActiveExpr,
    selection: &InputValue,
) -> Result<InputValue, &'static str> {
    // Admission covers both branches even though selection remains lazy.
    if !admitted_style_expression(expression) {
        return Err("unfamiliar style expression");
    }
    selected_style_value(expression, selection)
}

fn selected_style_value(
    expression: &InputActiveExpr,
    selection: &InputValue,
) -> Result<InputValue, &'static str> {
    match expression {
        InputActiveExpr::Value {
            value: InputValue::String(value),
        } if matches!(
            value.as_str(),
            "Line" | "Circles" | "plot.style_line" | "plot.style_circles"
        ) =>
        {
            Ok(InputValue::String(value.clone()))
        }
        InputActiveExpr::Input { input: 0 } if matches!(selection, InputValue::String(value) if matches!(value.as_str(), "Line" | "Circles")) => {
            Ok(selection.clone())
        }
        InputActiveExpr::Binary {
            op: InputActiveBinaryOp::Eq,
            lhs,
            rhs,
        } => Ok(InputValue::Bool(
            selected_style_value(lhs, selection)? == selected_style_value(rhs, selection)?,
        )),
        InputActiveExpr::Ternary {
            condition,
            when_true,
            when_false,
        } => match selected_style_value(condition, selection)? {
            InputValue::Bool(condition) => {
                selected_style_value(if condition { when_true } else { when_false }, selection)
            }
            _ => Err("style condition is not boolean"),
        },
        _ => Err("unfamiliar style expression"),
    }
}

fn bound_style(plot: &PlotSeries, selection: &InputValue) -> Result<String, &'static str> {
    let style = &plot.style;
    if !style.unsupported.is_empty() || !style.runtime_scalars.is_empty() {
        return Err("unresolved style metadata");
    }
    if style.bound_scalars.len() != 1 || style.bound_scalars[0].field != "style" {
        return Err("missing, duplicate or unfamiliar style binding");
    }
    match style_value(&style.bound_scalars[0].expression, selection)? {
        InputValue::String(value)
            if matches!(value.as_str(), "plot.style_line" | "plot.style_circles") =>
        {
            Ok(value)
        }
        _ => Err("style binding did not resolve a supported style token"),
    }
}

#[test]
fn input_alias_switch_binds_style_and_numeric_selection_in_fresh_runs() {
    let compiled = program(STYLE_SOURCE);
    assert_eq!(compiled.inputs.len(), 1);
    assert_eq!(compiled.inputs[0].id, 0);
    assert_eq!(compiled.inputs[0].kind, InputKind::String);
    assert_eq!(compiled.inputs[0].title.as_deref(), Some("Style"));
    assert_eq!(compiled.inputs[0].defval, InputValue::String("Line".into()));
    assert_eq!(
        compiled.inputs[0].constraints.options,
        vec![
            InputValue::String("Line".into()),
            InputValue::String("Circles".into())
        ]
    );
    assert_eq!(compiled.plots[0].style.bound_scalars.len(), 1);
    assert_eq!(compiled.plots[0].style.bound_scalars[0].field, "style");
    let resident = PreparedProgram::new(compiled).unwrap();
    let data = market(&[1.0, 2.0, 3.0, 4.0], 0);
    for (override_value, token, multiplier) in [
        (None, "plot.style_line", 1.0),
        (Some("Circles"), "plot.style_circles", 2.0),
        (None, "plot.style_line", 1.0),
    ] {
        let mut settings = options();
        if let Some(value) = override_value {
            settings.inputs.insert(0, InputValue::String(value.into()));
        }
        let selected = settings
            .inputs
            .get(&0)
            .cloned()
            .unwrap_or(InputValue::String("Line".into()));
        let result = resident.run_profiled(&data, &settings, None);
        assert_vm_run(&result, 4, 4);
        assert_eq!(result.plots.len(), 2);
        assert_eq!(
            values(&result.plots[0]),
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)]
        );
        assert_eq!(
            values(&result.plots[1]),
            (1..=4)
                .map(|value| Some(f64::from(value) * multiplier))
                .collect::<Vec<_>>()
        );
        assert_eq!(bound_style(&result.plots[0], &selected).unwrap(), token);
        assert_eq!(result.plots[0].style.offset, 1);
    }
}

#[test]
fn swapped_style_and_missing_binding_controls_cannot_receive_selection_credit() {
    let source = STYLE_SOURCE.replace(
        "\"Circles\" => plot.style_circles\n    => plot.style_line",
        "\"Circles\" => plot.style_line\n    => plot.style_circles",
    );
    let selection = InputValue::String("Circles".into());
    let mut settings = options();
    settings.inputs.insert(0, selection.clone());
    let mut result = run_program(
        &program(&source),
        &market(&[1.0, 2.0, 3.0, 4.0], 0),
        &settings,
    );
    assert_vm_run(&result, 4, 4);
    assert_eq!(
        values(&result.plots[1]),
        vec![Some(2.0), Some(4.0), Some(6.0), Some(8.0)]
    );
    assert_eq!(
        bound_style(&result.plots[0], &selection).unwrap(),
        "plot.style_line"
    );
    result.plots[0].style.bound_scalars[0].field = "unfamiliar".into();
    assert!(bound_style(&result.plots[0], &selection).is_err());
    result.plots[0].style.bound_scalars.clear();
    assert!(bound_style(&result.plots[0], &selection).is_err());
    assert!(style_value(&InputActiveExpr::Input { input: 1 }, &selection).is_err());
    assert!(style_value(&InputActiveExpr::Na, &selection).is_err());
    assert!(style_value(&InputActiveExpr::Input { input: 0 }, &InputValue::Int(1)).is_err());
}

#[test]
fn style_reader_rejects_unsupported_nodes_in_an_unselected_branch() {
    let literal = |value: &str| InputActiveExpr::Value {
        value: InputValue::String(value.into()),
    };
    for unreachable in [
        InputActiveExpr::Input { input: 1 },
        InputActiveExpr::Na,
        InputActiveExpr::Call {
            function: "unsupported".into(),
            args: vec![],
        },
    ] {
        let expression = InputActiveExpr::Ternary {
            condition: Box::new(InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Eq,
                lhs: Box::new(literal("Line")),
                rhs: Box::new(literal("Line")),
            }),
            when_true: Box::new(literal("plot.style_line")),
            when_false: Box::new(unreachable),
        };
        assert!(style_value(&expression, &InputValue::String("Line".into())).is_err());
    }
}

fn ticks() -> Vec<Tick> {
    [
        (3, 40.0, 41.0, 1.0, false),
        (3, 45.0, 46.0, 2.0, false),
        (3, 50.0, 51.0, 3.0, true),
        (4, 60.0, 61.0, 1.0, true),
    ]
    .into_iter()
    .map(|(bar_index, close, high, volume, is_bar_close)| Tick {
        bar_index,
        open: if bar_index == 3 { 40.0 } else { 60.0 },
        high,
        low: if bar_index == 3 { 39.0 } else { 59.0 },
        close,
        volume,
        time: START + i64::try_from(bar_index).unwrap() * QUARTER_HOUR,
        trading_day: None,
        is_bar_close,
    })
    .collect()
}

fn assert_table(result: &RunResult, alias_na: f64, text: &str) {
    assert_eq!(values(&result.plots[0]).last(), Some(&Some(alias_na)));
    assert_eq!(values(&result.plots[1]).last(), Some(&Some(1.0)));
    assert_eq!(result.drawings.tables.len(), 1);
    let table = &result.drawings.tables[0];
    assert_eq!(
        table.fields.get("position"),
        Some(&Value::String("position.top_right".into()))
    );
    assert_eq!(table.cells.len(), 1);
    assert_eq!((table.cells[0].column, table.cells[0].row), (0, 0));
    assert_eq!(
        table.cells[0].attrs.get("text"),
        Some(&Value::String(text.into()))
    );
}

#[test]
fn persistent_table_alias_restores_on_open_tick_and_stays_retired_after_commit() {
    let compiled = program(TABLE_SOURCE);
    let history = market(&[10.0, 20.0, 30.0], 0);
    let mut session = IncrementalSession::new(&compiled, &history, &options()).unwrap();
    let initial = session.result();
    assert_vm_run(&initial, 3, 3);
    assert_table(&initial, 0.0, "committed");
    for (tick, committed, alias_na, text) in ticks()
        .iter()
        .zip([
            (3, 1.0, "replacement"),
            (3, 0.0, "committed"),
            (4, 1.0, "replacement"),
            (5, 1.0, "replacement"),
        ])
        .map(|(tick, (committed, alias_na, text))| (tick, committed, alias_na, text))
    {
        let result = session.advance_profiled(std::slice::from_ref(tick), None);
        assert_vm_run(&result, committed, tick.bar_index + 1);
        assert_eq!(session.committed_bars(), committed);
        assert_eq!(session.resident_bars(), tick.bar_index + 1);
        assert!(!session.is_failed());
        assert_table(&result, alias_na, text);
    }
    assert_eq!(
        values(&session.result().plots[0]),
        vec![Some(0.0), Some(0.0), Some(0.0), Some(1.0), Some(1.0)]
    );
}

#[test]
fn removing_the_closing_replacement_changes_committed_alias_and_grid_state() {
    let source = TABLE_SOURCE.replace("(close == 40 or close == 50)", "close == 40");
    let mut session = IncrementalSession::new(
        &program(&source),
        &market(&[10.0, 20.0, 30.0], 0),
        &options(),
    )
    .unwrap();
    for update in ticks() {
        let result = session.advance_profiled(&[update], None);
        assert!(
            result.runtime_errors.is_empty(),
            "{:?}",
            result.runtime_errors
        );
    }
    let result = session.result();
    assert_vm_run(&result, 5, 5);
    assert_table(&result, 0.0, "committed");
}

fn hourly_market() -> MarketData {
    let mut data = market(&[2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], QUARTER_HOUR);
    data.time_close = data.time.iter().map(|time| time + QUARTER_HOUR).collect();
    data.request_time_close = HashMap::from([(
        "60".into(),
        vec![
            START + HOUR,
            START + HOUR,
            START + HOUR,
            START + 2 * HOUR,
            START + 2 * HOUR,
            START + 2 * HOUR,
            START + 2 * HOUR,
        ],
    )]);
    data.request_bars = HashMap::from([(
        "60".into(),
        RequestTimeframeBars {
            time: vec![START, START + HOUR],
            time_close: vec![START + HOUR, START + 2 * HOUR],
            open: vec![1.0, 5.0],
            high: vec![5.0, 9.0],
            low: vec![0.0, 4.0],
            close: vec![4.0, 8.0],
            volume: vec![4.0, 4.0],
        },
    )]);
    data
}

#[test]
fn hourly_request_closes_publish_on_clock_boundaries() {
    let compiled = program(HOURLY_SOURCE);
    let data = hourly_market();
    assert_eq!(data.time[0], START + QUARTER_HOUR);
    let expected = vec![
        None,
        None,
        Some(4.0),
        Some(4.0),
        Some(4.0),
        Some(4.0),
        Some(8.0),
    ];
    let original = run_program(&compiled, &data, &options());
    assert_vm_run(&original, 7, 7);
    assert_eq!(values(&original.plots[0]), expected);

    // Bars and clocks stay unchanged. Different requested closes
    // must be consumed; a bar-only aggregate must not satisfy the check.
    let mut changed = data.clone();
    let requested = changed.request_bars.get_mut("60").unwrap();
    requested.close = vec![40.0, 80.0];
    requested.high = vec![41.0, 81.0];
    let result = run_program(&compiled, &changed, &options());
    assert_vm_run(&result, 7, 7);
    assert_eq!(
        values(&result.plots[0]),
        vec![
            None,
            None,
            Some(40.0),
            Some(40.0),
            Some(40.0),
            Some(40.0),
            Some(80.0)
        ]
    );
    assert_ne!(values(&result.plots[0]), expected);

    let mut malformed = data.clone();
    malformed.request_time_close.get_mut("60").unwrap().pop();
    let failure = run_program(&compiled, &malformed, &options());
    assert_eq!(failure.bars_executed, 0);
    assert_eq!(failure.runtime_errors.len(), 1);
    assert_eq!(
        failure.runtime_errors[0].kind,
        RuntimeErrorKind::MalformedMarketData
    );
    let recovered = run_program(&compiled, &data, &options());
    assert_vm_run(&recovered, 7, 7);
    assert_eq!(values(&recovered.plots[0]), expected);
}

#[test]
fn every_scenario_honors_zero_instruction_budget_before_a_fresh_successful_run() {
    for (source, data, expected) in [
        (
            NAMED_SOURCE,
            market(&[1.0, 2.0, 3.0, 4.0], 0),
            vec![Some(3.0); 4],
        ),
        (
            STYLE_SOURCE,
            market(&[1.0, 2.0, 3.0, 4.0], 0),
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
        ),
        (
            TABLE_SOURCE,
            market(&[10.0, 20.0, 30.0], 0),
            vec![Some(0.0); 3],
        ),
        (
            HOURLY_SOURCE,
            hourly_market(),
            vec![
                None,
                None,
                Some(4.0),
                Some(4.0),
                Some(4.0),
                Some(4.0),
                Some(8.0),
            ],
        ),
    ] {
        let compiled = program(source);
        let failure = run_program(
            &compiled,
            &data,
            &RunOptions {
                max_instructions_per_bar: 0,
                ..options()
            },
        );
        assert_eq!(failure.bars_executed, 0);
        assert_eq!(failure.runtime_errors.len(), 1);
        assert_eq!(
            failure.runtime_errors[0].kind,
            RuntimeErrorKind::InstructionLimitExceeded
        );
        let recovered = run_program(&compiled, &data, &options());
        assert_vm_run(&recovered, u32::try_from(data.bars()).unwrap(), data.bars());
        assert_eq!(values(&recovered.plots[0]), expected);
    }
}

#[test]
fn positive_execution_counts_do_not_credit_empty_or_zero_charged_units() {
    let result = run_program(
        &program(NAMED_SOURCE),
        &market(&[1.0, 2.0, 3.0, 4.0], 0),
        &options(),
    );
    assert_vm_run(&result, 4, 4);
    let profile = result.profile.unwrap();
    let mut empty = profile.clone();
    empty.units.clear();
    assert!(empty.count.iter().sum::<f64>() > 0.0);
    assert!(!profile_has_charged_work(&empty));
    let mut zero = profile;
    zero.units.fill(0.0);
    assert!(zero.count.iter().sum::<f64>() > 0.0);
    assert!(!profile_has_charged_work(&zero));
}
