use pinewasm_compiler::{
    compile, run_program, CompileOptions, InputValue, MarketData, RunOptions, Value,
};
use pinewasm_csil::{CollectionFunc, CsilInstr};

const SOURCE: &str = include_str!("../../../tests/regression/indexed_distance_scan.pine");

fn compiled(source: &str) -> pinewasm_compiler::Program {
    let r = compile(source, CompileOptions::default());
    assert!(r.success, "{:?}", r.diagnostics);
    r.program.unwrap()
}
fn has_scan(p: &pinewasm_compiler::Program) -> bool {
    p.main().code.iter().any(|i| {
        matches!(
            i,
            CsilInstr::CallCollection {
                func: CollectionFunc::InternalDistanceScan,
                ..
            }
        )
    })
}
fn unoptimized_source(source: &str) -> String {
    source.replace(
        "        if d >= lastDistance",
        "        d := d\n        if d >= lastDistance",
    )
}
fn market(n: usize) -> MarketData {
    let close: Vec<_> = (0..n)
        .map(|i| 100.0 + (i as f64 * 0.19).sin() * 10.0)
        .collect();
    MarketData {
        open: close.clone(),
        high: vec![1e6; n],
        low: vec![0.0; n],
        volume: vec![0.0; n],
        close,
        ..MarketData::default()
    }
}
fn options(k: i64, f: i64, start: i64, lookback: i64, high: bool) -> RunOptions {
    let mut o = RunOptions {
        max_instructions_per_run: 10_000_000_000,
        ..RunOptions::default()
    };
    for (id, value) in [(0, k), (1, f), (2, start), (3, lookback)] {
        o.inputs.insert(id, InputValue::Int(value));
    }
    if high {
        o.inputs.insert(
            4,
            InputValue::Source(pinewasm_compiler::BuiltinSeries::High),
        );
    }
    o
}
fn assert_same_outputs(source: &str, n: usize, opts: &RunOptions) {
    let optimized = compiled(source);
    assert!(
        has_scan(&optimized),
        "eligible structural loop was not recognized"
    );
    let unoptimized = compiled(&unoptimized_source(source));
    assert!(!has_scan(&unoptimized));
    let data = market(n);
    let a = run_program(&optimized, &data, opts);
    let b = run_program(&unoptimized, &data, opts);
    assert_eq!(
        a.runtime_errors
            .iter()
            .map(|e| (&e.kind, e.bar_index))
            .collect::<Vec<_>>(),
        b.runtime_errors
            .iter()
            .map(|e| (&e.kind, e.bar_index))
            .collect::<Vec<_>>()
    );
    for (pa, pb) in a.plots.iter().zip(&b.plots) {
        for (i, (x, y)) in pa.values.iter().zip(&pb.values).enumerate() {
            assert_eq!(
                x, y,
                "plot={:?}, bar={i}, n={n}, inputs={:?}",
                pa.title, opts.inputs
            );
        }
    }
    assert_eq!(a.plots, b.plots);
    assert_eq!(a.alerts, b.alerts);
    assert_eq!(a.drawings, b.drawings);
}

#[test]
fn automatic_scan_matches_unoptimized_program_for_sources_warmup_and_settings() {
    for n in [70, 129, 257] {
        for k in [1, 2, 3, 8, 100] {
            for f in [2, 3, 4, 5, 6] {
                for high in [false, true] {
                    assert_same_outputs(SOURCE, n, &options(k, f, 0, 40000, high));
                }
            }
        }
    }
}
#[test]
fn bounds_aliases_and_mutated_prefix_use_correct_fallbacks() {
    for (start, end) in [(0, -1), (31, 1), (32, 64), (70, 100), (0, 1000)] {
        assert_same_outputs(SOURCE, 193, &options(8, 5, start, end, false));
    }
    let alias = SOURCE.replace(
        "var predictions = array.new_float()\nvar distances = array.new_float()",
        "var distances = array.new_float()\nvar predictions = distances",
    );
    assert_same_outputs(&alias, 129, &options(8, 5, 0, 1000, false));
    let mutation = SOURCE.replace("featureArrays = FeatureArrays.new", "if bar_index > 64\n    array.set(values_1, 1, 0.2 + 0.1 * math.sin(bar_index))\nfeatureArrays = FeatureArrays.new");
    assert_same_outputs(&mutation, 193, &options(8, 5, 0, 1000, false));
}
#[test]
fn warmup_keeps_missing_empty_sum() {
    let p = compiled(SOURCE);
    let r = run_program(&p, &market(70), &options(8, 5, 0, 100, false));
    assert!(r.runtime_errors.is_empty());
    assert!(r.plots[0].values[..31]
        .iter()
        .all(|v| matches!(v, Value::Na)));
}
#[test]
fn changed_math_or_effects_decline_recognition() {
    for source in [
        SOURCE.replace("d >= lastDistance", "d > lastDistance"),
        SOURCE.replace("i % 4 != 0", "i % 3 != 0"),
        SOURCE.replace(
            "        if d >=",
            "        array.push(values_1, 0.0)\n        if d >=",
        ),
        SOURCE.replace("settings.neighborsCount*3/4", "settings.neighborsCount*2/4"),
        SOURCE.replace("        if d >=", "        d := nz(d[1])\n        if d >="),
    ] {
        assert!(!has_scan(&compiled(&source)));
    }
}

#[test]
fn realtime_rollback_and_incremental_append_keep_the_same_predictions() {
    use pinewasm_compiler::{IncrementalSession, RealtimeSim, Tick};
    // Make the current feature depend on the changing tick, and close a leaf
    // group on the forming bar so a stale 32-row summary would be observable.
    let source = SOURCE.replace("0.5 + 0.45 * math.sin(bar_index * 0.023)", "close / 200.0");
    let program = compiled(&source);
    assert!(has_scan(&program));
    let unoptimized = compiled(&unoptimized_source(&source));
    let data = market(95);
    let ticks: Vec<_> = [70.0, 170.0, 110.0]
        .into_iter()
        .enumerate()
        .map(|(j, close)| Tick {
            bar_index: 95,
            open: 100.0,
            high: 180.0,
            low: 50.0,
            close,
            volume: 0.0,
            time: 1_700_000_000_000,
            trading_day: None,
            is_bar_close: j == 2,
        })
        .collect();
    let mut opts = options(8, 5, 0, 1000, false);
    opts.realtime = Some(RealtimeSim {
        ticks: ticks.clone(),
    });
    opts.realtime_trace = true;
    let a = run_program(&program, &data, &opts);
    let b = run_program(&unoptimized, &data, &opts);
    assert!(a.runtime_errors.is_empty(), "{:?}", a.runtime_errors);
    assert_eq!(a.plots, b.plots);
    assert_eq!(a.realtime_trace, b.realtime_trace);
    opts.realtime = None;
    let mut a = IncrementalSession::new(&program, &data, &opts).unwrap();
    let mut b = IncrementalSession::new(&unoptimized, &data, &opts).unwrap();
    for tick in ticks {
        assert_eq!(
            a.advance_samples(std::slice::from_ref(&tick)).unwrap(),
            b.advance_samples(&[tick]).unwrap()
        );
    }
    assert_eq!(a.result().plots, b.result().plots);
}

#[test]
fn runtime_abi_admission_and_profile_cost_are_explicit() {
    let program = compiled(SOURCE);
    assert_eq!(
        program.header.runtime_abi_version,
        pinewasm_csil::INDEXED_SCAN_ABI_VERSION
    );
    for old_abi in [0, 1] {
        let mut old = program.clone();
        old.header.runtime_abi_version = old_abi;
        assert!(pinewasm_csil::validate(&old).is_err());
    }
    let restored = pinewasm_compiler::Program::from_json(&program.to_json()).unwrap();
    assert_eq!(restored, program);
    let mut opts = options(8, 5, 0, 1000, false);
    let plain = run_program(&program, &market(129), &opts);
    opts.profile = true;
    let profiled = run_program(&program, &market(129), &opts);
    assert_eq!(plain.plots, profiled.plots);
    assert!(profiled.profile.is_some());
}

#[test]
fn total_guard_does_not_dereference_objects_on_an_empty_loop() {
    let source = SOURCE
        .replace("start = input.int(0, \"Start\")", "start = int(na)")
        .replace(
            "settings = SearchSettings.new(k, f)",
            "SearchSettings settings = na",
        );
    let source = source.replace("if bar_index >= start", "if true");
    assert_same_outputs(&source, 70, &RunOptions::default());
    assert!(
        run_program(&compiled(&source), &market(70), &RunOptions::default())
            .runtime_errors
            .is_empty()
    );
}

#[test]
fn explicit_opt_out_retains_the_unoptimized_program_and_old_runtime_abi() {
    let r = compile(
        SOURCE,
        CompileOptions {
            disable_distance_scan_optimization: true,
            ..CompileOptions::default()
        },
    );
    let program = r.program.unwrap();
    assert!(!has_scan(&program));
    assert!(program.header.runtime_abi_version < pinewasm_csil::INDEXED_SCAN_ABI_VERSION);
    let optimized = compiled(SOURCE);
    let options = options(2, 5, 0, 1000, false);
    assert_eq!(
        run_program(&program, &market(129), &options).plots,
        run_program(&optimized, &market(129), &options).plots
    );
}

#[test]
fn fractional_int_operands_decline_the_kernel_without_truncation() {
    for source in [
        SOURCE.replace("start = input.int(0, \"Start\")", "start = 1 / 2"),
        SOURCE.replace("k = input.int(8, \"Neighbors\")", "k = 7 / 2"),
        SOURCE.replace("f = input.int(5, \"Features\")", "f = 7 / 2"),
    ] {
        assert_same_outputs(&source, 129, &RunOptions::default());
    }
}

#[test]
fn persistent_threshold_and_float_label_idioms_keep_the_unoptimized_loop() {
    for source in [
        SOURCE.replace("lastDistance = -1.0", "varip lastDistance = -1.0"),
        SOURCE.replace(
            "var y_train_array = array.new_int()",
            "var y_train_array = array.new_float()",
        ),
    ] {
        assert!(!has_scan(&compiled(&source)));
    }
}

fn unoptimized_program(source: &str) -> pinewasm_compiler::Program {
    let r = compile(
        source,
        CompileOptions {
            disable_distance_scan_optimization: true,
            ..CompileOptions::default()
        },
    );
    assert!(r.success, "{:?}", r.diagnostics);
    r.program.unwrap()
}

#[test]
fn repeated_prefix_rebuilds_leave_room_for_logs_under_a_tight_memory_budget() {
    let source = SOURCE.replace("featureArrays = FeatureArrays.new", "if bar_index > 64\n    array.set(values_1, 1, 0.2 + 0.1 * math.sin(bar_index))\nfeatureArrays = FeatureArrays.new") + "\nlog.info(\"tick\")\n";
    let p = compiled(&source);
    assert!(has_scan(&p));
    let mut opts = options(8, 5, 0, 40000, false);
    opts.max_vm_bytes_per_run = 2_000_000;
    let data = market(1025);
    let a = run_program(&p, &data, &opts);
    let b = run_program(&unoptimized_program(&source), &data, &opts);
    assert!(
        b.runtime_errors.is_empty(),
        "unoptimized: {:?}",
        b.runtime_errors
    );
    assert!(
        a.runtime_errors.is_empty(),
        "indexed: {:?}",
        a.runtime_errors
    );
    assert_eq!(a.plots, b.plots);
    assert_eq!(a.logs, b.logs);
}

#[test]
fn repeated_last_committed_bar_ticks_do_not_accumulate_cache_reservations() {
    use pinewasm_compiler::{RealtimeSim, Tick};
    let source = SOURCE.replace("0.5 + 0.45 * math.sin(bar_index * 0.023)", "close / 200.0")
        + "\nlog.info(\"tick\")\n";
    let p = compiled(&source);
    let mut opts = options(8, 5, 0, 40000, false);
    opts.max_vm_bytes_per_run = 2_000_000;
    opts.realtime = Some(RealtimeSim {
        ticks: (0..700)
            .map(|i| Tick {
                bar_index: 128,
                open: 100.0,
                high: 180.0,
                low: 50.0,
                close: 70.0 + f64::from(i % 100),
                volume: 0.0,
                time: 1_700_000_000_000,
                trading_day: None,
                is_bar_close: i == 699,
            })
            .collect(),
    });
    let data = market(129);
    let a = run_program(&p, &data, &opts);
    let b = run_program(&unoptimized_program(&source), &data, &opts);
    assert!(
        b.runtime_errors.is_empty(),
        "unoptimized: {:?}",
        b.runtime_errors
    );
    assert!(
        a.runtime_errors.is_empty(),
        "indexed: {:?}",
        a.runtime_errors
    );
    assert_eq!(a.plots, b.plots);
    assert_eq!(a.logs, b.logs);
}

#[test]
fn alternating_scan_sites_share_a_bounded_cache_high_water_mark() {
    let begin = SOURCE.find("if bar_index >= start").unwrap();
    let end = SOURCE[begin..].find("plot(").unwrap() + begin;
    let second = SOURCE[begin..end]
        .replace("settings.", "otherSettings.")
        .replace("predictions", "secondPredictions")
        .replace("distances", "secondDistances")
        .replace("lastDistance", "secondThreshold");
    let source = format!("{SOURCE}\notherSettings = SearchSettings.new(k, 2)\nvar secondPredictions = array.new_float()\nvar secondDistances = array.new_float()\nsecondThreshold = -1.0\n{second}\nplot(array.sum(secondPredictions), \"Second prediction\")\nlog.info(\"tick\")\n");
    let p = compiled(&source);
    assert_eq!(
        p.main()
            .code
            .iter()
            .filter(|i| matches!(
                i,
                CsilInstr::CallCollection {
                    func: CollectionFunc::InternalDistanceScan,
                    ..
                }
            ))
            .count(),
        2
    );
    let mut opts = options(8, 5, 0, 40000, false);
    opts.max_vm_bytes_per_run = 2_000_000;
    let data = market(1025);
    let a = run_program(&p, &data, &opts);
    let b = run_program(&unoptimized_program(&source), &data, &opts);
    assert!(
        b.runtime_errors.is_empty(),
        "unoptimized: {:?}",
        b.runtime_errors
    );
    assert!(
        a.runtime_errors.is_empty(),
        "indexed: {:?}",
        a.runtime_errors
    );
    assert_eq!(a.plots, b.plots);
    assert_eq!(a.logs, b.logs);
}

#[test]
fn large_misaligned_ranges_and_mixed_domains_match_unoptimized_program() {
    let source = SOURCE.replace("if bar_index >= start","if bar_index >= 32766")
        .replace("0.5 + 0.45 * math.sin(bar_index * 0.023)","bar_index % 777 == 0 or bar_index == 32767 ? 2.0 : 0.5 + 0.45 * math.sin(bar_index * 0.023)");
    let p = compiled(&source);
    assert!(has_scan(&p));
    let mut opts = options(8, 5, 33, 40000, false);
    opts.max_collection_elements = 4_000_000;
    let data = market(32769);
    let a = run_program(&p, &data, &opts);
    let b = run_program(&unoptimized_program(&source), &data, &opts);
    assert!(a.runtime_errors.is_empty(), "{:?}", a.runtime_errors);
    assert!(b.runtime_errors.is_empty(), "{:?}", b.runtime_errors);
    assert_eq!(a.plots, b.plots);
}

#[test]
fn strategy_rollbacks_reuse_cache_memory_allowance() {
    for flag in ["calc_on_order_fills", "calc_on_every_history_tick"] {
        let source = SOURCE.replace(
            "indicator(\"Ordered distance scan\")",
            &format!("strategy(\"Ordered distance scan\", {flag}=true)"),
        ) + "\nlog.info(\"tick\")\n";
        let p = compiled(&source);
        assert!(has_scan(&p));
        let mut opts = options(8, 5, 0, 40000, false);
        opts.max_vm_bytes_per_run = 2_000_000;
        let data = market(513);
        let a = run_program(&p, &data, &opts);
        let b = run_program(&unoptimized_program(&source), &data, &opts);
        assert!(
            b.runtime_errors.is_empty(),
            "{flag}, unoptimized: {:?}",
            b.runtime_errors
        );
        assert!(
            a.runtime_errors.is_empty(),
            "{flag}, indexed: {:?}",
            a.runtime_errors
        );
        assert_eq!(a.plots, b.plots);
        assert_eq!(a.logs, b.logs);
        assert_eq!(a.strategy, b.strategy);
    }
}
