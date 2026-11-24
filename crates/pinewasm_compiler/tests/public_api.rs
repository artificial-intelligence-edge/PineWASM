//! Contract tests that consume only the supported `pinewasm_compiler` facade.

use pinewasm_compiler::{
    compile, compile_with_libraries_profiled, run_program, run_program_profiled, CompileOptions,
    CompileResult, IncrementalSession, InputValue, MarketData, Program, RunOptions, RunResult,
    RuntimeErrorKind, TaInputBinding, TaInputTarget, Tick, Value, DEFAULT_MAX_COLLECTION_ELEMENTS,
    DEFAULT_MAX_INSTRUCTIONS_PER_BAR, DEFAULT_MAX_INSTRUCTIONS_PER_RUN, DEFAULT_MAX_STACK_DEPTH,
    DEFAULT_MAX_STRING_BYTES_PER_RUN, DEFAULT_MAX_VM_BYTES_PER_RUN,
};
use std::collections::HashMap;
use std::sync::Arc;

fn market_data() -> MarketData {
    MarketData {
        open: vec![1.0, 2.0, 3.0],
        high: vec![1.0, 2.0, 3.0],
        low: vec![1.0, 2.0, 3.0],
        close: vec![1.0, 2.0, 3.0],
        volume: vec![10.0, 20.0, 30.0],
        ..MarketData::default()
    }
}

fn program() -> Program {
    let result = compile_with_libraries_profiled(
        "//@version=6\nindicator(\"API contract\")\nplot(close * 2)",
        &[],
        CompileOptions::default(),
        None,
    );
    assert!(result.success, "{:?}", result.diagnostics);
    result.program.expect("successful compile has a program")
}

#[test]
fn default_resource_limits_match_the_exported_contract() {
    let options = RunOptions::default();
    assert_eq!(
        options.max_instructions_per_bar,
        DEFAULT_MAX_INSTRUCTIONS_PER_BAR
    );
    assert_eq!(
        options.max_instructions_per_run,
        DEFAULT_MAX_INSTRUCTIONS_PER_RUN
    );
    assert_eq!(
        options.max_string_bytes_per_run,
        DEFAULT_MAX_STRING_BYTES_PER_RUN
    );
    assert_eq!(options.max_vm_bytes_per_run, DEFAULT_MAX_VM_BYTES_PER_RUN);
    assert_eq!(options.max_stack_depth, DEFAULT_MAX_STACK_DEPTH);
    assert_eq!(
        options.max_collection_elements,
        DEFAULT_MAX_COLLECTION_ELEMENTS
    );
    assert_eq!(options.max_bars, None);
}

#[test]
fn program_ta_binding_types_are_available_through_the_facade() {
    let binding = TaInputBinding {
        input: 3,
        negated: false,
        target: TaInputTarget::State { state: 4, param: 1 },
    };
    assert_eq!(binding.input, 3);
    assert_eq!(binding.target, TaInputTarget::State { state: 4, param: 1 });
}

/// Every run-constant TA input member of `Program` must stay usable with only
/// facade types: the binding field, the engine bounds, override resolution,
/// history depth under overrides, and clone specialization.
#[test]
fn program_ta_input_members_are_usable_through_the_facade() {
    let program = compile(
        "//@version=6\nindicator(\"TA facade\")\nn = input.int(2)\nplot(ta.sma(close, n))",
        CompileOptions::default(),
    )
    .program
    .expect("dynamic TA length source compiles");

    let bindings: &Vec<TaInputBinding> = &program.ta_input_bindings;
    let binding = bindings
        .first()
        .expect("a dynamic ta.sma length produces one binding");
    assert_eq!(binding.input, 0);
    assert!(!binding.negated);

    let (min, max): (i64, i64) = program
        .ta_input_bounds(binding)
        .expect("window lengths have an engine interval");
    assert!(min >= 1 && min <= max);

    let resolved: Option<i64> = program.resolve_ta_input(binding, Some(&InputValue::Int(3)));
    assert_eq!(resolved, Some(3));
    let default: Option<i64> = program.resolve_ta_input(binding, None);
    assert_eq!(default, Some(2));
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Int(0))),
        None
    );
    assert_eq!(
        program.resolve_ta_input(binding, Some(&InputValue::Float(3.0))),
        Some(2),
        "a wrong-kind override falls back to the declared default"
    );

    let mut overrides: HashMap<TaInputTarget, i64> = HashMap::new();
    overrides.insert(binding.target, 3);
    let depth: u32 = program.history_depth_with_ta_inputs(&overrides);
    assert!(depth >= program.history_depth());

    // `set_ta_inputs` specializes a clone for the engine after validation and
    // range checks; the specialized clone is not itself re-validated. Its
    // baseline depth must equal the override-aware depth of the original.
    let mut specialized: Program = program.clone();
    specialized.set_ta_inputs(&overrides);
    assert_eq!(specialized.history_depth(), depth);

    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Int(3));
    let overridden = run_program(&program, &market_data(), &options);
    assert!(
        overridden.runtime_errors.is_empty(),
        "{:?}",
        overridden.runtime_errors
    );
    assert_eq!(overridden.bars_executed, 3);
}

#[test]
fn timing_free_compile_and_run_results_are_exactly_repeatable() {
    let source = "//@version=6\nindicator(\"Deterministic\")\nplot(math.sin(close))";
    let compiled_a = compile_with_libraries_profiled(source, &[], CompileOptions::all(), None);
    let compiled_b = compile_with_libraries_profiled(source, &[], CompileOptions::all(), None);
    assert_eq!(compiled_a, compiled_b);
    assert!(compiled_a.timings.is_none());

    let program = compiled_a.program.expect("source must compile");
    let run_a = run_program_profiled(&program, &market_data(), &RunOptions::default(), None);
    let run_b = run_program_profiled(&program, &market_data(), &RunOptions::default(), None);
    assert_eq!(run_a, run_b);
    assert!(run_a.run_ms.is_none());
}

#[test]
fn cached_program_and_market_data_failures_stay_structured() {
    let mut invalid_program = program();
    invalid_program.header.magic = "not-csil".to_string();
    let invalid = run_program(&invalid_program, &market_data(), &RunOptions::default());
    assert_eq!(invalid.bars_executed, 0);
    assert_eq!(invalid.runtime_errors.len(), 1);
    assert_eq!(
        invalid.runtime_errors[0].kind,
        RuntimeErrorKind::InvalidProgram
    );

    let mut invalid_constraints = compile(
        "//@version=6\nindicator(\"Constraints\")\nn = input.float(2.0, minval=1.0, maxval=3.0)\nplot(n)",
        CompileOptions::default(),
    )
    .program
    .expect("constrained input source compiles");
    invalid_constraints.inputs[0].constraints.max = Some(InputValue::Float(1.0));
    let invalid = run_program(&invalid_constraints, &market_data(), &RunOptions::default());
    assert_eq!(invalid.bars_executed, 0);
    assert_eq!(invalid.runtime_errors.len(), 1);
    assert_eq!(
        invalid.runtime_errors[0].kind,
        RuntimeErrorKind::InvalidProgram
    );

    let malformed = MarketData {
        close: vec![1.0],
        ..MarketData::default()
    };
    let invalid = run_program(&program(), &malformed, &RunOptions::default());
    assert_eq!(invalid.bars_executed, 0);
    assert_eq!(invalid.runtime_errors.len(), 1);
    assert_eq!(
        invalid.runtime_errors[0].kind,
        RuntimeErrorKind::MalformedMarketData
    );

    let ta_program = compile(
        "//@version=6\nindicator(\"TA input\")\nn = input.int(2)\nplot(ta.sma(close, n))",
        CompileOptions::default(),
    )
    .program
    .expect("dynamic TA length source compiles");
    let mut invalid_options = RunOptions::default();
    invalid_options.inputs.insert(0, InputValue::Int(0));
    let invalid = run_program(&ta_program, &market_data(), &invalid_options);
    assert_eq!(invalid.bars_executed, 0);
    assert_eq!(invalid.runtime_errors.len(), 1);
    assert_eq!(
        invalid.runtime_errors[0].kind,
        RuntimeErrorKind::InvalidTaLength
    );
}

#[test]
fn advance_samples_answers_with_the_forming_bar_rather_than_the_history() {
    // Two sessions over the same script whose histories differ by three orders
    // of magnitude. The point of the bar-scoped entry point is that its answer
    // is the same size in both, while the snapshot-shaped `advance` grows with
    // whatever is behind the bar.
    let program = compile(
        "//@version=6\nindicator(\"Bar scope\")\nplot(close * 2, \"Doubled\")\nplot(close, \"Close\")",
        CompileOptions::default(),
    )
    .program
    .expect("two-plot source compiles");
    let feed = |bars: usize| MarketData {
        open: (0..bars).map(|i| i as f64 + 1.0).collect(),
        high: (0..bars).map(|i| i as f64 + 1.0).collect(),
        low: (0..bars).map(|i| i as f64 + 1.0).collect(),
        close: (0..bars).map(|i| i as f64 + 1.0).collect(),
        volume: vec![10.0; bars],
        time: (0..bars)
            .map(|i| 1_700_000_000_000 + i as i64 * 60_000)
            .collect(),
        timeframe: "1".to_string(),
        ..MarketData::default()
    };

    for bars in [4_usize, 4_000] {
        let market = feed(bars);
        let mut session = IncrementalSession::new(&program, &market, &RunOptions::default())
            .expect("history seeds a session");
        let forming = Tick {
            bar_index: bars,
            open: 500.0,
            high: 500.0,
            low: 500.0,
            close: 500.0,
            volume: 10.0,
            time: 1_700_000_000_000 + bars as i64 * 60_000,
            trading_day: None,
            is_bar_close: false,
        };
        let samples = session
            .advance_samples(std::slice::from_ref(&forming))
            .expect("the forming tick executes");
        assert_eq!(samples.len(), 1, "one sample per tick");
        assert_eq!(samples[0].bar_index as usize, bars);
        assert!(!samples[0].is_bar_close);
        // One value per plot slot regardless of the seeded prefix. This is the
        // property the incremental benchmark boundary depends on: anything
        // proportional to `bars` here would make that lane time the history.
        assert_eq!(samples[0].values.len(), 2, "one value per plot slot");
        assert_eq!(samples[0].values[0], Value::Float(1000.0));
        assert_eq!(samples[0].values[1], Value::Float(500.0));

        // Same session state, snapshot shape: every lane from bar zero.
        let snapshot = session.result();
        assert_eq!(snapshot.plots.len(), 2);
        assert_eq!(snapshot.plots[0].values.len(), bars + 1);
        assert_eq!(
            snapshot.plots[0].values.last(),
            Some(&Value::Float(1000.0)),
            "both entry points describe the same forming bar"
        );

        // Repeating the open tick re-executes the same declared bar index
        // instead of drifting through the feed, and committing it agrees with
        // the sample that was just observed.
        let repeated = session
            .advance_samples(std::slice::from_ref(&forming))
            .expect("the forming bar re-executes");
        assert_eq!(repeated[0].values, samples[0].values);
        let committed = session.advance(&[Tick {
            is_bar_close: true,
            ..forming
        }]);
        assert!(committed.runtime_errors.is_empty(), "{committed:?}");
        assert_eq!(committed.plots[0].values.len(), bars + 1);
        assert_eq!(session.committed_bars() as usize, bars + 1);
    }
}

#[test]
fn advance_samples_shares_the_session_terminal_failure_contract() {
    let program = compile(
        "//@version=6\nindicator(\"Terminal\")\nplot(close)",
        CompileOptions::default(),
    )
    .program
    .expect("source compiles");
    let market = MarketData {
        open: vec![1.0, 2.0],
        high: vec![1.0, 2.0],
        low: vec![1.0, 2.0],
        close: vec![1.0, 2.0],
        volume: vec![10.0, 20.0],
        time: vec![1_700_000_000_000, 1_700_000_060_000],
        timeframe: "1".to_string(),
        ..MarketData::default()
    };
    let mut session = IncrementalSession::new(&program, &market, &RunOptions::default())
        .expect("history seeds a session");
    // A tick that goes backwards in time is a structured failure, and the
    // bar-scoped entry point must surface it as an error rather than hand back
    // an empty sample list that a caller could read as a fast success.
    let error = session
        .advance_samples(&[Tick {
            bar_index: 2,
            open: 3.0,
            high: 3.0,
            low: 3.0,
            close: 3.0,
            volume: 30.0,
            time: 0,
            trading_day: None,
            is_bar_close: false,
        }])
        .expect_err("a backwards timestamp fails");
    assert_eq!(error.kind, RuntimeErrorKind::MalformedMarketData);
    assert!(session.is_failed());
    // Terminal means terminal through either entry point.
    assert_eq!(
        session
            .advance_samples(&[Tick {
                bar_index: 2,
                open: 3.0,
                high: 3.0,
                low: 3.0,
                close: 3.0,
                volume: 30.0,
                time: 1_700_000_120_000,
                trading_day: None,
                is_bar_close: false,
            }])
            .expect_err("the session stays terminal")
            .message,
        error.message
    );
    assert_eq!(
        session.advance(&[]).runtime_errors[0].message,
        error.message
    );
}

#[test]
fn overflowing_market_timestamps_are_structured_failures() {
    let program = compile(
        "//@version=6\nindicator(\"Timestamp boundary\")\nplot(time_close)",
        CompileOptions::default(),
    )
    .program
    .expect("time_close source compiles");
    let one_bar = |time, timeframe: String| MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        time: vec![time],
        timeframe,
        ..MarketData::default()
    };
    let appended_tick = |time, is_bar_close| Tick {
        bar_index: 1,
        open: 1.0,
        high: 1.0,
        low: 1.0,
        close: 1.0,
        volume: 1.0,
        time,
        trading_day: None,
        is_bar_close,
    };

    for market in [
        one_bar(i64::MAX, "1D".to_string()),
        one_bar(0, format!("{}S", i64::MAX)),
    ] {
        let result = run_program_profiled(&program, &market, &RunOptions::default(), None);
        assert_eq!(result.bars_executed, 0);
        assert_eq!(result.runtime_errors.len(), 1);
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::MalformedMarketData
        );
        assert!(result.runtime_errors[0]
            .message
            .contains("timeframe close time exceeds the i64 timestamp range"));
    }

    let explicit = one_bar(0, "1D".to_string());
    let mut synthetic = explicit.clone();
    synthetic.time.clear();
    for market in [explicit, synthetic] {
        let mut session = IncrementalSession::new(&program, &market, &RunOptions::default())
            .expect("bounded historical timestamp starts a session");
        let result = session.advance(&[appended_tick(i64::MAX, false)]);
        assert_eq!(result.runtime_errors.len(), 1);
        assert_eq!(
            result.runtime_errors[0].kind,
            RuntimeErrorKind::MalformedMarketData
        );
        assert!(result.runtime_errors[0]
            .message
            .contains("timeframe close time exceeds the i64 timestamp range"));
        assert_eq!(session.resident_bars(), 1);
        assert!(session.is_failed());
    }

    let mut synthetic = one_bar(0, "1D".to_string());
    synthetic.time.clear();
    let synthesized_last_open = synthetic.time_at(0);
    let mut non_monotonic = IncrementalSession::new(&program, &synthetic, &RunOptions::default())
        .expect("synthetic history starts a session");
    let result = non_monotonic.advance(&[appended_tick(synthesized_last_open, false)]);
    assert_eq!(result.runtime_errors.len(), 1);
    assert_eq!(
        result.runtime_errors[0].kind,
        RuntimeErrorKind::MalformedMarketData
    );
    assert!(result.runtime_errors[0]
        .message
        .contains("realtime time array must be strictly increasing"));
    assert_eq!(non_monotonic.resident_bars(), 1);
    assert!(non_monotonic.is_failed());

    let mut session = IncrementalSession::new(&program, &synthetic, &RunOptions::default())
        .expect("synthetic history starts a session");
    let appended_time = 1_700_000_000_000_i64;
    let result = session.advance(&[appended_tick(appended_time, true)]);
    assert!(result.runtime_errors.is_empty(), "{result:?}");
    assert_eq!(
        result.plots[0].values.last(),
        Some(&Value::Float((appended_time + 86_400_000) as f64)),
        "a synthesized history must materialize before appending the host timestamp"
    );
}

#[test]
fn invalid_host_input_values_fall_back_to_validated_defaults() {
    let color_program = compile(
        "//@version=6\nindicator(\"Color input\")\nc = input.color(color.red)\nplot(c == color.red ? 1.0 : 0.0)",
        CompileOptions::default(),
    )
    .program
    .expect("color input source compiles");
    let mut options = RunOptions::default();
    options.inputs.insert(
        0,
        InputValue::Color {
            r: 0,
            g: 0,
            b: 0,
            t: 255.0,
        },
    );
    let result = run_program(&color_program, &market_data(), &options);
    assert!(result.runtime_errors.is_empty(), "{result:?}");
    assert_eq!(result.plots[0].values, vec![Value::Float(1.0); 3]);

    let float_program = compile(
        "//@version=6\nindicator(\"Float input\")\nn = input.float(2.0)\nplot(n)",
        CompileOptions::default(),
    )
    .program
    .expect("float input source compiles");
    let mut options = RunOptions::default();
    options.inputs.insert(0, InputValue::Float(f64::NAN));
    let result = run_program(&float_program, &market_data(), &options);
    assert!(result.runtime_errors.is_empty(), "{result:?}");
    assert_eq!(result.plots[0].values, vec![Value::Float(2.0); 3]);
}

#[test]
fn immutable_inputs_support_independent_concurrent_runs() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    assert_send_sync::<Program>();
    assert_send_sync::<MarketData>();
    assert_send_sync::<RunOptions>();
    assert_send_sync::<CompileResult>();
    assert_send_sync::<RunResult>();
    assert_send::<IncrementalSession>();

    let program = Arc::new(program());
    let market = Arc::new(market_data());
    let options = Arc::new(RunOptions::default());
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let program = Arc::clone(&program);
            let market = Arc::clone(&market);
            let options = Arc::clone(&options);
            std::thread::spawn(move || run_program_profiled(&program, &market, &options, None))
        })
        .collect();
    let mut results = handles
        .into_iter()
        .map(|handle| handle.join().expect("worker must not panic"));
    assert_eq!(results.next(), results.next());
}
