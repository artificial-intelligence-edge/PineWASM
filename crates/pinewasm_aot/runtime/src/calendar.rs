//! The timeframe a standalone WebAssembly request names, and the civil
//! calendar of a timestamp in a fixed-offset timezone.
//!
//! These are what the VM reads for `timeframe.*`, `time_close`,
//! `timenow` and the calendar functions (`year(t, tz)`, `dayofmonth(t, tz)`,
//! `weekofyear(t, tz)`, ...). They mirror its session helpers
//! (`crates/pinewasm_vm/src/session.rs`), because this crate
//! builds outside the workspace and cannot link the VM.
//! `tests/standalone_wasm_chart_time.rs` runs the interpreter and an emitted
//! module over the same requests and fails when the two drift.
//!
//! Panic-free like the rest of the blob: no slice indexing, and checked
//! arithmetic wherever an intermediate can leave `i64`.

/// The predicates `timeframe.is*` reads, one bit each, in the timeframe's
/// third scratch cell. The emitter's `timeframe_flag` uses the
/// same numbering.
pub(crate) mod flag {
    pub const SECONDS: i64 = 1 << 0;
    pub const MINUTES: i64 = 1 << 1;
    pub const DAILY: i64 = 1 << 2;
    pub const WEEKLY: i64 = 1 << 3;
    pub const MONTHLY: i64 = 1 << 4;
    pub const DWM: i64 = 1 << 5;
    pub const INTRADAY: i64 = 1 << 6;
}

/// A request's timeframe as the emitted code reads it: four scratch
/// cells, in this order.
#[derive(Clone, Copy)]
pub(crate) struct ChartTimeframe {
    /// `timeframe.in_seconds()`: the nominal length in seconds (`1M` is 30
    /// days), at least 1.
    pub seconds: i64,
    /// `timeframe.multiplier`, at least 1.
    pub multiplier: i64,
    /// The [`flag`] bits of the `timeframe.is*` predicates that hold.
    pub flags: i64,
    /// What `timeframe.in_seconds(timeframe.period)` costs the interpreter
    /// beyond its dispatch unit: a unit per byte of the trimmed timeframe,
    /// the string it passes, after the first (`charge_pure_string_work`).
    pub period_extra: i64,
}

/// The timeframe of a request's `timeframe` string, or `None` when its
/// raw JSON content holds an escape or a non-ASCII byte.
///
/// The interpreter reads any string, but decoding escapes and Unicode
/// whitespace here would be a second implementation of both to keep in
/// step. A timeframe is short ASCII text, so the module asks for exactly
/// that and fails closed on anything else. In what it accepts the scanner
/// has already refused raw control bytes, so the only whitespace `str::trim`
/// would remove is the space.
pub(crate) fn chart_timeframe(raw: &[u8]) -> Option<ChartTimeframe> {
    if raw.iter().any(|&byte| byte == b'\\' || !byte.is_ascii()) {
        return None;
    }
    // `pinewasm_vm::session::timeframe_parts` and
    // `timeframe_seconds_trimmed_bytes` split and parse the trimmed text the
    // same way, and both read an empty timeframe as `1D`; one parse serves
    // the multiplier and the length.
    let trimmed = trim_spaces(raw);
    let (numeral, unit) = if trimmed.is_empty() {
        (trimmed, Some(b'D'))
    } else {
        split_unit(trimmed)
    };
    // A numeral that does not parse counts as one.
    let parsed = parse_i64_decimal(numeral).unwrap_or(1);
    let flags = match unit_class(unit) {
        Class::Seconds => flag::SECONDS | flag::INTRADAY,
        Class::Minutes => flag::MINUTES | flag::INTRADAY,
        Class::Daily => flag::DAILY | flag::DWM,
        Class::Weekly => flag::WEEKLY | flag::DWM,
        Class::Monthly => flag::MONTHLY | flag::DWM,
    };
    Some(ChartTimeframe {
        seconds: nominal_timeframe_seconds(parsed, unit),
        multiplier: parsed.max(1),
        flags,
        period_extra: trimmed.len().saturating_sub(1) as i64,
    })
}

fn trim_spaces(mut bytes: &[u8]) -> &[u8] {
    while let [b' ', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' '] = bytes {
        bytes = rest;
    }
    bytes
}

/// The trimmed timeframe split into its numeral and its trailing ASCII
/// letter, uppercased. Only a trailing letter is a unit.
fn split_unit(bytes: &[u8]) -> (&[u8], Option<u8>) {
    match bytes.split_last() {
        Some((&last, numeral)) if last.is_ascii_alphabetic() => {
            (numeral, Some(last.to_ascii_uppercase()))
        }
        _ => (bytes, None),
    }
}

/// `pinewasm_vm::session::nominal_timeframe_seconds`: a bare number is
/// minutes, `S` seconds, `D` days, `W` weeks, `M` 30-day months, and an
/// unknown unit one day. Products saturate.
fn nominal_timeframe_seconds(multiplier: i64, unit: Option<u8>) -> i64 {
    let multiplier = multiplier.max(1);
    match unit {
        None => multiplier.saturating_mul(60),
        Some(b'S') => multiplier,
        Some(b'D') => multiplier.saturating_mul(86_400),
        Some(b'W') => multiplier.saturating_mul(7).saturating_mul(86_400),
        Some(b'M') => multiplier.saturating_mul(30).saturating_mul(86_400),
        Some(_) => 86_400,
    }
}

enum Class {
    Seconds,
    Minutes,
    Daily,
    Weekly,
    Monthly,
}

/// `pinewasm_vm::session::tf_class_of_unit`: a bare number is minutes and
/// an unknown letter is daily.
fn unit_class(unit: Option<u8>) -> Class {
    match unit {
        None => Class::Minutes,
        Some(b'S') => Class::Seconds,
        Some(b'W') => Class::Weekly,
        Some(b'M') => Class::Monthly,
        Some(_) => Class::Daily,
    }
}

/// `str::parse::<i64>` on ASCII bytes (`pinewasm_hir::parse_i64_decimal_bytes`):
/// an optional sign and at least one digit, `None` when it is malformed or
/// leaves `i64`.
fn parse_i64_decimal(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    // Negative accumulation reaches `i64::MIN` without overflowing.
    let mut value: i64 = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        let digit = i64::from(byte - b'0');
        value = value.checked_mul(10)?;
        value = if negative {
            value.checked_sub(digit)?
        } else {
            value.checked_add(digit)?
        };
    }
    Some(value)
}

/// Calendar parts [`standalone_wasm_calendar`] answers. The emitter's
/// `calendar_part` uses the same numbering.
mod part {
    pub const DAY_OF_WEEK: i32 = 0;
    pub const DAY_OF_MONTH: i32 = 1;
    pub const MONTH: i32 = 2;
    pub const YEAR: i32 = 3;
    pub const HOUR: i32 = 4;
    pub const MINUTE: i32 = 5;
    pub const SECOND: i32 = 6;
    pub const WEEK_OF_YEAR: i32 = 7;
}

/// The `I64` lane's `na`.
const NA: i64 = i64::MIN;

/// One civil part of `time_ms` (epoch milliseconds) in the timezone
/// `offset_seconds` east of UTC: what the interpreter's calendar functions
/// return for a present time. `na` (`i64::MIN`) where the interpreter's is
/// `na`: a week of the year whose ISO Thursday leaves the epoch-millisecond
/// range. An offset that would move the local time out of `i64` is also
/// `na`; the emitter only passes offsets of at most 14h59m, where the
/// interpreter cannot get there either.
#[no_mangle]
pub extern "C" fn standalone_wasm_calendar(part: i32, time_ms: i64, offset_seconds: i64) -> i64 {
    let Some(civil) = civil_from_epoch_ms(time_ms, offset_seconds) else {
        return NA;
    };
    match part {
        part::DAY_OF_WEEK => civil.day_of_week,
        part::DAY_OF_MONTH => civil.day,
        part::MONTH => civil.month,
        part::YEAR => civil.year,
        part::HOUR => civil.hour,
        part::MINUTE => civil.minute,
        part::SECOND => civil.second,
        part::WEEK_OF_YEAR => iso_week_of_year(civil.year, civil.month, civil.day).unwrap_or(NA),
        _ => NA,
    }
}

struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    /// Sunday = 1 .. Saturday = 7.
    day_of_week: i64,
}

/// `pinewasm_vm::session::civil_from_epoch_ms`, with the one addition that
/// can overflow checked.
fn civil_from_epoch_ms(epoch_ms: i64, offset_seconds: i64) -> Option<Civil> {
    let local_secs = epoch_ms.div_euclid(1000).checked_add(offset_seconds)?;
    let days = local_secs.div_euclid(86_400);
    let secs_of_day = local_secs.rem_euclid(86_400);

    // civil_from_days: days since 1970-01-01 -> (y, m, d).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    // 1970-01-01 was a Thursday, and the day-of-week numbering makes Sunday 1.
    let day_of_week = (days.rem_euclid(7) + 4) % 7 + 1;

    Some(Civil {
        year,
        month,
        day,
        hour: secs_of_day / 3600,
        minute: (secs_of_day % 3600) / 60,
        second: secs_of_day % 60,
        day_of_week,
    })
}

/// `pinewasm_vm::session::iso_week_of_year`: the ISO 8601 week (1..=53) of
/// a civil date, `None` when an intermediate leaves `i64`.
fn iso_week_of_year(year: i64, month: i64, day: i64) -> Option<i64> {
    let days = days_from_civil(year, month, day)?;
    let weekday = days.checked_add(3)?.rem_euclid(7) + 1;
    // A week belongs to the year of its Thursday.
    let thursday = days.checked_sub(weekday - 4)?;
    let thursday_ms = thursday.checked_mul(86_400_000)?;
    let thursday_year = civil_from_epoch_ms(thursday_ms, 0)?.year;
    let jan1 = days_from_civil(thursday_year, 1, 1)?;
    Some(thursday.checked_sub(jan1)? / 7 + 1)
}

/// `pinewasm_vm::session::days_from_civil`: days since 1970-01-01 of a
/// civil date with `month` in 1..=12, `None` when an intermediate leaves
/// `i64`.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y.div_euclid(400);
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (mp.checked_mul(153)?.checked_add(2)? / 5)
        .checked_add(day)?
        .checked_sub(1)?;
    let doe = (yoe * 365 + yoe / 4 - yoe / 100).checked_add(doy)?;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
}

/// `timestamp(year, month, day, hour, minute, second)` and
/// `timestamp(timezone, ...)` in a fixed-offset timezone: the epoch
/// milliseconds the interpreter returns (`PureFunc::Timestamp` and
/// `PureFunc::TimestampTz`), `na` (`i64::MIN`) where its is. The emitter
/// passes `na` for a call with an `na` field, so only the arithmetic is here.
///
/// The fields are read as a wall-clock time and turned into epoch
/// milliseconds as `pinewasm_vm::session::epoch_ms_from_civil` does, months
/// carrying into the year and every step checked. The plain form (`zoned`
/// 0) is that reading in UTC. The timezone form (`zoned` 1) is `na` when the
/// reading is within two days of the `i64` limits, as the interpreter's
/// `local_ms_in_range` is, and otherwise moves it by the zone's offset, as
/// `pinewasm_tz::TimeZone::utc_from_local` does for a fixed zone.
///
/// A present result is a whole number of seconds, so it is never `i64::MIN`
/// and never mistaken for `na`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn standalone_wasm_timestamp(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    offset_seconds: i64,
    zoned: i32,
) -> i64 {
    let Some(local) = epoch_ms_from_civil(year, month, day, hour, minute, second) else {
        return NA;
    };
    if zoned == 0 {
        return local;
    }
    const MARGIN: i64 = 2 * 86_400_000;
    if local <= i64::MIN + MARGIN || local >= i64::MAX - MARGIN {
        return NA;
    }
    local
        .div_euclid(1000)
        .saturating_sub(offset_seconds)
        .saturating_mul(1000)
        .saturating_add(local.rem_euclid(1000))
}

/// `pinewasm_vm::session::epoch_ms_from_civil` with no offset: the UTC
/// epoch milliseconds of a civil reading, `None` when an intermediate
/// leaves `i64`. An out-of-range month carries into the year; days, hours,
/// minutes and seconds out of range carry through the arithmetic.
fn epoch_ms_from_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Option<i64> {
    let m0 = month.checked_sub(1)?;
    let year = year.checked_add(m0.div_euclid(12))?;
    let month = m0.rem_euclid(12) + 1;
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400)?
        .checked_add(hour.checked_mul(3600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?
        .checked_mul(1000)
}
