//! Higher-timeframe `time()` / `time_close()` on realtime-appended bars. Both
//! give the open/close time of the requested timeframe's bar containing the
//! current chart bar, with no distinction between past and realtime bars, so a
//! streamed feed must report what one run over the same bars reports. Each
//! expected value below is the bucket open or close time for its timeframe, as
//! that batch run computes it.
//!
//! The per-timeframe memo grows with the feed instead of being rebuilt on
//! every append, so these tests also hold it to a full regroup of the
//! session's current feed and each appended bar to a fixed charge.
//!
//! A feed supplied without a trading-day lane keeps grouping calendar
//! buckets by each bar's close date while its ticks omit `tradingDay`, as one
//! run over the same bars does. A lane built from each bar's open date would
//! move weekly and monthly bucket times of bars whose open and close fall on
//! different exchange dates; the final tests hold the streamed values to the
//! batch ones across week and month boundaries, forming bars and weekend
//! session gaps.
use super::{compile, data_from_close, floats};
use crate::{
    resample, run, IncrementalSession, MarketData, RealtimeSim, RunOptions, Tick,
    MAX_INCREMENTAL_BARS,
};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
/// A UTC midnight, so hourly bars 0..23 share one daily bucket.
const T0: i64 = 18_518 * DAY;
const HISTORY: usize = 40;
const TOTAL: usize = 48;

/// Fixed-width and calendar timeframes. The hourly history carries no
/// trading-day lane and the ticks bring none, so the appends keep grouping
/// the `"W"` memo by each bar's close date.
const SOURCE: &str = "//@version=6\n\
    indicator(\"T\")\n\
    plot(time(\"D\"))\n\
    plot(time_close(\"240\"))\n\
    plot(time(\"240\", bars_back = 1))\n\
    plot(time(\"W\"))\n";

fn hourly(bars: usize) -> MarketData {
    let closes: Vec<f64> = (0..bars).map(|bar| 100.0 + bar as f64).collect();
    let mut data = data_from_close(&closes);
    data.timeframe = "60".to_string();
    data.time = (0..bars as i64).map(|bar| T0 + bar * HOUR).collect();
    data
}

fn hourly_tick(bar_index: usize, price: f64, is_bar_close: bool) -> Tick {
    Tick {
        bar_index,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 1.0,
        time: T0 + bar_index as i64 * HOUR,
        trading_day: None,
        is_bar_close,
    }
}

/// A forming tick and a closing tick for every bar in `bars`.
fn streamed(bars: std::ops::Range<usize>) -> Vec<Tick> {
    bars.flat_map(|bar| {
        [
            hourly_tick(bar, 0.5, false),
            hourly_tick(bar, 100.0 + bar as f64, true),
        ]
    })
    .collect()
}

fn batch_plots() -> Vec<Vec<Option<f64>>> {
    batch_plots_of(SOURCE, TOTAL)
}

fn batch_plots_of(source: &str, bars: usize) -> Vec<Vec<Option<f64>>> {
    let outcome = run(&compile(source), &hourly(bars), &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect()
}

/// Each chart bar's bucket open, as a full regroup of `data` reports it.
fn regrouped(data: &MarketData, timeframe: &str) -> Vec<i64> {
    let sampled = resample::resample_timeframe(data, timeframe);
    sampled
        .bucket_of
        .iter()
        .map(|&bucket| sampled.bars[bucket].time)
        .collect()
}

/// Every `time()` memo entry equals a full regroup of the session's current
/// feed, however the entry was grown.
fn assert_memo_matches_a_full_regroup(session: &IncrementalSession) {
    let memo = &session.vm.time_func_memo;
    assert!(!memo.is_empty());
    for (timeframe, opens) in memo {
        let grouped: Vec<i64> = (0..opens.grouped_bars())
            .map(|bar| opens.get(bar).expect("grouped bar"))
            .collect();
        assert_eq!(
            grouped,
            regrouped(&session.vm.data, timeframe),
            "timeframe {timeframe:?}"
        );
    }
}

#[test]
fn batch_run_gives_the_bucket_time_on_every_bar() {
    let plots = batch_plots();
    assert_eq!(plots[0].len(), TOTAL);
    assert_eq!(plots[1].len(), TOTAL);
    for (bar, (daily, four_hour)) in plots[0].iter().zip(&plots[1]).enumerate() {
        let time = T0 + bar as i64 * HOUR;
        let daily_open = time - time.rem_euclid(DAY);
        let four_hour_close = time - time.rem_euclid(4 * HOUR) + 4 * HOUR;
        assert_eq!(*daily, Some(daily_open as f64), "time(D) bar {bar}");
        assert_eq!(
            *four_hour,
            Some(four_hour_close as f64),
            "time_close(240) bar {bar}"
        );
    }
}

#[test]
fn one_shot_realtime_appended_bars_match_the_batch_run() {
    let options = RunOptions {
        realtime: Some(RealtimeSim {
            ticks: streamed(HISTORY..TOTAL),
        }),
        ..RunOptions::default()
    };
    let outcome = run(&compile(SOURCE), &hourly(HISTORY), &options);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let streamed_plots: Vec<_> = outcome
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect();
    assert_eq!(streamed_plots, batch_plots());
}

#[test]
fn incremental_session_reports_the_forming_bar_on_every_tick() {
    let program = compile(SOURCE);
    let expected = batch_plots();
    let mut session = IncrementalSession::new(&program, &hourly(HISTORY), &RunOptions::default())
        .expect("session");
    // One request per tick, as a streamed feed sends them: the forming bar must
    // already carry its bucket times before the bar closes.
    for tick in streamed(HISTORY..TOTAL) {
        let bar = tick.bar_index;
        let outcome = session.advance(std::slice::from_ref(&tick));
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        for (plot, expected) in outcome.plots.iter().zip(&expected) {
            assert_eq!(
                floats(&plot.values),
                expected[..=bar],
                "bar {bar}, closing tick: {}",
                tick.is_bar_close
            );
        }
        assert_memo_matches_a_full_regroup(&session);
    }
}

#[test]
fn rerunning_the_last_history_bar_keeps_its_bucket_times() {
    // A tick on the last committed bar appends nothing, so the entry built
    // over the history still covers it and is read unchanged.
    let program = compile(SOURCE);
    let expected = batch_plots();
    let mut session = IncrementalSession::new(&program, &hourly(HISTORY), &RunOptions::default())
        .expect("session");
    let last = HISTORY - 1;
    let reserved = session.vm.bytes_reserved;
    for is_bar_close in [false, true] {
        let outcome = session.advance(&[hourly_tick(last, 7.0, is_bar_close)]);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        for (plot, expected) in outcome.plots.iter().zip(&expected) {
            assert_eq!(floats(&plot.values), expected[..=last]);
        }
    }
    assert_eq!(session.vm.bytes_reserved, reserved);
}

#[test]
fn appended_bars_reserve_the_cache_like_one_run_over_the_same_feed() {
    // The reservation is cumulative, so growing the entry on each append
    // must add only the new bars, including on the first append. Subtracting
    // a script without the cache isolates it from the feed and output growth
    // both sessions share.
    const CONTROL: &str =
        "//@version=6\nindicator(\"T\")\nplot(time)\nplot(time)\nplot(time)\nplot(time)\n";
    let reserved = |source: &str, history: usize| {
        let program = compile(source);
        let mut session =
            IncrementalSession::new(&program, &hourly(history), &RunOptions::default())
                .expect("session");
        for tick in streamed(history..TOTAL) {
            let outcome = session.advance(&[tick]);
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        }
        session.vm.bytes_reserved
    };
    let whole_feed = reserved(SOURCE, TOTAL) - reserved(CONTROL, TOTAL);
    let streamed_feed = reserved(SOURCE, HISTORY) - reserved(CONTROL, HISTORY);
    assert!(whole_feed > 0);
    assert_eq!(streamed_feed, whole_feed);
}

#[test]
fn a_session_streamed_to_its_bar_limit_charges_every_appended_bar_alike() {
    // Four distinct timeframes from a one-bar history, streamed up to the
    // session bar limit under the default run budget. Regrouping the whole
    // feed for each timeframe on every append charges about 1.25 billion
    // instructions over this session and exhausts the one-billion default at
    // bar 22,350; grouping only the appended bar keeps every append's charge
    // the same however long the session runs.
    const FOUR: &str = "//@version=6\n\
        indicator(\"T\")\n\
        plot(time(\"240\"))\n\
        plot(time(\"D\"))\n\
        plot(time(\"W\"))\n\
        plot(time_close(\"M\"))\n";
    let expected = batch_plots_of(FOUR, MAX_INCREMENTAL_BARS);
    let program = compile(FOUR);
    let mut session =
        IncrementalSession::new(&program, &hourly(1), &RunOptions::default()).expect("session");
    let mut charges = Vec::with_capacity(MAX_INCREMENTAL_BARS);
    for bar in 1..MAX_INCREMENTAL_BARS {
        let remaining = session.vm.instructions_remaining;
        let samples = session
            .advance_samples(&[hourly_tick(bar, 100.0 + bar as f64, true)])
            .unwrap_or_else(|error| panic!("bar {bar}: {error:?}"));
        charges.push(remaining - session.vm.instructions_remaining);
        assert_eq!(samples.len(), 1, "bar {bar}");
        let batch: Vec<Option<f64>> = expected.iter().map(|plot| plot[bar]).collect();
        assert_eq!(floats(&samples[0].values), batch, "bar {bar}");
    }
    assert_memo_matches_a_full_regroup(&session);
    // Every append costs the same, the first included: with no trading-day
    // lane materialized, no append regroups the calendar timeframes ("W" and
    // "M") over the whole feed.
    assert!(session.vm.data.trading_day.is_empty());
    assert_eq!(
        charges.iter().min(),
        charges.iter().max(),
        "per-append instruction charges vary: {:?} .. {:?}",
        &charges[..4],
        &charges[charges.len() - 4..]
    );
}

/// Daily bars stamped 00:00 UTC on a symbol five hours west of UTC: each bar
/// opens on the previous exchange date and closes on its own, the case in
/// which grouping by the open date and by the close date disagree.
fn west_daily(days: &[i64]) -> MarketData {
    let closes: Vec<f64> = (0..days.len()).map(|bar| 100.0 + bar as f64).collect();
    let mut data = data_from_close(&closes);
    data.timeframe = "1D".to_string();
    data.syminfo.timezone = "UTC-5".to_string();
    // Around the clock, so days, weeks and months start at local midnight.
    data.syminfo.session_hours = "24x7".to_string();
    data.time = days.iter().map(|day| day * DAY).collect();
    data
}

fn west_daily_tick(days: &[i64], bar_index: usize, price: f64, is_bar_close: bool) -> Tick {
    Tick {
        bar_index,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 1.0,
        time: days[bar_index] * DAY,
        trading_day: None,
        is_bar_close,
    }
}

#[test]
fn the_first_append_keeps_calendar_buckets_on_the_batch_grouping() {
    // A feed without a trading-day lane keeps the close-date grouping that one
    // run over the same bars uses. A lane built from each bar's open date would
    // move 1 March into February from the first append on (streamed FEB_25,
    // FEB_25, MAR_2, MAR_2 against the batch FEB_25, MAR_1, MAR_1, MAR_1).
    const SOURCE: &str = "//@version=6\nindicator(\"T\")\nplot(time(\"1M\", bars_back = 3))\n";
    const FEB_25: i64 = 18_317 * DAY;
    const MAR_1: i64 = FEB_25 + 5 * DAY;
    let days: Vec<i64> = (18_317..18_328).collect();

    let program = compile(SOURCE);
    let one_run = run(&program, &west_daily(&days), &RunOptions::default());
    assert!(one_run.errors.is_empty(), "{:?}", one_run.errors);
    let one_run = floats(&one_run.plots[0].values);
    let mut session =
        IncrementalSession::new(&program, &west_daily(&days[..7]), &RunOptions::default())
            .expect("session");
    assert_eq!(session.vm.time_func_memo["1M"].get(5), Some(MAR_1));
    for (bar, batch) in [(7, FEB_25), (8, MAR_1), (9, MAR_1), (10, MAR_1)] {
        assert_eq!(one_run[bar], Some(batch as f64), "one run, bar {bar}");
        let samples = session
            .advance_samples(&[west_daily_tick(&days, bar, 1.0, true)])
            .expect("tick");
        assert_eq!(
            floats(&samples[0].values),
            vec![Some(batch as f64)],
            "streamed session, bar {bar}"
        );
        assert_memo_matches_a_full_regroup(&session);
        assert!(session.vm.data.trading_day.is_empty(), "bar {bar}");
    }
    assert_eq!(session.vm.time_func_memo["1M"].get(5), Some(MAR_1));
}

/// Weekly and monthly bucket times, history reads through `bars_back`, and
/// `time_tradingday`, which the absent lane must leave unchanged.
const CALENDAR: &str = "//@version=6\n\
    indicator(\"T\")\n\
    plot(time(\"W\"))\n\
    plot(time_close(\"W\"))\n\
    plot(time(\"1M\"))\n\
    plot(time_close(\"1M\"))\n\
    plot(time(\"W\", bars_back = 2))\n\
    plot(time(\"1M\", bars_back = 3))\n\
    plot(time(\"D\"))\n\
    plot(time_tradingday)\n";

/// Weekdays from Monday 17 February to Friday 13 March 2020: four weekly
/// boundaries, each after a two-day weekend gap, and the 1 March month
/// boundary on a Sunday, so 2 March opens on 1 March in exchange time.
fn weekdays() -> Vec<i64> {
    (18_309..=18_334)
        .filter(|day: &i64| (day - 4).rem_euclid(7) < 5)
        .collect()
}

#[test]
fn streamed_daily_bars_across_week_and_month_boundaries_match_the_batch_run() {
    let days = weekdays();
    assert_eq!(days.len(), 20);
    let history = 6;
    let program = compile(CALENDAR);
    let batch = run(&program, &west_daily(&days), &RunOptions::default());
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);
    let expected: Vec<Vec<Option<f64>>> = batch
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect();

    let mut session = IncrementalSession::new(
        &program,
        &west_daily(&days[..history]),
        &RunOptions::default(),
    )
    .expect("session");
    // A forming tick, then the closing tick, one request each: the partial
    // bar must already carry the batch bucket times.
    for bar in history..days.len() {
        for (price, is_bar_close) in [(0.5, false), (100.0 + bar as f64, true)] {
            let outcome = session.advance(&[west_daily_tick(&days, bar, price, is_bar_close)]);
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            for (index, (plot, expected)) in outcome.plots.iter().zip(&expected).enumerate() {
                assert_eq!(
                    floats(&plot.values),
                    expected[..=bar],
                    "plot {index}, bar {bar}, closing tick: {is_bar_close}"
                );
            }
            assert_memo_matches_a_full_regroup(&session);
        }
    }
    assert!(session.vm.data.trading_day.is_empty());

    // One run with the same ticks simulated reports the same values.
    let ticks: Vec<Tick> = (history..days.len())
        .flat_map(|bar| {
            [
                west_daily_tick(&days, bar, 0.5, false),
                west_daily_tick(&days, bar, 100.0 + bar as f64, true),
            ]
        })
        .collect();
    let one_shot = run(
        &program,
        &west_daily(&days[..history]),
        &RunOptions {
            realtime: Some(RealtimeSim { ticks }),
            ..RunOptions::default()
        },
    );
    assert!(one_shot.errors.is_empty(), "{:?}", one_shot.errors);
    let one_shot: Vec<_> = one_shot
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect();
    assert_eq!(one_shot, expected);
}

#[test]
fn a_tick_that_brings_a_trading_day_materializes_the_lane() {
    // The host may supply `tradingDay` on ticks for a history sent without
    // one. That append materializes the lane (history bars dated by their
    // open, as `time_tradingday` reports them) so the tick's exact session
    // day is kept, and the memo then equals a full regroup of that feed.
    // Supplying the lane for the history as well keeps calendar buckets on
    // one grouping; docs/runtime.md records this.
    let days = weekdays();
    let program = compile(CALENDAR);
    let history = 6;
    let mut session = IncrementalSession::new(
        &program,
        &west_daily(&days[..history]),
        &RunOptions::default(),
    )
    .expect("session");
    let host_day = days[history] * DAY;
    let outcome = session.advance(&[Tick {
        trading_day: Some(host_day),
        ..west_daily_tick(&days, history, 1.0, true)
    }]);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(session.vm.data.trading_day.len(), history + 1);
    let tradingday = floats(&outcome.plots[7].values);
    assert_eq!(tradingday[history], Some(host_day as f64));
    assert_memo_matches_a_full_regroup(&session);
}

#[test]
fn only_a_tick_that_brings_a_trading_day_reserves_the_history_lane() {
    // A persistent session admits each append's feed growth before it
    // happens. The history's trading-day lane is materialized only by a tick
    // that carries `tradingDay`, so only that append is charged one `i64`
    // per history bar for it; a tick without one appends to no lane and is
    // charged nothing for it.
    let days = weekdays();
    let history = 6;
    let program = compile("//@version=6\nindicator(\"T\")\nplot(close)\n");
    let first_append_charge = |trading_day: Option<i64>| {
        let mut session = IncrementalSession::new(
            &program,
            &west_daily(&days[..history]),
            &RunOptions::default(),
        )
        .expect("session");
        let before = session.vm.bytes_reserved;
        let outcome = session.advance(&[Tick {
            trading_day,
            ..west_daily_tick(&days, history, 1.0, true)
        }]);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        session.vm.bytes_reserved - before
    };
    let without = first_append_charge(None);
    let with = first_append_charge(Some(days[history] * DAY));
    assert_eq!(with - without, history * std::mem::size_of::<i64>());
}

#[test]
fn a_tick_that_brings_a_trading_day_to_an_empty_history_keeps_it() {
    // A session may start from a zero-bar history. The first tick that
    // carries `tradingDay` must start the lane with that host value, as it
    // does after a nonempty history; the empty prefix of past bars must not
    // make the append skip it. Without the lane `time_tradingday` falls back
    // to the bar's open date, which on this symbol five hours west of UTC is
    // the previous exchange date, one day before the host value.
    let days = weekdays();
    let program = compile(CALENDAR);
    let host_day = days[0] * DAY;
    let tick = Tick {
        trading_day: Some(host_day),
        ..west_daily_tick(&days, 0, 1.0, true)
    };

    let mut session =
        IncrementalSession::new(&program, &west_daily(&days[..0]), &RunOptions::default())
            .expect("a zero-bar history seeds a session");
    let outcome = session.advance(std::slice::from_ref(&tick));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(session.vm.data.trading_day, vec![host_day]);
    assert_eq!(
        floats(&outcome.plots[7].values),
        vec![Some(host_day as f64)]
    );
    assert_memo_matches_a_full_regroup(&session);

    let one_shot = run(
        &program,
        &west_daily(&days[..0]),
        &RunOptions {
            realtime: Some(RealtimeSim { ticks: vec![tick] }),
            ..RunOptions::default()
        },
    );
    assert!(one_shot.errors.is_empty(), "{:?}", one_shot.errors);
    assert_eq!(
        floats(&one_shot.plots[7].values),
        vec![Some(host_day as f64)]
    );
}
