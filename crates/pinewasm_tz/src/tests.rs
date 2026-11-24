extern crate std;

use super::*;
use std::format;
use std::string::String;
use std::vec::Vec;

#[test]
fn the_pinned_release_is_the_one_embedded() {
    #[cfg(feature = "bundled")]
    assert_eq!(jiff_tzdb::VERSION, Some(TZDATA_VERSION));
}

#[test]
fn fixed_offsets_keep_the_forms_pinewasm_accepts() {
    for (text, offset) in [
        ("UTC", 0),
        ("GMT", 0),
        (" UTC ", 0),
        ("UTC+0", 0),
        ("UTC-5", -5 * 3600),
        ("UTC+05:30", 5 * 3600 + 30 * 60),
        ("GMT+0530", 5 * 3600 + 30 * 60),
        ("+05:30", 5 * 3600 + 30 * 60),
        ("-3", -3 * 3600),
        ("UTC+14:59", 14 * 3600 + 59 * 60),
        ("UTC-14:59", -(14 * 3600 + 59 * 60)),
    ] {
        assert_eq!(parse_fixed(text), Some(offset), "{text:?}");
    }
    for text in [
        "",
        "  ",
        "America/New_York",
        "EST",
        "UTC+15",
        "UTC+5:60",
        "UTC5",
        "UTC+",
        "UTC+123",
        "UTC+1:2:3",
        "utc",
        "UTC+٥",
    ] {
        assert_eq!(parse_fixed(text), None, "{text:?}");
    }
}

#[cfg(feature = "bundled")]
mod bundled {
    use super::*;
    use jiff::tz::TimeZone as JiffZone;
    use jiff::Timestamp;

    fn jiff_zone_pair(name: &str) -> (TimeZone<'static>, JiffZone) {
        let (canonical, tzif) = bundled_tzif(name).expect(name);
        let ours = TimeZone::Rules(Zone::from_tzif(tzif).unwrap_or_else(|| panic!("{name}")));
        let theirs = JiffZone::tzif(canonical, tzif).unwrap_or_else(|e| panic!("{name}: {e}"));
        (ours, theirs)
    }

    fn their_offset(zone: &JiffZone, second: i64) -> i32 {
        zone.to_offset(Timestamp::from_second(second).unwrap())
            .seconds()
    }

    /// Seconds since 1970 of 00:00 UTC on 1 January of `year`.
    fn year_start(year: i64) -> i64 {
        days_from_civil(year, 1, 1) * DAY
    }

    #[test]
    fn every_bundled_zone_reads() {
        let names: Vec<&str> = bundled_names().collect();
        assert!(names.len() > 500, "{}", names.len());
        for name in names {
            assert!(
                matches!(resolve(name), Some(TimeZone::Rules(_) | TimeZone::Fixed(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn offsets_match_jiff_on_the_bundled_tzif() {
        let low = year_start(1850);
        let high = year_start(2101);
        let mut checked = 0_u64;
        for name in bundled_names() {
            let (ours, theirs) = jiff_zone_pair(name);
            // Every transition jiff reports, one second either side.
            let mut instants: Vec<i64> = theirs
                .following(Timestamp::from_second(low).unwrap())
                .map(|transition| transition.timestamp().as_second())
                .take_while(|second| *second < high)
                .flat_map(|second| [second - 1, second, second + 1])
                .collect();
            // And the middle of each month.
            for year in 1850..2101 {
                for month in 1..=12 {
                    instants.push(days_from_civil(year, month, 15) * DAY + 12 * 3600);
                }
            }
            for second in instants {
                assert_eq!(
                    ours.offset_at(second),
                    their_offset(&theirs, second),
                    "{name} at {second}"
                );
                checked += 1;
            }
        }
        assert!(checked > 1_000_000, "{checked}");
    }

    #[test]
    fn local_times_resolve_as_an_independent_reader_resolves_them() {
        use jiff::civil::DateTime;
        let low = year_start(1900);
        let high = year_start(2101);
        for name in bundled_names() {
            let (ours, theirs) = jiff_zone_pair(name);
            let transitions: Vec<(i64, i32, i32)> = theirs
                .following(Timestamp::from_second(low).unwrap())
                .map(|transition| transition.timestamp().as_second())
                .take_while(|second| *second < high)
                .map(|second| {
                    (
                        second,
                        their_offset(&theirs, second - 1),
                        their_offset(&theirs, second),
                    )
                })
                .collect();
            for (second, before, after) in transitions {
                if before == after {
                    // Only the daylight-saving flag changes (Nuuk, October
                    // 2023), so every wall-clock reading near it occurs
                    // once. `jiff` resolves some of these to an instant that
                    // does not read back as the same wall clock; the
                    // round-trip test below covers them instead.
                    continue;
                }
                // Wall-clock readings across the change: inside a gap or
                // overlap, at its edges, and an hour either side.
                let edge_before = second + i64::from(before);
                let edge_after = second + i64::from(after);
                for local in [
                    edge_before - 3600,
                    edge_before - 1,
                    edge_before,
                    edge_before + 1,
                    (edge_before + edge_after) / 2,
                    edge_after - 1,
                    edge_after,
                    edge_after + 1,
                    edge_after + 3600,
                ] {
                    let civil: DateTime = Timestamp::from_second(local)
                        .unwrap()
                        .to_zoned(JiffZone::UTC)
                        .datetime();
                    let expected = theirs
                        .to_ambiguous_timestamp(civil)
                        .compatible()
                        .unwrap()
                        .as_second();
                    assert_eq!(
                        ours.utc_from_local(local),
                        expected,
                        "{name} at local {local}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_wall_clock_reading_that_occurs_reads_back_as_itself() {
        // Every local reading outside a gap names an instant whose offset
        // turns it back into the same reading.
        let low = year_start(1900);
        let high = year_start(2101);
        for name in bundled_names() {
            let (ours, theirs) = jiff_zone_pair(name);
            let changes: Vec<i64> = theirs
                .following(Timestamp::from_second(low).unwrap())
                .map(|transition| transition.timestamp().as_second())
                .take_while(|second| *second < high)
                .collect();
            for second in changes {
                for delta in [-7200, -3601, -1, 0, 1, 3599, 7200] {
                    let utc = second + delta;
                    let local = utc + i64::from(ours.offset_at(utc));
                    let back = ours.utc_from_local(local);
                    assert_eq!(
                        back + i64::from(ours.offset_at(back)),
                        local,
                        "{name} at {utc}"
                    );
                }
            }
        }
    }

    #[test]
    fn new_york_follows_its_clock_changes() {
        let zone = resolve("America/New_York").unwrap();
        // 2024-03-10 07:00 UTC: 02:00 EST becomes 03:00 EDT.
        let spring = days_from_civil(2024, 3, 10) * DAY + 7 * 3600;
        assert_eq!(zone.offset_at(spring - 1), -5 * 3600);
        assert_eq!(zone.offset_at(spring), -4 * 3600);
        // 2024-11-03 06:00 UTC: 02:00 EDT becomes 01:00 EST.
        let autumn = days_from_civil(2024, 11, 3) * DAY + 6 * 3600;
        assert_eq!(zone.offset_at(autumn - 1), -4 * 3600);
        assert_eq!(zone.offset_at(autumn), -5 * 3600);
        // 02:30 on 10 March never happens: it reads as 03:30 EDT.
        let local = days_from_civil(2024, 3, 10) * DAY + 2 * 3600 + 1800;
        assert_eq!(zone.utc_from_local(local), spring + 1800);
        // 01:30 on 3 November happens twice: the earlier, EDT.
        let local = days_from_civil(2024, 11, 3) * DAY + 3600 + 1800;
        assert_eq!(zone.utc_from_local(local), autumn - 1800);
        // The footer rule, decades past the last explicit transition.
        let summer_2090 = days_from_civil(2090, 7, 1) * DAY;
        assert_eq!(zone.offset_at(summer_2090), -4 * 3600);
    }

    #[test]
    fn a_zone_that_never_changed_its_offset_is_that_fixed_offset() {
        assert_eq!(resolve("Etc/UTC"), Some(TimeZone::Fixed(0)));
        assert_eq!(resolve("utc"), Some(TimeZone::Fixed(0)));
        assert_eq!(resolve("Etc/GMT+5"), Some(TimeZone::Fixed(-5 * 3600)));
        // Tokyo keeps UTC+9 today but kept other offsets before 1952.
        assert!(matches!(resolve("Asia/Tokyo"), Some(TimeZone::Rules(_))));
        assert!(matches!(
            resolve("America/New_York"),
            Some(TimeZone::Rules(_))
        ));
    }

    #[test]
    fn names_resolve_without_regard_to_case_and_unknown_ones_do_not() {
        assert!(resolve("america/new_york").is_some());
        assert!(resolve(" Asia/Tokyo ").is_some());
        assert_eq!(resolve("UTC-5"), Some(TimeZone::Fixed(-5 * 3600)));
        for name in ["", "America/NewYork", "Mars/Olympus", "UTC+15", "New York"] {
            assert_eq!(resolve(name), None, "{name:?}");
        }
    }
}

#[test]
fn malformed_tzif_is_refused_and_never_panics() {
    // Truncations and single-byte corruptions of a real zone.
    #[cfg(feature = "bundled")]
    {
        let (_, tzif) = bundled_tzif("Europe/Berlin").unwrap();
        for end in 0..tzif.len() {
            if let Some(zone) = Zone::from_tzif(&tzif[..end]) {
                let _ = zone.offset_at(0);
            }
        }
        let mut copy: Vec<u8> = tzif.to_vec();
        for at in 0..copy.len() {
            let original = copy[at];
            for value in [0, 0xFF, b'\n', original.wrapping_add(1)] {
                copy[at] = value;
                if let Some(zone) = Zone::from_tzif(&copy) {
                    for second in [i64::MIN, -1, 0, 1, 1_700_000_000, i64::MAX] {
                        let _ = zone.offset_at(second);
                        let _ = TimeZone::Rules(zone).utc_from_local(second);
                    }
                }
            }
            copy[at] = original;
        }
    }
    assert_eq!(Zone::from_tzif(b""), None);
    assert_eq!(Zone::from_tzif(b"TZif"), None);
}

#[test]
fn posix_rules_parse_the_forms_tzif_footers_use() {
    for rule in [
        "EST5EDT,M3.2.0,M11.1.0",
        "<+0545>-5:45",
        "AEST-10AEDT,M10.1.0,M4.1.0/3",
        "<-03>3<-02>,M3.2.0/-2,M11.1.0/-1",
        "IST-2IDT,M3.4.4/26,M10.5.0",
        "EST5EDT,0/0,J365/25",
        "XXX3YYY,J60,300/1:30:15",
    ] {
        assert!(Posix::parse(rule.as_bytes()).is_some(), "{rule}");
    }
    let mut bad: Vec<String> = [
        "",
        "E5",
        "EST",
        "EST5EDT",
        "EST5EDT,M13.1.0,M11.1.0",
        "EST5EDT,M3.6.0,M11.1.0",
        "EST5EDT,J0,J365",
        "EST5EDT,366,1",
        "EST25",
        "EST5EDT,M3.2.0,M11.1.0,",
        "<AB>5",
    ]
    .iter()
    .map(|s| String::from(*s))
    .collect();
    bad.push(format!("EST5EDT,M3.2.0/{},M11.1.0", 168));
    for rule in &bad {
        assert!(Posix::parse(rule.as_bytes()).is_none(), "{rule}");
    }
}

#[test]
fn civil_dates_round_trip() {
    for days in (-1_000_000..1_000_000).step_by(997) {
        let (year, month, day) = civil_from_days(days);
        assert_eq!(days_from_civil(year, month, day), days);
    }
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(
        days_from_civil(2024, 2, 29) + 1,
        days_from_civil(2024, 3, 1)
    );
    assert_eq!(weekday_of(days_from_civil(2024, 1, 25)), 4);
}
