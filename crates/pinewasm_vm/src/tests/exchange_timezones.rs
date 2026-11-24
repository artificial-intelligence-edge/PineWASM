//! Exchange timezones by IANA name, daylight-saving changes included, and
//! the refusals for groupings whose start depends on an unsettled session.
//! Expected instants are written out by hand in UTC.
use super::{compile, data_from_close, floats, run};
use crate::{IncrementalSession, MarketData, RunOptions, RuntimeErrorKind, Tick};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
/// 2024-03-09 00:00 UTC, the day before New York's clocks went forward
/// (2024-03-10 07:00 UTC: 02:00 EST became 03:00 EDT).
const MAR_9: i64 = 1_709_942_400_000;

/// `count` bars `step` apart from `start`, on chart timeframe `timeframe`.
fn feed(start: i64, step: i64, count: usize, timeframe: &str) -> MarketData {
    let mut data = data_from_close(&vec![1.0; count]);
    data.time = (0..count).map(|bar| start + bar as i64 * step).collect();
    data.timeframe = timeframe.to_string();
    data
}

fn new_york(mut data: MarketData, session: &str) -> MarketData {
    data.syminfo.timezone = "America/New_York".to_string();
    data.syminfo.session_hours = session.to_string();
    data
}

/// The plotted values of a script whose plots are all numeric.
fn plots(source: &str, data: &MarketData) -> Vec<Vec<Option<f64>>> {
    let outcome = run(&compile(source), data, &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{source}: {:?}", outcome.errors);
    outcome
        .plots
        .iter()
        .map(|plot| floats(&plot.values))
        .collect()
}

fn error(source: &str, data: &MarketData) -> (RuntimeErrorKind, String) {
    let outcome = run(&compile(source), data, &RunOptions::default());
    let error = outcome
        .errors
        .first()
        .unwrap_or_else(|| panic!("{source}: no error"));
    (error.kind, error.message.clone())
}

#[test]
fn calendar_fields_follow_new_york_clock_changes() {
    // Hourly bars from 2024-03-09 00:00 UTC for three days.
    let data = new_york(feed(MAR_9, HOUR, 72, "60"), "24x7");
    let values = plots(
        "indicator(\"T\")\nplot(hour * 1.0)\nplot(dayofmonth * 1.0)\nplot(hour(time, \"UTC\") * 1.0)\n",
        &data,
    );
    // 2024-03-10 06:00 UTC is 01:00 EST; 07:00 UTC is 03:00 EDT.
    assert_eq!(values[0][30], Some(1.0));
    assert_eq!(values[0][31], Some(3.0));
    assert_eq!(values[1][30], Some(10.0));
    // 2024-03-09 04:00 UTC is still the 8th in New York (23:00 EST).
    assert_eq!(values[1][4], Some(8.0));
    assert_eq!(values[1][5], Some(9.0));
    // An explicit timezone argument overrides the exchange's.
    assert_eq!(values[2][31], Some(7.0));
}

#[test]
fn a_named_timezone_argument_follows_its_own_rules() {
    // A UTC exchange; the script names other zones.
    let data = feed(MAR_9, HOUR, 72, "60");
    let values = plots(
        "indicator(\"T\")\nplot(hour(time, \"America/New_York\") * 1.0)\nplot(hour(time, \"Asia/Tokyo\") * 1.0)\nplot(weekofyear(time, \"America/New_York\") * 1.0)\n",
        &data,
    );
    assert_eq!(values[0][30], Some(1.0));
    assert_eq!(values[0][31], Some(3.0));
    // Tokyo keeps UTC+9: 2024-03-10 15:00 UTC is 00:00 on the 11th.
    assert_eq!(values[1][39], Some(0.0));
    assert_eq!(values[2][0], Some(10.0));
}

#[test]
fn a_wall_clock_timestamp_resolves_gaps_and_overlaps() {
    let data = feed(MAR_9, HOUR, 1, "60");
    let source = concat!(
        "indicator(\"T\")\n",
        // 02:30 on 10 March 2024 never happened in New York: 03:30 EDT.
        "plot(timestamp(\"America/New_York\", 2024, 3, 10, 2, 30, 0) * 1.0)\n",
        // 01:30 on 3 November 2024 happened twice: the first, EDT.
        "plot(timestamp(\"America/New_York\", 2024, 11, 3, 1, 30, 0) * 1.0)\n",
        // Folded at compile time, as an input default must be.
        "plot(input.time(timestamp(\"America/New_York\", 2024, 3, 10, 2, 30, 0)) * 1.0)\n",
        "plot(timestamp(\"UTC-5\", 2024, 3, 10, 2, 30, 0) * 1.0)\n",
    );
    let values = plots(source, &data);
    let spring: i64 = 1_710_055_800_000; // 2024-03-10 07:30 UTC
    let autumn: i64 = 1_730_611_800_000; // 2024-11-03 05:30 UTC
    assert_eq!(values[0][0], Some(spring as f64));
    assert_eq!(values[1][0], Some(autumn as f64));
    assert_eq!(values[2][0], Some(spring as f64));
    // A fixed offset ignores clock changes: 07:30 UTC as well, here.
    assert_eq!(values[3][0], Some(spring as f64));
}

#[test]
fn a_timezone_pinewasm_does_not_know_stops_the_run() {
    let data = feed(MAR_9, HOUR, 3, "60");
    for source in [
        "indicator(\"T\")\nplot(hour(time, \"Mars/Olympus\") * 1.0)\n",
        "indicator(\"T\")\nplot(timestamp(\"New York\", 2024, 1, 1, 0, 0, 0))\n",
        "indicator(\"T\")\nplot(time(\"D\", \"0930-1600\", \"UTC+15\"))\n",
    ] {
        let (kind, message) = error(source, &data);
        assert_eq!(kind, RuntimeErrorKind::Unsupported, "{source}");
        assert!(
            message.contains("is not a timezone PineWASM knows"),
            "{source}: {message}"
        );
    }
}

#[test]
fn market_data_naming_an_unknown_timezone_or_session_is_refused() {
    for (timezone, session) in [
        ("Mars/Olympus", ""),
        ("UTC+15", ""),
        ("", ""),
        ("America/New_York", "nonsense"),
    ] {
        let mut data = feed(MAR_9, HOUR, 3, "60");
        data.syminfo.timezone = timezone.to_string();
        data.syminfo.session_hours = session.to_string();
        let (kind, _) = error("indicator(\"T\")\nplot(close)\n", &data);
        assert_eq!(
            kind,
            RuntimeErrorKind::MalformedMarketData,
            "{timezone:?} {session:?}"
        );
    }
}

/// The bars on which the first plot of `source` is 1.
fn marked(source: &str, data: &MarketData) -> Vec<usize> {
    plots(source, data)[0]
        .iter()
        .enumerate()
        .filter(|(_, value)| **value == Some(1.0))
        .map(|(bar, _)| bar)
        .collect()
}

#[test]
fn a_continuous_session_starts_days_at_new_york_midnight_across_the_change() {
    let data = new_york(feed(MAR_9, HOUR, 72, "60"), "24x7");
    // Midnight EST is 05:00 UTC (bars 5 and 29); after the change, midnight
    // EDT is 04:00 UTC (bar 52).
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
            &data
        ),
        [0, 5, 29, 52]
    );
    // Four-hour buckets would meet the skipped hour: refused.
    let (kind, _) = error(
        "indicator(\"T\")\nplot(timeframe.change(\"240\") ? 1 : 0)\n",
        &data,
    );
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
    // On days without a change they walk from local midnight: 00:00,
    // 04:00, ... EDT on 11 March (04:00, 08:00, ... UTC).
    let data = new_york(feed(MAR_9 + 2 * DAY + 2 * HOUR, HOUR, 24, "60"), "24x7");
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"240\") ? 1 : 0)\n",
            &data
        ),
        [0, 2, 6, 10, 14, 18, 22]
    );
}

#[test]
fn a_day_session_groups_days_weeks_and_months_but_not_intraday_buckets() {
    // Five-minute bars of the 09:30 to 16:00 New York session on Friday 15
    // and Monday 18 March 2024 (13:30 UTC opens, both on daylight time).
    let mut times: Vec<i64> = (0..78)
        .map(|bar| MAR_9 + 6 * DAY + 810 * 60_000 + bar * 300_000)
        .collect();
    times.extend((0..78).map(|bar| MAR_9 + 9 * DAY + 810 * 60_000 + bar * 300_000));
    let mut data = new_york(data_from_close(&vec![1.0; times.len()]), "0930-1600");
    data.time = times;
    data.timeframe = "5".to_string();
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
            &data
        ),
        [0, 78]
    );
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"W\") ? 1 : 0)\n",
            &data
        ),
        [0, 78]
    );
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"M\") ? 1 : 0)\n",
            &data
        ),
        [0]
    );
    for timeframe in ["60", "240", "2D"] {
        let (kind, message) = error(
            &format!("indicator(\"T\")\nplot(timeframe.change(\"{timeframe}\") ? 1 : 0)\n"),
            &data,
        );
        assert_eq!(kind, RuntimeErrorKind::Unsupported, "{timeframe}");
        assert!(message.contains("0930-1600"), "{timeframe}: {message}");
    }
    // The same refusal reaches request.security.
    let (kind, _) = error(
        "indicator(\"T\")\nplot(request.security(syminfo.tickerid, \"60\", close))\n",
        &data,
    );
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
}

#[test]
fn an_overnight_session_needs_a_trading_day_lane_for_its_days() {
    // Hourly futures bars from Sunday 10 March 2024 22:00 UTC (17:00 CDT).
    let mut data = feed(MAR_9 + DAY + 22 * HOUR, HOUR, 30, "60");
    data.syminfo.timezone = "America/Chicago".to_string();
    data.syminfo.session_hours = "1700-1600".to_string();
    let source = "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n";
    let (kind, _) = error(source, &data);
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
    // The trading day of each bar: the session opening at 17:00 on Sunday
    // trades Monday 11 March; the one from 17:00 Monday, Tuesday.
    let monday = MAR_9 + 2 * DAY;
    data.trading_day = (0..30)
        .map(|bar| if bar < 23 { monday } else { monday + DAY })
        .collect();
    assert_eq!(marked(source, &data), [0, 23]);
}

#[test]
fn no_session_on_a_utc_exchange_keeps_the_midnight_walk() {
    // The default market data: UTC, no session declared.
    let data = feed(MAR_9, HOUR, 30, "60");
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
            &data
        ),
        [0, 24]
    );
    // A non-UTC exchange with no session declared is refused instead.
    let mut data = data;
    data.syminfo.timezone = "UTC-5".to_string();
    let (kind, message) = error(
        "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
        &data,
    );
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
    assert!(message.contains("declares no session"), "{message}");
}

#[test]
fn a_timeframe_no_longer_than_the_charts_is_never_refused() {
    // An hourly stock session on New York time: every bar is its own bucket
    // of the chart's period, so no session or clock rule is needed.
    let data = new_york(feed(MAR_9 + 14 * HOUR, HOUR, 30, "60"), "0930-1600");
    let values = plots(
        concat!(
            "indicator(\"T\")\n",
            "plot(timeframe.change(\"60\") ? 1 : 0)\n",
            "plot(request.security(syminfo.tickerid, timeframe.period, close))\n",
            "plot(request.security(syminfo.tickerid, \"60\", close))\n",
            "plot(na(time(\"30\")) ? 1 : 0)\n",
        ),
        &data,
    );
    assert!(values[0].iter().all(|v| *v == Some(1.0)));
    assert!(values[1].iter().all(|v| *v == Some(1.0)));
    assert!(values[2].iter().all(|v| *v == Some(1.0)));
    assert!(values[3].iter().all(|v| *v == Some(1.0)));
}

#[test]
fn sub_day_buckets_are_refused_across_a_clock_change() {
    // 2024-11-03 05:00 to 07:00 UTC: 01:00 EDT, 01:30 EDT, 01:00 EST, 01:30
    // EST. Two different hours share one wall-clock hour.
    const NOV_3_5H: i64 = 1_730_610_000_000;
    let data = new_york(feed(NOV_3_5H, 30 * 60_000, 4, "30"), "24x7");
    let (kind, message) = error(
        "indicator(\"T\")\nplot(timeframe.change(\"60\") ? 1 : 0)\n",
        &data,
    );
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
    assert!(message.contains("changes during them"), "{message}");
    // A day is unaffected: both 01:00 readings fall on 3 November.
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
            &data
        ),
        [0]
    );
    // Without a clock change in the bars, hours group normally.
    let data = new_york(feed(NOV_3_5H + 2 * HOUR, 30 * 60_000, 6, "30"), "24x7");
    assert_eq!(
        marked(
            "indicator(\"T\")\nplot(timeframe.change(\"60\") ? 1 : 0)\n",
            &data
        ),
        [0, 2, 4]
    );
}

#[test]
fn a_zone_that_never_changed_is_utc_for_grouping() {
    // `Etc/UTC` (and a lower-case `utc`) are the fixed offset 0, so an
    // undeclared session keeps the midnight walk.
    for timezone in ["Etc/UTC", "utc"] {
        let mut data = feed(MAR_9, HOUR, 30, "60");
        data.syminfo.timezone = timezone.to_string();
        assert_eq!(
            marked(
                "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
                &data
            ),
            [0, 24],
            "{timezone}"
        );
    }
}

#[test]
fn a_wall_clock_timestamp_at_the_edge_of_time_is_na() {
    let data = feed(MAR_9, HOUR, 1, "60");
    let values = plots(
        "indicator(\"T\")\nplot(na(timestamp(\"America/New_York\", 292278994, 8, 16, 0, 0, 0)) ? 1 : 0)\n",
        &data,
    );
    assert_eq!(values[0][0], Some(1.0));
}

/// 2024-11-03 04:00 UTC: midnight EDT, an hour before New York's clocks went
/// back (06:00 UTC: 02:00 EDT became 01:00 EST).
const NOV_3_4H: i64 = 1_730_606_400_000;

#[test]
fn a_same_period_request_keeps_each_bar_through_the_repeated_hour() {
    // Hourly bars at 04:00 .. 09:00 UTC read 00:00, 01:00 EDT, then 01:00,
    // 02:00, 03:00, 04:00 EST: two bars share the 01:00 wall clock.
    let mut data = new_york(feed(NOV_3_4H, HOUR, 6, "60"), "24x7");
    data.close = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let values = plots(
        "indicator(\"T\")\nplot(request.security(syminfo.tickerid, \"60\", close))\nplot(request.security(syminfo.tickerid, timeframe.period, close))\n",
        &data,
    );
    let own: Vec<Option<f64>> = (1..=6).map(|close| Some(f64::from(close))).collect();
    assert_eq!(values[0], own);
    assert_eq!(values[1], own);
}

#[test]
fn a_live_session_refuses_buckets_once_appended_bars_cross_a_clock_change() {
    // Thirty-minute bars from 04:00 UTC on 3 November 2024: the history ends
    // before the change, so hourly buckets are settled until bars cross it.
    let history = 3;
    let program = compile("indicator(\"T\")\nplot(timeframe.change(\"60\") ? 1 : 0)\n");
    let data = new_york(feed(NOV_3_4H, 30 * 60_000, history, "30"), "24x7");
    let mut session =
        IncrementalSession::new(&program, &data, &RunOptions::default()).expect("session");
    let mut refused = None;
    for bar in history..8 {
        let tick = Tick {
            bar_index: bar,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            time: NOV_3_4H + bar as i64 * 30 * 60_000,
            trading_day: None,
            is_bar_close: true,
        };
        let outcome = session.advance(std::slice::from_ref(&tick));
        if let Some(error) = outcome.errors.first() {
            refused = Some((bar, error.kind, error.message.clone()));
            break;
        }
    }
    // Bar 4 (06:00 UTC) is the first on EST.
    let (bar, kind, message) = refused.expect("the live session must refuse as a fresh run does");
    assert_eq!(bar, 4);
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
    assert!(message.contains("changes during them"), "{message}");
}

#[test]
fn requested_bars_do_not_settle_a_time_bucket() {
    // An overnight session without per-bar trading days: daily request bars
    // settle `request.security`, not `time("D")`, which groups the chart's
    // own bars.
    let mut data = feed(MAR_9 + DAY + 22 * HOUR, HOUR, 30, "60");
    data.syminfo.timezone = "America/Chicago".to_string();
    data.syminfo.session_hours = "1700-1600".to_string();
    let open = MAR_9 + DAY + 22 * HOUR;
    data.request_bars.insert(
        "1D".to_string(),
        crate::RequestTimeframeBars {
            time: vec![open, open + DAY],
            time_close: vec![open + 23 * HOUR, open + DAY + 23 * HOUR],
            open: vec![1.0; 2],
            high: vec![1.0; 2],
            low: vec![1.0; 2],
            close: vec![1.0; 2],
            volume: vec![1.0; 2],
        },
    );
    let (kind, _) = error(
        "indicator(\"T\")\nplot(timeframe.change(\"D\") ? 1 : 0)\n",
        &data,
    );
    assert_eq!(kind, RuntimeErrorKind::Unsupported);
}

#[test]
fn a_session_close_in_the_repeated_hour_follows_the_bars_own_occurrence() {
    // Hourly bars at 05:00 and 06:00 UTC on 3 November 2024: 01:00 EDT and
    // 01:00 EST. A 01:00-01:30 session closes at 01:30 on each bar's own
    // clock: 05:30 and 06:30 UTC, never before the bar's open.
    let data = new_york(feed(NOV_3_4H + HOUR, HOUR, 2, "60"), "24x7");
    let values = plots(
        "indicator(\"T\")\nplot(time_close(\"60\", \"0100-0130\", \"America/New_York\") * 1.0)\n",
        &data,
    );
    let half = 30 * 60_000;
    assert_eq!(values[0][0], Some((NOV_3_4H + HOUR + half) as f64));
    assert_eq!(values[0][1], Some((NOV_3_4H + 2 * HOUR + half) as f64));
}

fn tick(bar: usize, time: i64) -> Tick {
    Tick {
        bar_index: bar,
        open: 1.0,
        high: 1.0,
        low: 1.0,
        close: 1.0,
        volume: 1.0,
        time,
        trading_day: None,
        is_bar_close: true,
    }
}

#[test]
fn incremental_clock_scan_inspects_only_appended_bars() {
    let program = compile("indicator(\"T\")\nplot(na(time(\"60\")) ? 0 : 1)\n");
    let mut data = feed(MAR_9, 60_000, 10, "1");
    data.syminfo.timezone = "Asia/Tokyo".to_string();
    data.syminfo.session_hours = "24x7".to_string();
    let mut session =
        IncrementalSession::new(&program, &data, &RunOptions::default()).expect("session");
    let mut last = 10;
    for bar in 10..30 {
        let outcome = session.advance(&[tick(bar, MAR_9 + bar as i64 * 60_000)]);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let scan = session.vm.time_func_memo["60"].clock_scan_ref();
        // Each append advances the scan by one bar, never rescanning from 0.
        assert_eq!(scan.checked, bar + 1);
        assert!(scan.checked > last);
        last = scan.checked;
    }
}
