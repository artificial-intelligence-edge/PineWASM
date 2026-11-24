//! `timestamp(dateString)` ISO 8601 forms.
//!
//! `dateString` takes an ISO 8601 string of the shape `YYYY-MM-DDThh:mm:ss±hh:mm`,
//! and date-only strings such as `"2020-02-20"` are accepted. The expected epoch
//! values below are computed by hand from the ISO 8601 shape above.
//! Forms outside that shape (`Z`, a weekday prefix, a space instead of `T`)
//! stay rejected.

use pinewasm_compiler::{compile, compile_and_run, CompileOptions, InputValue, MarketData};
use pinewasm_compiler::{RunOptions, Value};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
const MINUTE_MS: i64 = 60_000;
/// 2023-11-15T00:00:00 UTC.
const NOV_15_2023: i64 = 1_700_006_400_000;

/// Compile `input.time(timestamp("<date>"))` and return the folded default.
fn input_time_default(date: &str) -> Result<i64, Vec<Option<String>>> {
    let source = format!(
        "//@version=6\nindicator(\"t\")\nstart = input.time(timestamp(\"{date}\"), \"Start\")\nplot(start)\n"
    );
    let result = compile(&source, CompileOptions::default());
    if !result.success {
        return Err(result.diagnostics.into_iter().map(|d| d.code).collect());
    }
    let program = result.program.expect("program present on success");
    match program.inputs[0].defval {
        InputValue::Int(value) => Ok(value),
        ref other => panic!("{date}: input.time default is not an int: {other:?}"),
    }
}

#[test]
fn iso_date_only_strings_fold_to_utc_midnight() {
    for (date, expected) in [
        ("2023-11-15", NOV_15_2023),
        // Date-only string in the ISO 8601 form.
        ("2020-02-20", 1_582_156_800_000),
        ("2024-02-29", 1_709_164_800_000),
        ("1970-01-01", 0),
        ("1969-12-31", -DAY_MS),
    ] {
        assert_eq!(input_time_default(date), Ok(expected), "{date}");
    }
}

#[test]
fn iso_clock_without_seconds_folds() {
    assert_eq!(
        input_time_default("2023-11-15T09:30"),
        Ok(NOV_15_2023 + 9 * HOUR_MS + 30 * MINUTE_MS)
    );
    assert_eq!(input_time_default("2023-11-15T00:00"), Ok(NOV_15_2023));
}

#[test]
fn iso_utc_offsets_shift_the_wall_clock() {
    for (date, expected) in [
        ("2023-11-15T00:00:00+00:00", NOV_15_2023),
        ("2023-11-15T00:00:00-00:00", NOV_15_2023),
        // Midnight five hours west of UTC is 05:00 UTC.
        ("2023-11-15T00:00:00-05:00", NOV_15_2023 + 5 * HOUR_MS),
        // 09:30 at +05:30 is 04:00 UTC.
        ("2023-11-15T09:30+05:30", NOV_15_2023 + 4 * HOUR_MS),
        (
            "2023-11-15T09:30:15+05:30",
            NOV_15_2023 + 4 * HOUR_MS + 15_000,
        ),
        ("2023-11-15T00:00:00+14:00", NOV_15_2023 - 14 * HOUR_MS),
    ] {
        assert_eq!(input_time_default(date), Ok(expected), "{date}");
    }
}

#[test]
fn iso_date_only_timestamp_is_a_plottable_const() {
    let source = "//@version=6\nindicator(\"t\")\nplot(timestamp(\"2023-11-15\"))\n";
    let data = MarketData {
        open: vec![1.0, 2.0],
        high: vec![1.0, 2.0],
        low: vec![1.0, 2.0],
        close: vec![1.0, 2.0],
        volume: vec![1.0, 1.0],
        ..MarketData::default()
    };
    let run = compile_and_run(
        source,
        &data,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(run.runtime_errors.is_empty(), "{:?}", run.runtime_errors);
    assert_eq!(
        run.plots[0].values,
        vec![Value::Float(NOV_15_2023 as f64); 2]
    );
}

#[test]
fn previously_accepted_date_strings_are_unchanged() {
    for (date, expected) in [
        ("2023-11-15T00:00:00", NOV_15_2023),
        ("2019-11-01T00:00:00", 1_572_566_400_000),
        ("2015 03 03", 1_425_340_800_000),
        ("15 Nov 2023", NOV_15_2023),
        ("Nov 15 2023", NOV_15_2023),
        ("15 Nov 2023 00:00 +0300", NOV_15_2023 - 3 * HOUR_MS),
        ("27 Dec 2024 09:30 -0500", 1_735_309_800_000),
    ] {
        assert_eq!(input_time_default(date), Ok(expected), "{date}");
    }
}

#[test]
fn unconfirmed_and_malformed_forms_stay_rejected() {
    for date in [
        // Not part of the accepted forms: zone designator, weekday prefix,
        // space separator.
        "2023-11-15T00:00:00Z",
        "Wed, 15 Nov 2023 00:00:00 +0000",
        "2023-11-15 00:00",
        "2023-11-15 00:00:00",
        // Not calendar dates or clock times.
        "2023-02-29",
        "2023-02-30",
        "2023-13-01",
        "2023-00-10",
        "2023-11-00",
        "2023-11-15T24:00",
        "2023-11-15T00:60",
        "2023-11-15T00:00:60",
        // Offsets outside the fixed-offset range or not `±hh:mm`.
        "2023-11-15T00:00:00+15:00",
        "2023-11-15T00:00:00+14:01",
        "2023-11-15T00:00:00-14:59",
        "2023-11-15T00:00:00+05:60",
        "2023-11-15T00:00:00+0500",
        "2023-11-15T00:00:00+05",
        "2023-11-15T00:00:00+5:00",
        "2023-11-15T00:00:00 +05:00",
        "2023-11-15T00:00:00+05:00:00",
        "2023-11-15T00:00:00+",
        "2023-11-15T00:00:00UTC+5",
        // An offset needs a clock.
        "2023-11-15+05:00",
        "2023-11-15-05:00",
        // Truncated, padded or unpadded fields.
        "2023-11-15T",
        "2023-11-15T00",
        "2023-11-15T00:",
        "2023-11-15T00:00:",
        "2023-11-15T00:00:00:00",
        "2023-11-15T0:00",
        "2023-11",
        "2023-1-5",
        "23-11-15",
        "02023-11-15",
        "2023-11-15-",
        "2023-11-15 ",
        " 2023-11-15",
        "2023/11/15",
        "20231115",
        "2023-11-15t00:00",
        // Signs are not digits.
        "2023-+1-15",
        "2023-11-+5",
        "2023-11-15T+1:00",
        "2019-+1-01T00:00:00",
        "",
    ] {
        let codes = input_time_default(date).expect_err(date);
        assert!(
            codes.iter().any(|code| code.as_deref() == Some("E0204")),
            "{date}: {codes:?}"
        );
    }
}
