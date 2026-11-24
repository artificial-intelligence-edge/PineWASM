//! The contracts the two sides of the engine depend on.
//!
//! `pinewasm_vm` and an emitted artifact run the same code, but they reach
//! it differently: the interpreter passes `Decl` values directly, while an
//! artifact writes them into a data segment and the linked engine reads them
//! back. Everything that has to survive that round trip is pinned here.

use pinewasm_ta::{Decl, Engine, Lanes, Layout, State, TaFunc, MAX_PARAMS, STATE_BYTES};

/// Expected operand table for each `TaFunc`, pinned by this test.
fn expected_engine_operands(func: TaFunc) -> &'static [pinewasm_ta::TaOperand] {
    use pinewasm_ta::TaOperand::{Bool, Color, Cond, Num};
    match func {
        TaFunc::Atr
        | TaFunc::Tr
        | TaFunc::Wpr
        | TaFunc::Sar
        | TaFunc::Dmi
        | TaFunc::Supertrend
        | TaFunc::Obv
        | TaFunc::AccDist
        | TaFunc::Nvi
        | TaFunc::Pvi
        | TaFunc::Pvt
        | TaFunc::Wad
        | TaFunc::Wvad
        | TaFunc::Iii
        | TaFunc::TimeframeChange
        | TaFunc::Vwap => &[],
        TaFunc::VwapSource => &[Num],
        TaFunc::VwapAnchored => &[Num, Cond],
        TaFunc::VwapBands => &[Num, Cond, Num],
        TaFunc::Crossover | TaFunc::Crossunder | TaFunc::Cross | TaFunc::Correlation => &[Num, Num],
        TaFunc::Stoch => &[Num, Num, Num],
        TaFunc::BarsSince => &[Cond],
        TaFunc::ValueWhen | TaFunc::ValueWhenInt => &[Cond, Num],
        TaFunc::ValueWhenBool => &[Cond, Bool],
        TaFunc::ValueWhenColor => &[Cond, Color],
        _ => &[Num],
    }
}

#[test]
fn operand_shape_and_count_match_pre_refactor_table_for_every_ta_function() {
    for func in TaFunc::ALL {
        let expected = expected_engine_operands(func);
        assert_eq!(func.engine_operands(), expected, "{func:?}");
        assert_eq!(func.engine_operand_count(), expected.len(), "{func:?}");
    }
}

/// Flat market data: constant per lane, so a test asserts on the formula
/// rather than on a fixture.
struct Flat {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl Lanes for Flat {
    fn open(&self, _bar: u32) -> f64 {
        self.open
    }
    fn high(&self, _bar: u32) -> f64 {
        self.high
    }
    fn low(&self, _bar: u32) -> f64 {
        self.low
    }
    fn close(&self, _bar: u32) -> f64 {
        self.close
    }
    fn volume(&self, _bar: u32) -> f64 {
        self.volume
    }
    fn time_ms(&self, bar: u32) -> i64 {
        1_577_836_800_000 + i64::from(bar) * 86_400_000
    }
}

const FLAT: Flat = Flat {
    open: 1.0,
    high: 2.0,
    low: 0.5,
    close: 1.5,
    volume: 100.0,
};

/// Drive one declaration over `inputs`, returning each bar's scalar result.
fn run(func: TaFunc, params: &[i64], inputs: &[f64]) -> Vec<f64> {
    let decls = [Decl::new(func, params).expect("declaration fits")];
    let layout = Layout::plan(&decls);
    let mut states = vec![State::Invalid; 1];
    let mut cells = vec![f64::NAN; layout.cells as usize];
    let mut scratch = vec![f64::NAN; layout.scratch as usize];
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
    inputs
        .iter()
        .enumerate()
        .map(|(bar, &x)| {
            engine
                .advance(0, [x, f64::NAN, f64::NAN], &FLAT, bar as u32)
                .expect("advanceable")
                .value
        })
        .collect()
}

#[test]
fn tags_round_trip_in_declaration_order() {
    for (index, func) in TaFunc::ALL.iter().enumerate() {
        assert_eq!(
            func.tag(),
            index as u16,
            "{func:?} is not at its declaration index; TaFunc::ALL is out of order"
        );
        assert_eq!(TaFunc::from_tag(func.tag()), Some(*func));
    }
    // A tag past the last variant is refused rather than mis-decoded: that is
    // what makes reading the table back out of linear memory safe.
    assert_eq!(TaFunc::from_tag(TaFunc::COUNT), None);
    assert_eq!(TaFunc::from_tag(u16::MAX), None);
}

#[test]
fn every_engine_signature_fits_the_decl_table() {
    // `Decl` carries a fixed `[i64; MAX_PARAMS]`, so a signature wider than
    // that could not be represented at all. Four is the current maximum
    // (`ta.alma`: length, offset, sigma, floor); this fails the moment a
    // wider one is added, rather than at emit time on one unlucky script.
    for func in TaFunc::ALL {
        let rules = func.engine_param_rules();
        assert!(
            rules.len() <= MAX_PARAMS,
            "{func:?} declares {} const params, over the {MAX_PARAMS} a Decl holds",
            rules.len()
        );
    }
}

#[test]
fn a_declaration_survives_the_data_segment_round_trip() {
    for func in TaFunc::ALL {
        let params: Vec<i64> = (0..func.engine_param_rules().len())
            .map(|i| (i as i64 + 1) * -7_000_000_003)
            .collect();
        let decl = Decl::new(func, &params).expect("declaration fits");
        let bytes = decl.to_le_bytes();
        assert_eq!(bytes.len(), Decl::SIZE);
        let back = Decl::from_le_bytes(&bytes).expect("round trip");
        assert_eq!(back, decl, "{func:?} did not survive the round trip");
        assert_eq!(back.func(), Some(func));
        for (i, expected) in params.iter().enumerate() {
            assert_eq!(back.param(i), Some(*expected));
        }
        assert_eq!(back.param(params.len()), None);
    }
}

#[test]
fn a_corrupt_table_entry_is_refused_rather_than_decoded() {
    let decl = Decl::new(TaFunc::Ema, &[9]).expect("fits");
    let mut bytes = decl.to_le_bytes();
    // A tag no variant claims.
    bytes[0..2].copy_from_slice(&TaFunc::COUNT.to_le_bytes());
    assert_eq!(Decl::from_le_bytes(&bytes), None);
    // A param count wider than the array.
    let mut bytes = decl.to_le_bytes();
    bytes[2..4].copy_from_slice(&(MAX_PARAMS as u16 + 1).to_le_bytes());
    assert_eq!(Decl::from_le_bytes(&bytes), None);
    // A short slice.
    assert_eq!(
        Decl::from_le_bytes(&decl.to_le_bytes()[..Decl::SIZE - 1]),
        None
    );
}

#[test]
fn the_region_plan_covers_every_ring() {
    // The emitter reserves `region_bytes()` and the artifact's engine lays the
    // states and cells out inside it. If the plan under-counted, a ring would
    // index past the region — so the arithmetic is asserted directly.
    let decls = [
        Decl::new(TaFunc::Ema, &[9]).unwrap(),
        Decl::new(TaFunc::MathSum, &[50]).unwrap(),
        Decl::new(TaFunc::Vwma, &[20]).unwrap(),
        Decl::new(TaFunc::Median, &[33]).unwrap(),
        Decl::new(TaFunc::Hma, &[16]).unwrap(),
        Decl::new(TaFunc::PivotHigh, &[3, 4]).unwrap(),
    ];
    let layout = Layout::plan(&decls);
    assert_eq!(layout.states, decls.len() as u32);
    // sum (0) + vwma pairs (40) + median (33) + hma (16 + round(sqrt 16) = 4)
    // + pivot (3 + 4 + 1) = 151; the scratch is the widest sorted window.
    assert_eq!(layout.cells, 50 + 40 + 33 + 20 + 8);
    assert_eq!(layout.scratch, 33);
    let (states, cells, scratch) = layout.offsets();
    assert_eq!(states, decls.len() as u32 * Decl::SIZE as u32);
    assert_eq!(cells, states + decls.len() as u32 * STATE_BYTES);
    assert_eq!(scratch, cells + layout.cells * 8);
    assert_eq!(layout.region_bytes(), scratch + layout.scratch * 8);
}

#[test]
fn a_second_init_is_a_fresh_run() {
    // The artifact calls `pinewasm_rt_ta_init` at the top of every `run`, which is what
    // makes a resident instance produce identical output twice (standalone WebAssembly §6).
    let decls = [Decl::new(TaFunc::Cum, &[]).unwrap()];
    let layout = Layout::plan(&decls);
    let mut states = vec![State::Invalid; 1];
    let mut cells = vec![f64::NAN; layout.cells as usize];
    let mut scratch = vec![f64::NAN; layout.scratch as usize];

    let mut first = Vec::new();
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
    for bar in 0..5u32 {
        first.push(
            engine
                .advance(0, [f64::from(bar), f64::NAN, f64::NAN], &FLAT, bar)
                .unwrap()
                .value,
        );
    }
    assert_eq!(first, vec![0.0, 1.0, 3.0, 6.0, 10.0]);

    engine.reset();
    let mut second = Vec::new();
    for bar in 0..5u32 {
        second.push(
            engine
                .advance(0, [f64::from(bar), f64::NAN, f64::NAN], &FLAT, bar)
                .unwrap()
                .value,
        );
    }
    assert_eq!(
        first, second,
        "a reset run must reproduce the first exactly"
    );
}

#[test]
fn the_sorting_family_agrees_with_a_sorted_copy() {
    // `ta.median` and the percentiles run through the crate's own heapsort
    // (`slice::sort_unstable_by` can panic, and a reachable panic in the
    // artifact build pulls in machinery the runtime splice rejects). The
    // answer must be the one a plain sorted copy gives.
    let window: Vec<f64> = vec![
        3.5, -1.0, 0.0, -0.0, 12.25, 7.0, 7.0, -8.5, 1e300, -1e300, 0.125, 99.0, 4.0,
    ];
    let out = run(TaFunc::Median, &[window.len() as i64], &window);
    let mut sorted = window.clone();
    sorted.sort_by(f64::total_cmp);
    let expected = sorted[sorted.len() / 2];
    assert_eq!(
        *out.last().unwrap(),
        expected,
        "median disagreed with a sorted copy"
    );

    // Every warm-up bar before the window fills is na.
    assert!(out[..window.len() - 1].iter().all(|v| v.is_nan()));
}

#[test]
fn a_window_is_poisoned_by_one_na_and_recovers() {
    let out = run(TaFunc::MathSum, &[3], &[1.0, 2.0, f64::NAN, 4.0, 5.0, 6.0]);
    assert!(out[0].is_nan() && out[1].is_nan(), "warm-up is na");
    // Bars 2..4 each hold the na cell.
    assert!(out[2].is_nan() && out[3].is_nan() && out[4].is_nan());
    assert_eq!(out[5], 15.0, "the window recovers once the na scrolls out");
}

#[test]
fn a_recursive_function_freezes_on_na_rather_than_reseeding() {
    // `ta.ema`: seed with the mean of the first `length` non-na inputs, then
    // recurse. An na input yields na that bar and leaves the state alone.
    let out = run(TaFunc::Ema, &[2], &[10.0, 20.0, f64::NAN, 30.0]);
    assert!(out[0].is_nan());
    assert_eq!(out[1], 15.0, "seeded with the mean of the first two");
    assert!(out[2].is_nan(), "an na input reports na");
    let alpha = 2.0 / 3.0;
    assert_eq!(
        out[3],
        alpha * 30.0 + (1.0 - alpha) * 15.0,
        "the recursion continued from the frozen value"
    );
}

#[test]
fn an_unknown_declaration_is_not_advanceable() {
    // `ta.change`/`ta.sma` are slot-based, never engine states. The CSIL
    // validator rejects them in this table; reaching one anyway is a
    // structured refusal, not a wrong number.
    let decls = [Decl::new(TaFunc::Sma, &[9]).unwrap()];
    let layout = Layout::plan(&decls);
    let mut states = vec![State::Invalid; 1];
    let mut cells = vec![f64::NAN; layout.cells as usize];
    let mut scratch = vec![f64::NAN; layout.scratch as usize];
    let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
    assert!(engine.advance(0, [1.0; 3], &FLAT, 0).is_none());
    // So is a site index past the table.
    assert!(engine.advance(7, [1.0; 3], &FLAT, 0).is_none());
}

// Expected values come from a direct ordered scan of the window. Scalar
// extrema require the current source but skip earlier missing cells; offset
// extrema use the full-window readiness rule.
fn scan_extremum(func: TaFunc, length: usize, inputs: &[f64]) -> f64 {
    if inputs.len() < length {
        return f64::NAN;
    }
    let window = &inputs[inputs.len() - length..];
    if window.last().is_some_and(|x| x.is_nan())
        || (matches!(func, TaFunc::HighestBars | TaFunc::LowestBars)
            && window.iter().any(|x| x.is_nan()))
    {
        return f64::NAN;
    }
    let value = match func {
        TaFunc::Highest | TaFunc::Lowest => window
            .iter()
            .copied()
            .filter(|x| !x.is_nan())
            .reduce(|best, value| {
                if func == TaFunc::Highest {
                    best.max(value)
                } else {
                    best.min(value)
                }
            })
            // Window extrema never return `-0.0`: `f64::max`/`f64::min`
            // leave the sign of a zero result to the target, and the engine
            // returns `+0.0` on every one.
            .map_or(
                f64::NAN,
                |extremum| if extremum == 0.0 { 0.0 } else { extremum },
            ),
        TaFunc::HighestBars | TaFunc::LowestBars => {
            let mut best = 0;
            for i in 0..length {
                if if func == TaFunc::HighestBars {
                    window[i] >= window[best]
                } else {
                    window[i] <= window[best]
                } {
                    best = i;
                }
            }
            -((length - 1 - best) as f64)
        }
        _ => unreachable!(),
    };
    if value.is_finite() {
        value
    } else {
        f64::NAN
    }
}

fn assert_extremum(actual: f64, expected: f64, context: impl std::fmt::Debug) {
    if expected.is_nan() {
        assert!(actual.is_nan(), "{context:?}: expected na, got {actual}");
    } else {
        // Strict on every target, Miri included: a zero extremum is
        // canonicalized by both the engine and the scan.
        assert_eq!(actual.to_bits(), expected.to_bits(), "{context:?}");
    }
}

#[test]
fn extrema_match_ordered_scan_through_ties_holes_wraps_and_extreme_values() {
    let mut seed = 42u64;
    // Miri still crosses the scan/queue cutoff and wraps every tested queue;
    // native tests also run the larger randomized and long-window coverage.
    let bars = if cfg!(miri) { 192 } else { 4096 };
    let lengths: &[usize] = if cfg!(miri) {
        &[1, 2, 31, 32, 33, 64]
    } else {
        &[1, 2, 3, 10, 31, 32, 33, 100, 301, 1000]
    };
    let inputs: Vec<_> = (0..bars)
        .map(|i| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            match i % 503 {
                0 | 12 | 13 => f64::NAN,
                14 | 16 => -0.0,
                15 | 17 => 0.0,
                18 => f64::MIN,
                19 => f64::MAX,
                20 => f64::INFINITY,
                21 => f64::NEG_INFINITY,
                _ => ((seed >> 32) % 41) as f64 - 20.0,
            }
        })
        .collect();
    for func in [
        TaFunc::Highest,
        TaFunc::Lowest,
        TaFunc::HighestBars,
        TaFunc::LowestBars,
    ] {
        for &length in lengths {
            for (bar, actual) in run(func, &[length as i64], &inputs).into_iter().enumerate() {
                assert_extremum(
                    actual,
                    scan_extremum(func, length, &inputs[..=bar]),
                    (func, length, bar),
                );
            }
        }
        for inputs in [
            [-0.0, 0.0, -0.0, 0.0].repeat(32),
            [0.0, -0.0, 0.0, -0.0].repeat(32),
            vec![f64::INFINITY; 128],
            vec![f64::NEG_INFINITY; 128],
            (0..200).map(f64::from).collect(),
            (0..200).rev().map(f64::from).collect(),
        ] {
            for length in [3, 32, 37] {
                for (bar, actual) in run(func, &[length as i64], &inputs).into_iter().enumerate() {
                    assert_extremum(
                        actual,
                        scan_extremum(func, length, &inputs[..=bar]),
                        (func, length, bar),
                    );
                }
            }
        }
    }
}

#[test]
fn extrema_restore_and_reset_keep_site_local_call_history() {
    let funcs = [
        TaFunc::Highest,
        TaFunc::Lowest,
        TaFunc::HighestBars,
        TaFunc::LowestBars,
    ];
    let lengths: &[i64] = if cfg!(miri) {
        &[3, 31, 32, 37]
    } else {
        &[1, 3, 17, 31, 32, 37]
    };
    for &length in lengths {
        let decls = funcs.map(|func| Decl::new(func, &[length]).unwrap());
        let layout = Layout::plan(&decls);
        assert_eq!(layout.cells, 4 * length as u32);
        let mut states = [State::Invalid; 4];
        let mut cells = vec![f64::NAN; layout.cells as usize];
        let mut scratch = [];
        Engine::init(&decls, &mut states, &mut cells, &mut scratch);
        let mut history: [Vec<f64>; 4] = std::array::from_fn(|_| Vec::new());
        let mut snapshot = (states, cells.clone(), history.clone());
        for replay in 0..3 {
            let start = if replay == 1 {
                (states, cells, history) = snapshot.clone();
                100
            } else {
                if replay == 2 {
                    Engine::init(&decls, &mut states, &mut cells, &mut scratch);
                    history = std::array::from_fn(|_| Vec::new());
                }
                0
            };
            for bar in start..200 {
                if bar == 100 && replay == 0 {
                    snapshot = (states, cells.clone(), history.clone());
                }
                for (site, func) in funcs.iter().enumerate() {
                    if bar % (site + 2) == 0 {
                        continue;
                    }
                    let x = if bar % 113 == 0 {
                        f64::NAN
                    } else if bar < 150 {
                        // Fill and truncate both directions' candidate
                        // caches, including at the snapshot boundary.
                        if site % 2 == 0 {
                            -(bar as f64)
                        } else {
                            bar as f64
                        }
                    } else {
                        ((bar * 13) % 23) as f64
                    };
                    history[site].push(x);
                    let actual = Engine::bind(&decls, &mut states, &mut cells, &mut scratch)
                        .advance(site as u32, [x, f64::NAN, f64::NAN], &FLAT, bar as u32)
                        .unwrap()
                        .value;
                    assert_extremum(
                        actual,
                        scan_extremum(*func, length as usize, &history[site]),
                        (length, replay, site, bar),
                    );
                }
            }
        }
    }
}

#[test]
fn extrema_refuse_truncated_window_storage() {
    let decls = [Decl::new(TaFunc::Highest, &[37]).unwrap()];
    let mut states = [State::Invalid];
    for size in 0..37 {
        let mut cells = vec![f64::NAN; size];
        let mut scratch = [];
        let mut engine = Engine::init(&decls, &mut states, &mut cells, &mut scratch);
        for bar in 0..100 {
            assert!(engine
                .advance(0, [1.0, 0.0, 0.0], &FLAT, bar)
                .unwrap()
                .value
                .is_nan());
        }
    }
}

#[test]
fn extrema_cache_overflow_keeps_omitted_candidates_until_refill_or_domination() {
    let bars = if cfg!(miri) { 256 } else { 4096 };
    let lengths: &[usize] = if cfg!(miri) {
        &[32, 37, 65]
    } else {
        &[32, 37, 65, 1000]
    };
    for direction in [-1.0, 1.0] {
        // Long monotone stretches overflow the cache. Interspersed jumps
        // replace omitted candidates, then retained tails, including ties.
        let mut inputs: Vec<_> = (0..bars).map(|i| direction * f64::from(i)).collect();
        for (i, value) in [(72, 41.0), (73, 41.0), (87, 3.0), (151, 100.0)] {
            inputs[i] = direction * value;
        }
        inputs[169] = f64::NAN;
        for func in [
            TaFunc::Highest,
            TaFunc::Lowest,
            TaFunc::HighestBars,
            TaFunc::LowestBars,
        ] {
            for &length in lengths {
                for (bar, actual) in run(func, &[length as i64], &inputs).into_iter().enumerate() {
                    assert_extremum(
                        actual,
                        scan_extremum(func, length, &inputs[..=bar]),
                        (direction, func, length, bar),
                    );
                }
            }
        }
    }
}

#[test]
fn extrema_preserve_the_existing_memory_layout_at_all_lengths() {
    assert_eq!(pinewasm_ta::STATE_BYTES, 128);
    assert!(std::mem::size_of::<State>() <= 128);
    for func in [
        TaFunc::Highest,
        TaFunc::Lowest,
        TaFunc::HighestBars,
        TaFunc::LowestBars,
    ] {
        for length in [1, 31, 32, 33, 1000, 100_000] {
            let layout = Layout::plan(&[Decl::new(func, &[length]).unwrap()]);
            assert_eq!(layout.cells, length as u32);
            assert_eq!(layout.scratch, 0);
            assert_eq!(layout.states, 1);
        }
    }
}
