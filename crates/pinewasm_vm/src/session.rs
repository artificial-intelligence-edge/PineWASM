//! Timeframe, timezone, trading-session, and civil-calendar helpers: pure
//! string parsing and date/time arithmetic with no `Vm` state.

/// The length of a timeframe string in seconds. Recognizes the second
/// (`"1S"`, `"30S"`), minute (bare `"1"`, `"5"`, `"60"`, `"240"`), and
/// daily/weekly/monthly (`"1D"`, `"3D"`, `"1W"`, `"1M"`) forms. An
/// empty trimmed string or an unrecognized trailing ASCII unit gives one day.
/// A malformed numeric part uses a multiplier of one under the selected unit
/// (one minute for the bare form). Valid multipliers are clamped below at one,
/// and duration products saturate at `i64::MAX`. `1M` is treated as 30 days for
/// close-time arithmetic: a fixed nominal month, with calendar-aware month length
/// not modelled.
pub fn timeframe_in_seconds(tf: &str) -> i64 {
    timeframe_seconds_trimmed_bytes(tf.trim().as_bytes())
}

/// Decode the bytes after the native string caller has trimmed whitespace.
/// Only a trailing ASCII letter is a unit; all other bytes belong to the
/// decimal numeral. Numeric parse errors keep the existing multiplier of one.
#[expect(
    clippy::manual_range_contains,
    reason = "Keep byte bounds explicit in the timeframe decoder"
)]
pub(crate) fn timeframe_seconds_trimmed_bytes(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 86_400;
    }
    let last_index = bytes.len() - 1;
    let last = bytes[last_index];
    let upper = last >= b'A' && last <= b'Z';
    let lower = last >= b'a' && last <= b'z';
    let (num_part, unit) = if upper || lower {
        let uppercase = if lower { last - 32 } else { last };
        (&bytes[..last_index], Some(char::from(uppercase)))
    } else {
        (bytes, None)
    };
    let mult = pinewasm_hir::parse_i64_decimal_bytes(num_part).unwrap_or(1);
    nominal_timeframe_seconds(mult, unit)
}

/// Nominal duration after the timeframe parser has selected its multiplier and
/// optional uppercase unit. Parsing and its defaults remain in the caller.
/// The clamp is applied first, then the saturating products, in a fixed order.
pub(crate) fn nominal_timeframe_seconds(mult: i64, unit: Option<char>) -> i64 {
    let mult = mult.max(1);
    match unit {
        // Bare number = minutes.
        None => mult.saturating_mul(60),
        Some(unit) => {
            // Equality branches keep the parser's cases without a char switch,
            // which the Aeneas-extractable subset does not translate.
            if unit == 'S' {
                mult
            } else if unit == 'D' {
                mult.saturating_mul(86_400)
            } else if unit == 'W' {
                mult.saturating_mul(7).saturating_mul(86_400)
            } else if unit == 'M' {
                mult.saturating_mul(30).saturating_mul(86_400)
            } else {
                // Unrecognized unit: one day.
                86_400
            }
        }
    }
}

#[cfg(test)]
mod nominal_timeframe_tests {
    use super::{nominal_timeframe_seconds, timeframe_in_seconds};

    // Test copy of the parsing and multiplier logic, used as a test expectation.
    fn reference_timeframe_in_seconds(tf: &str) -> i64 {
        let tf = tf.trim();
        if tf.is_empty() {
            return 86_400;
        }
        let (num_part, unit) = match tf.chars().last() {
            Some(c) if c.is_ascii_alphabetic() => {
                (&tf[..tf.len() - 1], Some(c.to_ascii_uppercase()))
            }
            _ => (tf, None),
        };
        let mult: i64 = num_part.parse().unwrap_or(1).max(1);
        match unit {
            None => mult.saturating_mul(60),
            Some('S') => mult,
            Some('D') => mult.saturating_mul(86_400),
            Some('W') => mult.saturating_mul(7).saturating_mul(86_400),
            Some('M') => mult.saturating_mul(30).saturating_mul(86_400),
            Some(_) => 86_400,
        }
    }

    // The wide-integer test model avoids reproducing saturating products.
    fn nominal_oracle(mult: i64, unit: Option<char>) -> i64 {
        let factor = match unit {
            None => 60_i128,
            Some('S') => 1,
            Some('D') => 86_400,
            Some('W') => 604_800,
            Some('M') => 2_592_000,
            Some(_) => return 86_400,
        };
        i64::try_from((i128::from(mult.max(1)) * factor).min(i128::from(i64::MAX)))
            .expect("positive product is capped at i64::MAX")
    }

    #[test]
    fn all_unicode_units_and_none_match_wide_oracle() {
        let multipliers = [i64::MIN, -1, 0, 1, 2, 31, i64::MAX / 60, i64::MAX];
        for mult in multipliers {
            assert_eq!(
                nominal_timeframe_seconds(mult, None),
                nominal_oracle(mult, None)
            );
        }
        let mut units = 0;
        for codepoint in 0..=0x10_FFFF {
            if let Some(unit) = char::from_u32(codepoint) {
                units += 1;
                for mult in multipliers {
                    assert_eq!(
                        nominal_timeframe_seconds(mult, Some(unit)),
                        nominal_oracle(mult, Some(unit)),
                        "mult={mult}, unit={unit:?}"
                    );
                }
            }
        }
        assert_eq!(units, 1_112_064);
    }

    #[test]
    fn clamp_scale_and_every_saturation_boundary() {
        let mut multipliers = vec![i64::MIN, -100, -1, 0, 1, 2, i64::MAX];
        for factor in [7, 30, 60, 86_400, 604_800, 2_592_000] {
            let boundary = i64::MAX / factor;
            multipliers.extend([boundary - 1, boundary, boundary + 1]);
        }
        for mult in multipliers {
            for unit in [None, Some('S'), Some('D'), Some('W'), Some('M'), Some('X')] {
                let actual = nominal_timeframe_seconds(mult, unit);
                assert_eq!(actual, nominal_oracle(mult, unit), "{mult}, {unit:?}");
                assert!(actual > 0);
            }
        }
    }

    #[test]
    fn parser_defaults_remain_exact() {
        for (text, expected) in [
            ("", 86_400),
            (" \t\r\n", 86_400),
            ("\u{2003}", 86_400),
            ("+", 60),
            ("-", 60),
            ("é", 60),
            ("💡", 60),
            ("0", 60),
            ("-9", 60),
            ("S", 1),
            ("D", 86_400),
            ("1m", 2_592_000),
            ("+2D", 172_800),
            ("1.5", 60),
            ("1X", 86_400),
            ("9223372036854775808S", 1),
            ("9223372036854775807D", i64::MAX),
            ("-9223372036854775808W", 604_800),
        ] {
            assert_eq!(timeframe_in_seconds(text), expected, "{text:?}");
            assert_eq!(
                timeframe_in_seconds(text),
                reference_timeframe_in_seconds(text)
            );
        }
    }

    #[test]
    fn parser_matches_frozen_body_on_unit_number_and_whitespace_grid() {
        let numbers = [
            "",
            "0",
            "1",
            "2",
            "-1",
            "+1",
            "0008",
            "1.5",
            "--1",
            "1_000",
            "é",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775808",
        ];
        let whitespace = ["", " ", "\t", "\r\n", "\u{2003}"];
        let mut cases = 0;
        for number in numbers {
            for codepoint in 0..=127 {
                let unit = char::from_u32(codepoint).expect("ASCII is a scalar value");
                for space in whitespace {
                    let text = format!("{space}{number}{unit}{space}");
                    assert_eq!(
                        timeframe_in_seconds(&text),
                        reference_timeframe_in_seconds(&text),
                        "{text:?}"
                    );
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 8_960);
    }

    #[test]
    fn parser_matches_frozen_body_on_generated_malformed_inputs() {
        let alphabet = [
            '0', '1', '9', '+', '-', '.', '_', 'S', 'd', 'w', 'M', 'X', ' ', '\t', 'é', '💡',
        ];
        let mut state = 0xD1CE_BA5E_F00D_1234_u64;
        for _ in 0..4096 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let len = usize::try_from(state % 25).expect("length is below 25");
            let mut text = String::new();
            for _ in 0..len {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let index = usize::try_from(state % 16).expect("alphabet index is below 16");
                text.push(alphabet[index]);
            }
            assert_eq!(
                timeframe_in_seconds(&text),
                reference_timeframe_in_seconds(&text),
                "{text:?}"
            );
        }
    }
}

#[cfg(test)]
mod timeframe_bytes_tests {
    use super::{timeframe_in_seconds, timeframe_seconds_trimmed_bytes};

    fn reference_timeframe_in_seconds(tf: &str) -> i64 {
        let tf = tf.trim();
        if tf.is_empty() {
            return 86_400;
        }
        let (num_part, unit) = match tf.chars().last() {
            Some(c) if c.is_ascii_alphabetic() => {
                (&tf[..tf.len() - 1], Some(c.to_ascii_uppercase()))
            }
            _ => (tf, None),
        };
        let mult: i64 = num_part.parse().unwrap_or(1).max(1);
        match unit {
            None => mult.saturating_mul(60),
            Some('S') => mult,
            Some('D') => mult.saturating_mul(86_400),
            Some('W') => mult.saturating_mul(7).saturating_mul(86_400),
            Some('M') => mult.saturating_mul(30).saturating_mul(86_400),
            Some(_) => 86_400,
        }
    }

    fn assert_frozen(text: &str) {
        let actual = timeframe_in_seconds(text);
        assert_eq!(actual, reference_timeframe_in_seconds(text), "{text:?}");
        assert!(actual > 0, "{text:?}");
    }

    #[test]
    fn shared_decimal_wrapper_matches_std_on_short_and_long_inputs() {
        fn assert_parse(text: &str) {
            assert_eq!(
                pinewasm_hir::parse_i64_decimal_bytes(text.as_bytes()),
                text.parse::<i64>().ok(),
                "{text:?}"
            );
        }
        fn visit(text: &mut String, remaining: usize, count: &mut usize) {
            assert_parse(text);
            *count += 1;
            if remaining > 0 {
                for ch in ['+', '-', '0', '1', '9', 'x', ' ', '\n'] {
                    text.push(ch);
                    visit(text, remaining - 1, count);
                    text.pop();
                }
            }
        }
        let mut count = 0;
        visit(&mut String::new(), 4, &mut count);
        assert_eq!(count, 4681);
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            for prefix in ["", "+", "-", "000000000000000000000000000000000000"] {
                for suffix in ["", "x", "0", "9", "\0", "é"] {
                    assert_parse(&format!("{prefix}{value}{suffix}"));
                }
            }
        }
        for sign in ["", "+", "-"] {
            for digit in ["0", "9"] {
                for length in [256, 4096, 100_000] {
                    assert_parse(&format!("{sign}{}", digit.repeat(length)));
                }
            }
        }
        for bytes in [&[0xFF][..], &[0xC0, 0xAF], &[0, b'1'], &[b'1', 0x80]] {
            assert_eq!(pinewasm_hir::parse_i64_decimal_bytes(bytes), None);
        }
    }

    #[test]
    fn all_unicode_terminal_scalars_preserve_full_parser() {
        let stems = ["", "1", "-1", "9223372036854775808"];
        let mut units = 0;
        for scalar in 0..=0x10_FFFF {
            if let Some(unit) = char::from_u32(scalar) {
                units += 1;
                for stem in stems {
                    assert_frozen(&format!("{stem}{unit}"));
                }
            }
        }
        assert_eq!(units, 1_112_064);
    }

    #[test]
    fn every_terminal_byte_has_defined_exact_default_and_unit_behavior() {
        fn oracle(bytes: &[u8]) -> i64 {
            let Some((&last, prefix)) = bytes.split_last() else {
                return 86_400;
            };
            let (number, unit) = if last.is_ascii_alphabetic() {
                (prefix, Some(last.to_ascii_uppercase()))
            } else {
                (bytes, None)
            };
            let mult = std::str::from_utf8(number)
                .ok()
                .and_then(|text| text.parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            let factor = match unit {
                None => 60_i128,
                Some(b'S') => 1,
                Some(b'D') => 86_400,
                Some(b'W') => 604_800,
                Some(b'M') => 2_592_000,
                Some(_) => return 86_400,
            };
            i64::try_from((i128::from(mult) * factor).min(i128::from(i64::MAX)))
                .expect("positive duration is capped at i64::MAX")
        }
        assert_eq!(timeframe_seconds_trimmed_bytes(b""), oracle(b""));
        for stem in [
            "",
            "+",
            "-",
            "0",
            "1",
            "-1",
            "1.5",
            "é",
            "9223372036854775807",
            "9223372036854775808",
        ] {
            for last in 0..=u8::MAX {
                let mut bytes = stem.as_bytes().to_vec();
                bytes.push(last);
                assert_eq!(
                    timeframe_seconds_trimmed_bytes(&bytes),
                    oracle(&bytes),
                    "{bytes:?}"
                );
            }
        }
        // This helper receives already-trimmed bytes, so raw whitespace is
        // intentionally distinct from the native String wrapper.
        assert_eq!(timeframe_seconds_trimmed_bytes(b" "), 60);
        assert_eq!(timeframe_in_seconds(" "), 86_400);
        assert_eq!(timeframe_seconds_trimmed_bytes(b" 2D "), 60);
        assert_eq!(timeframe_in_seconds(" 2D "), 172_800);
    }

    #[test]
    fn unicode_whitespace_and_multibyte_prefixes_preserve_native_trim() {
        let spaces = [
            "",
            " ",
            "\t\r\n",
            "\u{000B}\u{000C}",
            "\u{0085}",
            "\u{00A0}",
            "\u{1680}",
            "\u{2000}",
            "\u{2003}",
            "\u{200A}",
            "\u{2028}",
            "\u{2029}",
            "\u{202F}",
            "\u{205F}",
            "\u{3000}",
        ];
        for before in spaces {
            for after in spaces {
                for middle in [
                    "",
                    "2",
                    "+2d",
                    "-2W",
                    "S",
                    "1X",
                    "éS",
                    "💡m",
                    "١D",
                    "１M",
                    "\0D",
                    "\u{200B}1D",
                ] {
                    assert_frozen(&format!("{before}{middle}{after}"));
                }
            }
        }
    }

    #[test]
    fn signed_boundaries_long_numerals_and_saturation_preserve_full_parser() {
        let mut numbers = vec![
            String::new(),
            "+".to_owned(),
            "-".to_owned(),
            "++1".to_owned(),
            "--1".to_owned(),
            "1.5".to_owned(),
            "1_000".to_owned(),
            "9223372036854775808".to_owned(),
            "-9223372036854775809".to_owned(),
        ];
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            numbers.push(value.to_string());
        }
        for factor in [7, 30, 60, 86_400, 604_800, 2_592_000] {
            let boundary = i64::MAX / factor;
            for value in [boundary - 1, boundary, boundary + 1] {
                numbers.push(value.to_string());
            }
        }
        for sign in ["", "+", "-"] {
            for length in [256, 4096, 100_000] {
                numbers.push(format!("{sign}{}", "0".repeat(length)));
                numbers.push(format!("{sign}{}", "9".repeat(length)));
            }
        }
        for number in numbers {
            for suffix in ["", "S", "s", "D", "d", "W", "w", "M", "m", "X", "é", "💡"] {
                assert_frozen(&format!("{number}{suffix}"));
            }
        }
        for (text, expected) in [
            ("", 86_400),
            ("+", 60),
            ("0", 60),
            ("-1S", 1),
            ("1m", 2_592_000),
            ("1X", 86_400),
            ("9223372036854775808S", 1),
        ] {
            assert_eq!(timeframe_in_seconds(text), expected, "{text:?}");
        }
    }
}

/// A timeframe's unit class, used by the `timeframe.is*` predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TfClass {
    Seconds,
    Minutes,
    Daily,
    Weekly,
    Monthly,
}

/// Split a timeframe string into its trailing unit letter (uppercased) and
/// the leading numeric part. A bare number is minutes (no unit letter). An
/// empty/malformed string is treated as `"1D"` (daily), matching
/// [`timeframe_in_seconds`].
pub(crate) fn timeframe_parts(tf: &str) -> (i64, Option<char>) {
    let tf = tf.trim();
    if tf.is_empty() {
        return (1, Some('D'));
    }
    let (num_part, unit) = match tf.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&tf[..tf.len() - 1], Some(c.to_ascii_uppercase())),
        _ => (tf, None),
    };
    let mult = num_part.parse().unwrap_or(1).max(1);
    (mult, unit)
}

/// The unit class of a parsed timeframe's trailing unit letter
/// ([`timeframe_parts`]; drives `timeframe.isdaily`, etc.): a bare number is
/// minutes, and an unrecognized letter falls back to daily (matching
/// [`timeframe_in_seconds`]). Extracted with the `timeframe.*` read kernel
/// (`vm_ops::timeframe_series_value`), which also yields the multiplier
/// (`timeframe.multiplier`: 5 for `"5"`, 30 for `"30S"`, 1 for `"1D"`).
#[inline(always)]
pub(crate) fn tf_class_of_unit(unit: Option<char>) -> TfClass {
    // An `if` chain on `==` rather than a `match` on the letter: Aeneas does
    // not translate a switch on a `char`.
    match unit {
        None => TfClass::Minutes,
        Some(c) => {
            if c == 'S' {
                TfClass::Seconds
            } else if c == 'W' {
                TfClass::Weekly
            } else if c == 'M' {
                TfClass::Monthly
            } else {
                // `D`, and the fallback for an unrecognized letter.
                TfClass::Daily
            }
        }
    }
}

/// Format a length in seconds back into a canonical timeframe string
/// (`timeframe.from_seconds`). Chooses the largest exact unit: whole days →
/// `"<n>D"`, whole minutes → `"<n>"` (bare minutes), else `"<n>S"`. A
/// non-positive input yields `"1S"` (the smallest representable timeframe).
pub fn timeframe_from_seconds(seconds: i64) -> String {
    if seconds <= 0 {
        return "1S".to_string();
    }
    if seconds % 86_400 == 0 {
        return format!("{}D", seconds / 86_400);
    }
    if seconds % 60 == 0 {
        return format!("{}", seconds / 60);
    }
    format!("{seconds}S")
}

/// The time zone `name` denotes: a fixed offset in a form
/// [`timezone_offset_seconds`] reads, or an IANA zone of the pinned release
/// [`pinewasm_tz::TZDATA_VERSION`] (`"America/New_York"`), whose offset
/// follows every daylight-saving change. `None` for any other name, an empty
/// one included: there is no fallback to UTC.
///
/// Recent names are cached, because a script reads the exchange timezone,
/// and any it names, on every bar.
pub fn resolve_timezone(name: &str) -> Option<pinewasm_tz::TimeZone<'static>> {
    const CAPACITY: usize = 8;
    // A poisoned lock only means another thread panicked mid-update; the
    // cache holds plain values, so keep using it.
    let mut recent = RECENT_TIMEZONES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, zone)) = recent.iter().find(|(cached, _)| cached == name) {
        return *zone;
    }
    let zone = pinewasm_tz::resolve(name);
    if recent.len() == CAPACITY {
        recent.remove(0);
    }
    recent.push((name.to_string(), zone));
    zone
}

/// A timezone name and what [`resolve_timezone`] resolved it to.
type ResolvedTimezone = (String, Option<pinewasm_tz::TimeZone<'static>>);

/// The names [`resolve_timezone`] resolved most recently, oldest first.
static RECENT_TIMEZONES: std::sync::Mutex<Vec<ResolvedTimezone>> =
    std::sync::Mutex::new(Vec::new());

/// The offset of `zone` in seconds east of UTC at the epoch-millisecond
/// instant `time`.
pub(crate) fn offset_at_ms(zone: &pinewasm_tz::TimeZone<'_>, time: i64) -> i64 {
    i64::from(zone.offset_at(time.div_euclid(1000)))
}

/// Whether an epoch-millisecond wall-clock reading is far enough inside the
/// i64 range that any offset (under a day) converts it without overflow.
pub(crate) fn local_ms_in_range(local: i64) -> bool {
    const MARGIN: i64 = 2 * 86_400_000;
    local > i64::MIN + MARGIN && local < i64::MAX - MARGIN
}

/// The epoch-millisecond instant of `local`, an epoch-millisecond reading of
/// `zone`'s wall clock, by [`pinewasm_tz::TimeZone::utc_from_local`]'s rules
/// for readings that occur twice or never.
pub(crate) fn utc_from_local_ms(zone: &pinewasm_tz::TimeZone<'_>, local: i64) -> i64 {
    let seconds = zone.utc_from_local(local.div_euclid(1000));
    seconds
        .saturating_mul(1000)
        .saturating_add(local.rem_euclid(1000))
}

/// Parse a fixed-offset timezone string to an offset in **seconds** east of
/// UTC. Recognizes `"UTC"`/`"GMT"` (0), and the `UTC±HH[:MM]` / `GMT±HH[MM]`
/// forms (`"UTC-5"`, `"UTC+05:30"`, `"GMT+0530"`). Full IANA names (e.g.
/// `"America/New_York"`) are NOT resolved — pulling in a tz database is a
/// deliberate non-dependency, so such strings return `0` (UTC) as a known
/// limitation.
pub fn timezone_offset_seconds(tz: &str) -> i64 {
    let tz = tz.trim();
    if tz.is_empty() {
        return 0;
    }
    // Strip a leading "UTC"/"GMT" prefix; what remains is the signed offset.
    let rest = tz
        .strip_prefix("UTC")
        .or_else(|| tz.strip_prefix("GMT"))
        .unwrap_or(tz);
    if rest.is_empty() {
        return 0; // bare "UTC"/"GMT"
    }
    let (negative, digits) = match rest.as_bytes()[0] {
        b'+' => (false, &rest[1..]),
        b'-' => (true, &rest[1..]),
        // Anything else (an IANA name like "America/New_York") is unsupported.
        _ => return 0,
    };
    // Accept "H", "HH", "HH:MM", or "HHMM". Lengths are byte lengths, and
    // the "HHMM" split is guarded by a char-boundary check: `digits` comes
    // from a user-script string, and byte-slicing a multi-byte string at 2
    // would panic (a wasm trap in the browser). Non-ASCII content fails the
    // i64 parses and lands on the unsupported-timezone fallback either way.
    let (hh, mm) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok(), m.parse::<i64>().ok()),
        None => match digits.len() {
            1 | 2 => (digits.parse::<i64>().ok(), Some(0)),
            4 if digits.is_char_boundary(2) => (
                digits[..2].parse::<i64>().ok(),
                digits[2..].parse::<i64>().ok(),
            ),
            _ => (None, None),
        },
    };
    fixed_offset_seconds(negative, hh, mm)
}

/// The last step of [`timezone_offset_seconds`]: the signed offset of parsed
/// `HH`/`MM` fields, or `0` (UTC) when either is missing or out of range
/// (`HH` in `0..=14`, `MM` in `0..=59`). Every result is within ±14h59m
/// (±53,940 s).
// Aeneas has no model of `RangeInclusive::contains`; the explicit comparisons
// are the same test.
#[allow(clippy::manual_range_contains)]
pub(crate) fn fixed_offset_seconds(negative: bool, hh: Option<i64>, mm: Option<i64>) -> i64 {
    let (h, m) = match (hh, mm) {
        (Some(h), Some(m)) => (h, m),
        _ => return 0,
    };
    if h < 0 || h > 14 || m < 0 || m > 59 {
        return 0;
    }
    let magnitude = h * 3600 + m * 60;
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// The `weekofyear(time, timezone)` builtin (function and `weekofyear`
/// variable forms) after the timezone is resolved to `tz_offset_seconds`: the
/// ISO 8601 week (1..=53) of the local civil date of `time`, or `None` (the
/// VM's `na`) when the date's ISO Thursday leaves the i64 epoch-ms range.
pub(crate) fn week_of_year_at(time: i64, tz_offset_seconds: i64) -> Option<i64> {
    let (year, month, day, _, _, _, _) = civil_from_epoch_ms(time, tz_offset_seconds);
    iso_week_of_year(year, month, day)
}

/// The `timestamp(...)` builtin after its six `int` fields (`year, month,
/// day, hour, minute, second`) are read: the epoch-ms time in the fixed-offset
/// timezone `tz_offset_seconds`, or `None` (the VM's `na`) when an
/// intermediate of the calendar arithmetic leaves i64. `fields` has length 6
/// (the caller passes a `[i64; 6]`; a slice because Aeneas does not translate
/// constant indexing into a borrowed array).
pub(crate) fn timestamp_at(fields: &[i64], tz_offset_seconds: i64) -> Option<i64> {
    epoch_ms_from_civil(
        fields[0],
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        tz_offset_seconds,
    )
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TimeSessionPeriod {
    start_minute: i64,
    end_minute: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct TimeSession {
    periods: Vec<TimeSessionPeriod>,
    days: [bool; 8],
}

/// The shape of a feed's declared session (`syminfo.session_hours`), or
/// `None` when it is not a session specification: the shared
/// [`pinewasm_tz::session_shape`], which emitted artifacts run too.
pub(crate) fn session_shape(hours: &str) -> Option<pinewasm_tz::SessionShape> {
    pinewasm_tz::session_shape(hours)
}

impl TimeSession {
    pub(crate) fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.eq_ignore_ascii_case("24x7") {
            return Some(Self::full_week());
        }
        let (periods_src, days_src) = input.split_once(':').unwrap_or((input, "1234567"));
        if periods_src.is_empty() || days_src.is_empty() {
            return None;
        }
        let mut days = [false; 8];
        for ch in days_src.chars() {
            let d = ch.to_digit(10)?;
            if !(1..=7).contains(&d) {
                return None;
            }
            days[d as usize] = true;
        }
        if !days.iter().any(|d| *d) {
            return None;
        }
        let mut periods = Vec::new();
        for part in periods_src.split(',') {
            let (start, end) = part.split_once('-')?;
            periods.push(TimeSessionPeriod {
                start_minute: parse_session_hhmm(start)?,
                end_minute: parse_session_hhmm(end)?,
            });
        }
        if periods.is_empty() {
            return None;
        }
        Some(Self { periods, days })
    }

    fn full_week() -> Self {
        let mut days = [false; 8];
        days[1..].fill(true);
        Self {
            periods: vec![TimeSessionPeriod {
                start_minute: 0,
                end_minute: 0,
            }],
            days,
        }
    }

    pub(crate) fn is_full_week(&self) -> bool {
        self.periods.len() == 1
            && self.periods[0].start_minute == 0
            && self.periods[0].end_minute == 0
            && self.days[1..].iter().all(|d| *d)
    }

    pub(crate) fn close_for_open(&self, open_ms: i64, tz_offset_seconds: i64) -> Option<i64> {
        const SECONDS_PER_DAY: i64 = 86_400;
        let local_seconds = open_ms.div_euclid(1000).saturating_add(tz_offset_seconds);
        let local_day = local_seconds.div_euclid(SECONDS_PER_DAY);
        let seconds_of_day = local_seconds.rem_euclid(SECONDS_PER_DAY);
        let minute_of_day = seconds_of_day / 60;

        for period in &self.periods {
            if let Some(close_local_ms) =
                self.period_close_local_ms(*period, local_day, minute_of_day)
            {
                return Some(close_local_ms.saturating_sub(tz_offset_seconds.saturating_mul(1000)));
            }
        }
        None
    }

    fn period_close_local_ms(
        &self,
        period: TimeSessionPeriod,
        local_day: i64,
        minute_of_day: i64,
    ) -> Option<i64> {
        let current_day = pine_dayofweek_from_days(local_day) as usize;
        let next_day = if current_day == 7 { 1 } else { current_day + 1 };
        let start = period.start_minute;
        let end = period.end_minute;
        let close_local_minute = match start.cmp(&end) {
            std::cmp::Ordering::Less => {
                if minute_of_day >= start && minute_of_day < end && self.days[current_day] {
                    local_day.saturating_mul(1440).saturating_add(end)
                } else {
                    return None;
                }
            }
            std::cmp::Ordering::Greater => {
                if minute_of_day >= start {
                    if self.days[next_day] {
                        local_day
                            .saturating_add(1)
                            .saturating_mul(1440)
                            .saturating_add(end)
                    } else {
                        return None;
                    }
                } else if minute_of_day < end {
                    if self.days[current_day] {
                        local_day.saturating_mul(1440).saturating_add(end)
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            std::cmp::Ordering::Equal => {
                if start == 0 {
                    if self.days[current_day] {
                        local_day.saturating_add(1).saturating_mul(1440)
                    } else {
                        return None;
                    }
                } else if minute_of_day >= start {
                    if self.days[next_day] {
                        local_day
                            .saturating_add(1)
                            .saturating_mul(1440)
                            .saturating_add(end)
                    } else {
                        return None;
                    }
                } else if self.days[current_day] {
                    local_day.saturating_mul(1440).saturating_add(end)
                } else {
                    return None;
                }
            }
        };
        Some(close_local_minute.saturating_mul(60_000))
    }
}

pub(crate) fn parse_session_hhmm(input: &str) -> Option<i64> {
    if input.len() != 4 || !input.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hour = input[..2].parse::<i64>().ok()?;
    let minute = input[2..].parse::<i64>().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    Some(hour * 60 + minute)
}

pub(crate) fn pine_dayofweek_from_days(days: i64) -> i64 {
    let dow0 = (days.rem_euclid(7) + 4) % 7;
    dow0 + 1
}

/// Civil (calendar) breakdown of an epoch-millisecond timestamp shifted into a
/// fixed-offset timezone. Uses the standard `civil_from_days` algorithm
/// (proleptic Gregorian, valid for all years). Fields, in order:
/// `(year, month 1..=12, day 1..=31, hour 0..=23, minute, second,
/// dayofweek 1..=7 with 1 = Sunday)`.
pub(crate) fn civil_from_epoch_ms(
    epoch_ms: i64,
    tz_offset_seconds: i64,
) -> (i64, i64, i64, i64, i64, i64, i64) {
    // Shift into local time, then split into whole days and intra-day seconds.
    let local_secs = epoch_ms.div_euclid(1000) + tz_offset_seconds;
    let days = local_secs.div_euclid(86_400);
    let secs_of_day = local_secs.rem_euclid(86_400);

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // civil_from_days: days since 1970-01-01 -> (y, m, d).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    // Day of week: 1970-01-01 was a Thursday; this runtime numbers Sunday as 1.
    // `weekday_from_days`: (days + 4) mod 7 gives 0 = Sunday.
    let dow0 = (days.rem_euclid(7) + 4) % 7; // 0 = Sunday
    let dayofweek = dow0 + 1; // 1 = Sunday .. 7 = Saturday

    (year, m, d, hour, minute, second, dayofweek)
}

/// The ISO 8601 week-of-year (1..=53) for a civil date. ISO week 1 is the week
/// (Monday-based) containing the year's first Thursday, equivalently the week
/// containing January 4. The `weekofyear` builtin uses this value. Pure integer
/// date math.
///
/// Total: every intermediate that can leave `i64` is checked, and the function
/// returns `None` instead of panicking (debug) or wrapping (release). For a
/// valid civil date that happens exactly when the date's ISO Thursday lies more
/// than 106,751,991,167 days from 1970-01-01, so that `thursday * 86_400_000`
/// (the Thursday's epoch-ms) leaves `i64`: dates after 292278994-08-17 or
/// before -292275055-05-17. Callers map `None` to `na`. Whenever no
/// intermediate overflows, the result equals the plain unchecked computation. `month` is not normalized: callers pass civil dates with
/// `month` in `1..=12`.
// `let ... else` instead of `?`: Aeneas's model of `?` on `Option` (the
// `Try`/`FromResidual` impls) is unverified, so the item avoids it.
#[allow(clippy::question_mark)]
pub(crate) fn iso_week_of_year(year: i64, month: i64, day: i64) -> Option<i64> {
    let Some(days) = days_from_civil(year, month, day) else {
        return None;
    };
    let Some(weekday) = iso_weekday(days) else {
        return None;
    };
    // The Thursday of the current ISO week determines the ISO year and week: a
    // week belongs to the year containing its Thursday. `weekday - 4` is in
    // `-3..=3`.
    let Some(thursday) = days.checked_sub(weekday - 4) else {
        return None;
    };
    let Some(thursday_ms) = thursday.checked_mul(86_400_000) else {
        return None;
    };
    // `civil_from_epoch_ms` cannot overflow with a zero offset.
    let (ty, _, _, _, _, _, _) = civil_from_epoch_ms(thursday_ms, 0);
    let Some(jan1) = days_from_civil(ty, 1, 1) else {
        return None;
    };
    let Some(since_jan1) = thursday.checked_sub(jan1) else {
        return None;
    };
    Some(since_jan1 / 7 + 1)
}

/// Days since the Gregorian epoch (1970-01-01) via the inverse of the civil
/// algorithm in [`civil_from_epoch_ms`], or `None` if an intermediate leaves
/// `i64`. Expects `m` in `1..=12` (other months are not normalized); used by
/// [`iso_week_of_year`].
#[allow(clippy::question_mark)]
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let y = if m <= 2 { y.checked_sub(1) } else { Some(y) };
    let Some(y) = y else {
        return None;
    };
    let era = y.div_euclid(400);
    let Some(era_years) = era.checked_mul(400) else {
        return None;
    };
    let Some(yoe) = y.checked_sub(era_years) else {
        return None;
    };
    // `m - 3` (for `m > 2`) and `m + 9` (for `m <= 2`) cannot overflow.
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let Some(mp153) = mp.checked_mul(153) else {
        return None;
    };
    let Some(mp153) = mp153.checked_add(2) else {
        return None;
    };
    let Some(doy) = (mp153 / 5).checked_add(d) else {
        return None;
    };
    let Some(doy) = doy.checked_sub(1) else {
        return None;
    };
    // `yoe` is in `0..400`, so the year-of-era terms cannot overflow.
    let Some(doe) = (yoe * 365 + yoe / 4 - yoe / 100).checked_add(doy) else {
        return None;
    };
    let Some(era_days) = era.checked_mul(146_097) else {
        return None;
    };
    let Some(days) = era_days.checked_add(doe) else {
        return None;
    };
    days.checked_sub(719_468)
}

/// ISO weekday: Monday = 1 .. Sunday = 7. 1970-01-01 was a Thursday (=4).
/// `None` only when `days + 3` leaves `i64`.
#[allow(clippy::question_mark)]
fn iso_weekday(days: i64) -> Option<i64> {
    let Some(shifted) = days.checked_add(3) else {
        return None;
    };
    Some(shifted.rem_euclid(7) + 1)
}

/// Format an epoch-ms `time` into a date/time string against a `format`
/// pattern in the given fixed-offset timezone (`tz_offset_seconds` east of
/// UTC). Supported tokens are: `yy` for
/// the last two digits of the year and a four-digit year for any other `y`
/// run; one- or two-width `M`/`d`/`H`/`h`/`m`/`s`, where `h` is the 12-hour
/// hour (1 to 12); `a` for `AM`/`PM`; an `S` run for the milliseconds,
/// zero-padded to the run's length; and the `+HHmm` offset for any `Z` run.
/// Single-quoted spans are literal text and `''` is an apostrophe. Other
/// field-letter runs pass through literally. IANA zone names and DST are not
/// handled.
pub(crate) fn format_time_string(epoch_ms: i64, format: &str, tz_offset_seconds: i64) -> String {
    let (year, month, day, hour, minute, second, _dow) =
        civil_from_epoch_ms(epoch_ms, tz_offset_seconds);
    // Whole-second offsets never change the millisecond.
    let millisecond = epoch_ms.rem_euclid(1000);
    let hour12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    let mut out = String::with_capacity(format.len());
    let bytes: Vec<char> = format.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        // Single-quoted literal span. `''` (empty or doubled) is one apostrophe.
        if c == '\'' {
            // Doubled quote -> a literal apostrophe.
            if i + 1 < bytes.len() && bytes[i + 1] == '\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i] != '\'' {
                out.push(bytes[i]);
                i += 1;
            }
            // Skip the closing quote (if present; an unterminated span ends).
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        // Count a run of the same field letter.
        if c.is_ascii_alphabetic() {
            let mut n = 1usize;
            while i + n < bytes.len() && bytes[i + n] == c {
                n += 1;
            }
            let run: String = std::iter::repeat_n(c, n).collect();
            let replaced = match (c, n) {
                ('y', 2) => Some(format!("{:02}", year.rem_euclid(100))),
                ('y', _) => Some(format!("{year:04}")),
                ('M', 2) => Some(format!("{month:02}")),
                ('M', 1) => Some(format!("{month}")),
                ('d', 2) => Some(format!("{day:02}")),
                ('d', 1) => Some(format!("{day}")),
                ('H', 2) => Some(format!("{hour:02}")),
                ('H', 1) => Some(format!("{hour}")),
                ('h', 2) => Some(format!("{hour12:02}")),
                ('h', 1) => Some(format!("{hour12}")),
                ('a', _) => Some(if hour < 12 { "AM" } else { "PM" }.to_string()),
                ('S', n) => Some(format!("{millisecond:0n$}")),
                ('m', 2) => Some(format!("{minute:02}")),
                ('m', 1) => Some(format!("{minute}")),
                ('s', 2) => Some(format!("{second:02}")),
                ('s', 1) => Some(format!("{second}")),
                ('Z', _) => {
                    let sign = if tz_offset_seconds < 0 { '-' } else { '+' };
                    let magnitude = tz_offset_seconds.unsigned_abs();
                    Some(format!(
                        "{sign}{:02}{:02}",
                        magnitude / 3600,
                        magnitude % 3600 / 60
                    ))
                }
                _ => None,
            };
            match replaced {
                Some(s) => out.push_str(&s),
                // Unsupported field letter: pass through literally.
                None => out.push_str(&run),
            }
            i += n;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The UNIX time in **milliseconds** of a calendar date/time expressed in a
/// fixed-offset timezone (`tz_offset_seconds` east of UTC). Uses the standard
/// `days_from_civil` algorithm (proleptic Gregorian, the exact
/// inverse of the `civil_from_days` in [`civil_from_epoch_ms`]), valid for all
/// years. Out-of-range month and day values carry into the adjacent unit (e.g.
/// month 13 rolls into the next year).
///
/// Total: every intermediate that can leave `i64` is checked, from the month
/// normalization through the day count to the final millisecond product. The
/// function returns `None` as soon as any of them overflows (the caller maps
/// that to `na`), in debug and release alike; it never panics. Whenever no
/// intermediate overflows, the result equals the plain unchecked computation.
// `let ... else` instead of `?`: Aeneas's model of `?` on `Option` (the
// `Try`/`FromResidual` impls) is unverified, so the item avoids it.
#[allow(clippy::question_mark)]
pub(crate) fn epoch_ms_from_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    tz_offset_seconds: i64,
) -> Option<i64> {
    // days_from_civil: (y, m, d) -> days since 1970-01-01. Normalize the month
    // into 1..=12 with a year carry so out-of-range months roll over.
    let Some(m0) = month.checked_sub(1) else {
        return None;
    };
    let Some(year) = year.checked_add(m0.div_euclid(12)) else {
        return None;
    };
    // From here on `month` is in `1..=12`, `mp` in `0..=11` and `yoe` in
    // `0..400`, so the plain operations on them cannot overflow.
    let month = m0.rem_euclid(12) + 1;
    let y = if month <= 2 {
        year.checked_sub(1)
    } else {
        Some(year)
    };
    let Some(y) = y else {
        return None;
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let Some(doy) = ((153 * mp + 2) / 5).checked_add(day) else {
        return None;
    };
    let Some(doy) = doy.checked_sub(1) else {
        return None;
    };
    let Some(doe) = (yoe * 365 + yoe / 4 - yoe / 100).checked_add(doy) else {
        return None;
    };
    let Some(era_days) = era.checked_mul(146_097) else {
        return None;
    };
    let Some(days) = era_days.checked_add(doe) else {
        return None;
    };
    let Some(days) = days.checked_sub(719_468) else {
        return None;
    };

    // The overflow checks run in the same order as a `checked_*` method chain;
    // they are spelled with `let ... else` so the item stays within the
    // Aeneas-extractable subset (no `?` on `Option`).
    let Some(day_secs) = days.checked_mul(86_400) else {
        return None;
    };
    let Some(hour_secs) = hour.checked_mul(3600) else {
        return None;
    };
    let Some(secs) = day_secs.checked_add(hour_secs) else {
        return None;
    };
    let Some(minute_secs) = minute.checked_mul(60) else {
        return None;
    };
    let Some(secs) = secs.checked_add(minute_secs) else {
        return None;
    };
    let Some(secs) = secs.checked_add(second) else {
        return None;
    };
    // Fields are wall-clock in the local zone; subtract the offset to get UTC.
    let Some(secs) = secs.checked_sub(tz_offset_seconds) else {
        return None;
    };
    secs.checked_mul(1000)
}

/// Plain unchecked implementations of the calendar helpers. The tests compare
/// the checked versions above against them.
#[cfg(test)]
mod reference {
    pub(super) fn iso_week_of_year(year: i64, month: i64, day: i64) -> i64 {
        // Days since the Gregorian epoch (1970-01-01) via the inverse of the civil
        // algorithm in `civil_from_epoch_ms`.
        fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
            let y = if m <= 2 { y - 1 } else { y };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let mp = if m > 2 { m - 3 } else { m + 9 };
            let doy = (153 * mp + 2) / 5 + d - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            era * 146_097 + doe - 719_468
        }
        // ISO weekday: Monday = 1 .. Sunday = 7. 1970-01-01 was a Thursday (=4).
        fn iso_weekday(days: i64) -> i64 {
            (days + 3).rem_euclid(7) + 1
        }
        let days = days_from_civil(year, month, day);
        // The Thursday of the current ISO week determines the ISO year and week: a
        // week belongs to the year containing its Thursday.
        let thursday = days - (iso_weekday(days) - 4);
        let (ty, _, _, _, _, _, _) = super::civil_from_epoch_ms(thursday * 86_400_000, 0);
        let jan1 = days_from_civil(ty, 1, 1);
        (thursday - jan1) / 7 + 1
    }

    pub(super) fn epoch_ms_from_civil(
        year: i64,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
        second: i64,
        tz_offset_seconds: i64,
    ) -> Option<i64> {
        let m0 = month - 1;
        let year = year + m0.div_euclid(12);
        let month = m0.rem_euclid(12) + 1;
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = if month > 2 { month - 3 } else { month + 9 };
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;

        let secs = days
            .checked_mul(86_400)?
            .checked_add(hour.checked_mul(3600)?)?
            .checked_add(minute.checked_mul(60)?)?
            .checked_add(second)?
            .checked_sub(tz_offset_seconds)?;
        secs.checked_mul(1000)
    }
}

/// Tests comparing the checked calendar helpers with the plain unchecked ones.
#[cfg(test)]
mod calendar_refactor_equivalence {
    use super::{epoch_ms_from_civil, iso_week_of_year, reference};

    /// Deterministic 64-bit LCG (MMIX multiplier and increment).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            let span = u64::try_from(hi - lo + 1).expect("non-empty range");
            lo + i64::try_from(self.next() % span).expect("span fits")
        }
        fn any_i64(&mut self) -> i64 {
            i64::from_ne_bytes(self.next().to_ne_bytes())
        }
    }

    #[test]
    fn iso_week_of_year_matches_reference_on_edges_and_sweep() {
        for year in [
            -4000, -401, -400, -1, 0, 1, 1582, 1899, 1900, 1969, 1970, 2000, 2004, 2020, 2021,
            2100, 2400, 9999,
        ] {
            for month in -3..=15 {
                for day in -40..=70 {
                    assert_eq!(
                        iso_week_of_year(year, month, day),
                        Some(reference::iso_week_of_year(year, month, day)),
                        "({year}, {month}, {day})"
                    );
                }
            }
        }
        // Every real day across a 1200-year window (three full 400-year eras).
        for year in 1600..=2800 {
            for month in 1..=12 {
                for day in 1..=31 {
                    assert_eq!(
                        iso_week_of_year(year, month, day),
                        Some(reference::iso_week_of_year(year, month, day)),
                        "({year}, {month}, {day})"
                    );
                }
            }
        }
        let mut rng = Lcg(0x5EED_CA1E_0001);
        for _ in 0..200_000 {
            let (y, m, d) = (
                rng.range(-1_000_000, 1_000_000),
                rng.range(-100, 100),
                rng.range(-10_000, 10_000),
            );
            assert_eq!(
                iso_week_of_year(y, m, d),
                Some(reference::iso_week_of_year(y, m, d)),
                "({y}, {m}, {d})"
            );
        }
    }

    #[test]
    fn epoch_ms_from_civil_matches_reference_on_edges_and_sweep() {
        let edge = [
            i64::MIN,
            i64::MIN + 1,
            -86_400,
            -3600,
            -60,
            -1,
            0,
            1,
            59,
            60,
            3600,
            86_400,
            i64::MAX - 1,
            i64::MAX,
        ];
        // Checked tail: extreme hour/minute/second/offset values, sane dates.
        for &h in &edge {
            for &mi in &edge {
                for &s in &edge {
                    for &tz in &edge {
                        assert_eq!(
                            epoch_ms_from_civil(2024, 2, 29, h, mi, s, tz),
                            reference::epoch_ms_from_civil(2024, 2, 29, h, mi, s, tz),
                            "(2024-02-29 {h}:{mi}:{s} tz {tz})"
                        );
                    }
                }
            }
        }
        let mut rng = Lcg(0x5EED_CA1E_0002);
        for i in 0..300_000 {
            // Dates wide enough to overflow `days * 86_400` sometimes, but
            // small enough that the unchecked date arithmetic cannot overflow.
            let year = rng.range(-1_000_000_000_000, 1_000_000_000_000);
            let month = rng.range(-1_000_000, 1_000_000);
            let day = rng.range(-1_000_000_000_000_000, 1_000_000_000_000_000);
            let (hour, minute, second, tz) = if i % 2 == 0 {
                (rng.any_i64(), rng.any_i64(), rng.any_i64(), rng.any_i64())
            } else {
                (
                    rng.range(-48, 48),
                    rng.range(-120, 120),
                    rng.range(-120, 120),
                    rng.range(-86_400, 86_400),
                )
            };
            assert_eq!(
                epoch_ms_from_civil(year, month, day, hour, minute, second, tz),
                reference::epoch_ms_from_civil(year, month, day, hour, minute, second, tz),
                "({year}, {month}, {day}, {hour}, {minute}, {second}, {tz})"
            );
        }
        for year in 1600..=2800 {
            for month in -2..=14 {
                for day in [-1, 0, 1, 28, 29, 30, 31, 32] {
                    assert_eq!(
                        epoch_ms_from_civil(year, month, day, 23, 59, 59, -18_000),
                        reference::epoch_ms_from_civil(year, month, day, 23, 59, 59, -18_000),
                        "({year}, {month}, {day})"
                    );
                }
            }
        }
    }

    #[test]
    fn iso_week_of_year_is_none_exactly_where_reference_panics() {
        // The checked version returns `None` exactly where the unchecked
        // implementation overflows (debug builds panic there).
        let checks_on =
            std::panic::catch_unwind(|| std::hint::black_box(i64::MAX) + std::hint::black_box(1))
                .is_err();
        for (y, m, d) in [
            (i64::MAX / 400, 6, 15),
            (1970, 1, i64::MAX / 2),
            (1970, 1, 106_751_991_167_300),
            (1970, 1, 106_751_991_167_301),
            (-1970, 1, -106_751_991_167_301),
        ] {
            let new = iso_week_of_year(y, m, d);
            let old = std::panic::catch_unwind(|| reference::iso_week_of_year(y, m, d)).ok();
            // Release builds wrap instead of panicking; there the old value is
            // only meaningful where the new one is `Some`.
            if checks_on || new.is_some() {
                assert_eq!(new, old, "({y}, {m}, {d})");
            }
        }
    }
}

/// Tests for the calendar helpers' overflow behavior.
#[cfg(test)]
mod aeneas_findings {
    use super::*;
    use std::hint::black_box;
    use std::panic::catch_unwind;

    /// Whether this build panics on `i64` overflow (debug / test profile) or wraps.
    fn overflow_checks_on() -> bool {
        catch_unwind(|| black_box(i64::MAX) + black_box(1)).is_err()
    }

    /// The plain unchecked computation is exact only while the ISO Thursday satisfies
    /// `|thursday| <= 106_751_991_167` (so that `thursday * 86_400_000` fits in i64).
    /// The checked version returns `None` past the limit in debug and release
    /// builds alike.
    #[test]
    fn iso_week_of_year_is_none_past_the_thursday_limit() {
        // Last admitted date on each side.
        assert!(iso_week_of_year(292_278_994, 8, 17).is_some());
        assert!(iso_week_of_year(-292_275_055, 5, 17).is_some());
        for (y, m, d) in [
            (292_278_994, 8, 18),
            (-292_275_055, 5, 16),
            (300_000_000, 1, 1),
            (-300_000_000, 1, 1),
            (i64::MAX, 12, 31),
            (i64::MIN, 1, 1),
            (1970, 1, i64::MAX),
            (1970, 1, i64::MIN),
            (1970, i64::MAX, 1),
            (1970, i64::MIN, 1),
        ] {
            assert_eq!(iso_week_of_year(y, m, d), None, "({y}, {m}, {d})");
        }
    }

    /// The `weekofyear` path reaches this limit: the civil date of the bar time
    /// `i64::MIN` (offset 0) is -292275055-05-16, whose ISO Thursday is past the limit. The
    /// checked function returns `None` there (the VM maps it to `na`).
    #[test]
    fn weekofyear_of_min_epoch_is_none() {
        let (y, m, d, ..) = civil_from_epoch_ms(i64::MIN, 0);
        assert_eq!((y, m, d), (-292_275_055, 5, 16));
        assert_eq!(iso_week_of_year(y, m, d), None);
        // The `i64::MAX` side stays in range (its Thursday is 3 days earlier).
        let (y, m, d, ..) = civil_from_epoch_ms(i64::MAX, 0);
        assert_eq!((y, m, d), (292_278_994, 8, 17));
        assert!(matches!(iso_week_of_year(y, m, d), Some(1..=53)));
        // Every exchange offset the VM can resolve, at both ends of the bar-time range.
        for tz in [-53_940, -3600, 0, 3600, 53_940] {
            for ms in [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX] {
                let (y, m, d, ..) = civil_from_epoch_ms(ms, tz);
                if let Some(w) = iso_week_of_year(y, m, d) {
                    assert!((1..=53).contains(&w), "{ms} {tz}: {w}");
                }
            }
        }
    }

    /// Script ints passed to `timestamp(...)` reach the day-count arithmetic
    /// (`month - 1`, the year carry, the day count, `era * 146_097`). Plain
    /// arithmetic on them panics in debug builds (e.g. month `i64::MIN`), so every
    /// intermediate is checked and yields `None`.
    #[test]
    fn epoch_ms_from_civil_is_none_on_extreme_fields() {
        for (y, mo, d) in [
            (2020, i64::MIN, 1),
            (i64::MAX, 12, 1),
            (i64::MIN, 1, 1),
            (i64::MIN, 3, 1),
            (2020, 1, i64::MAX),
            (2020, 1, i64::MIN),
            (2020, i64::MAX, 1),
            (i64::MAX, i64::MAX, i64::MAX),
            (i64::MIN, i64::MIN, i64::MIN),
        ] {
            assert_eq!(
                epoch_ms_from_civil(y, mo, d, 0, 0, 0, 0),
                None,
                "({y}, {mo}, {d})"
            );
        }
        // The last representable instant round-trips; one second more is `None`.
        let (y, mo, d, h, mi, s, _) = civil_from_epoch_ms(i64::MAX - 807, 0);
        assert_eq!(
            epoch_ms_from_civil(y, mo, d, h, mi, s, 0),
            Some(i64::MAX - 807)
        );
        assert_eq!(epoch_ms_from_civil(y, mo, d, h, mi, s + 1, 0), None);
    }

    /// The function fails exactly when `epoch_ms.div_euclid(1000) + tz_offset_seconds`
    /// leaves i64. Not reachable through `timezone_offset_seconds`, which is bounded
    /// by 14h59m.
    #[test]
    fn civil_from_epoch_ms_overflows_on_extreme_offset() {
        assert!(catch_unwind(|| civil_from_epoch_ms(0, i64::MAX)).is_ok());
        if overflow_checks_on() {
            assert!(catch_unwind(|| civil_from_epoch_ms(1000, i64::MAX)).is_err());
            assert!(catch_unwind(|| civil_from_epoch_ms(-1, i64::MIN)).is_err());
        }
        assert_eq!(timezone_offset_seconds("UTC+14:59"), 53_940);
        assert_eq!(timezone_offset_seconds("UTC-14:59"), -53_940);
    }
}

/// Comparison tests for the checked calendar helpers (total `iso_week_of_year`,
/// fully checked `epoch_ms_from_civil`). `pre_fix` holds plain unchecked copies
/// of the two functions; `oracle` re-runs the same arithmetic, operation for
/// operation, in `i128` and reports whether every intermediate fits in `i64`
/// (i.e. whether the unchecked code avoids overflow). The checked helpers must
/// agree with the unchecked code wherever that code does not overflow, and
/// return `None` everywhere else.
#[cfg(test)]
mod calendar_fix_differential {
    use super::{civil_from_epoch_ms, epoch_ms_from_civil, iso_week_of_year};
    use std::hint::black_box;
    use std::panic::catch_unwind;

    /// Plain unchecked copies of the two functions, without their doc comments.
    mod pre_fix {
        use super::super::civil_from_epoch_ms;

        pub(super) fn iso_week_of_year(year: i64, month: i64, day: i64) -> i64 {
            let days = days_from_civil(year, month, day);
            let thursday = days - (iso_weekday(days) - 4);
            let (ty, _, _, _, _, _, _) = civil_from_epoch_ms(thursday * 86_400_000, 0);
            let jan1 = days_from_civil(ty, 1, 1);
            (thursday - jan1) / 7 + 1
        }

        fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
            let y = if m <= 2 { y - 1 } else { y };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let mp = if m > 2 { m - 3 } else { m + 9 };
            let doy = (153 * mp + 2) / 5 + d - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            era * 146_097 + doe - 719_468
        }

        fn iso_weekday(days: i64) -> i64 {
            (days + 3).rem_euclid(7) + 1
        }

        #[allow(clippy::question_mark)]
        pub(super) fn epoch_ms_from_civil(
            year: i64,
            month: i64,
            day: i64,
            hour: i64,
            minute: i64,
            second: i64,
            tz_offset_seconds: i64,
        ) -> Option<i64> {
            let m0 = month - 1;
            let year = year + m0.div_euclid(12);
            let month = m0.rem_euclid(12) + 1;
            let y = if month <= 2 { year - 1 } else { year };
            let era = y.div_euclid(400);
            let yoe = y.rem_euclid(400);
            let mp = if month > 2 { month - 3 } else { month + 9 };
            let doy = (153 * mp + 2) / 5 + day - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            let days = era * 146_097 + doe - 719_468;
            let Some(day_secs) = days.checked_mul(86_400) else {
                return None;
            };
            let Some(hour_secs) = hour.checked_mul(3600) else {
                return None;
            };
            let Some(secs) = day_secs.checked_add(hour_secs) else {
                return None;
            };
            let Some(minute_secs) = minute.checked_mul(60) else {
                return None;
            };
            let Some(secs) = secs.checked_add(minute_secs) else {
                return None;
            };
            let Some(secs) = secs.checked_add(second) else {
                return None;
            };
            let Some(secs) = secs.checked_sub(tz_offset_seconds) else {
                return None;
            };
            secs.checked_mul(1000)
        }
    }

    /// The unchecked arithmetic in `i128`, with every intermediate range-checked
    /// against `i64`. Operands here stay far below `2^100`, so `i128` itself
    /// never overflows, and on `i64`-sized operands `/`, `div_euclid` and
    /// `rem_euclid` agree between the two widths.
    mod oracle {
        use super::super::civil_from_epoch_ms;

        /// Tracks whether every intermediate so far fits in `i64`.
        struct Fit(std::cell::Cell<bool>);
        impl Fit {
            fn new() -> Self {
                Fit(std::cell::Cell::new(true))
            }
            fn v(&self, x: i128) -> i128 {
                if i64::try_from(x).is_err() {
                    self.0.set(false);
                }
                x
            }
            fn ok(&self) -> bool {
                self.0.get()
            }
        }

        fn days_from_civil(f: &Fit, y: i128, m: i128, d: i128) -> i128 {
            let y = if m <= 2 { f.v(y - 1) } else { y };
            let era = y.div_euclid(400);
            let yoe = f.v(y - f.v(era * 400));
            let mp = if m > 2 { f.v(m - 3) } else { f.v(m + 9) };
            let doy = f.v(f.v(f.v(f.v(153 * mp) + 2) / 5 + d) - 1);
            let doe = f.v(f.v(f.v(f.v(yoe * 365) + yoe / 4) - yoe / 100) + doy);
            f.v(f.v(f.v(era * 146_097) + doe) - 719_468)
        }

        /// `Some(week)` iff the unchecked `iso_week_of_year` does not overflow.
        pub(super) fn iso_week_of_year(year: i64, month: i64, day: i64) -> Option<i64> {
            let f = &Fit::new();
            let days = days_from_civil(f, year.into(), month.into(), day.into());
            let weekday = f.v(f.v(days + 3).rem_euclid(7) + 1);
            let thursday = f.v(days - f.v(weekday - 4));
            let thursday_ms = f.v(thursday * 86_400_000);
            if !f.ok() {
                return None;
            }
            let (ty, ..) = civil_from_epoch_ms(i64::try_from(thursday_ms).ok()?, 0);
            let jan1 = days_from_civil(f, ty.into(), 1, 1);
            let week = f.v(f.v(thursday - jan1) / 7 + 1);
            if f.ok() {
                i64::try_from(week).ok()
            } else {
                None
            }
        }

        /// `Some(ms)` iff no intermediate of the unchecked `epoch_ms_from_civil`
        /// (unchecked prefix or checked tail) leaves `i64`.
        pub(super) fn epoch_ms_from_civil(
            year: i64,
            month: i64,
            day: i64,
            hour: i64,
            minute: i64,
            second: i64,
            tz: i64,
        ) -> Option<i64> {
            let f = &Fit::new();
            let m0 = f.v(i128::from(month) - 1);
            let year = f.v(i128::from(year) + m0.div_euclid(12));
            let month = f.v(m0.rem_euclid(12) + 1);
            let y = if month <= 2 { f.v(year - 1) } else { year };
            let era = y.div_euclid(400);
            let yoe = y.rem_euclid(400);
            let mp = if month > 2 {
                f.v(month - 3)
            } else {
                f.v(month + 9)
            };
            let doy = f.v(f.v(f.v(f.v(153 * mp) + 2) / 5 + i128::from(day)) - 1);
            let doe = f.v(f.v(f.v(f.v(yoe * 365) + yoe / 4) - yoe / 100) + doy);
            let days = f.v(f.v(f.v(era * 146_097) + doe) - 719_468);
            let secs = f.v(f.v(days * 86_400) + f.v(i128::from(hour) * 3600));
            let secs = f.v(secs + f.v(i128::from(minute) * 60));
            let secs = f.v(f.v(secs + i128::from(second)) - i128::from(tz));
            let ms = f.v(secs * 1000);
            if f.ok() {
                i64::try_from(ms).ok()
            } else {
                None
            }
        }
    }

    /// Deterministic 64-bit LCG (MMIX multiplier and increment).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            let span = u64::try_from(i128::from(hi) - i128::from(lo) + 1).expect("span");
            let off = i128::from(self.next() % span);
            i64::try_from(i128::from(lo) + off).expect("in range")
        }
        fn any_i64(&mut self) -> i64 {
            i64::from_ne_bytes(self.next().to_ne_bytes())
        }
        /// A value from a mix of scales: small, calendar-sized, near the
        /// overflow thresholds, and arbitrary.
        fn mixed(&mut self) -> i64 {
            match self.next() % 6 {
                0 => self.range(-100, 100),
                1 => self.range(-1_000_000, 1_000_000),
                2 => self.range(-400_000_000_000, 400_000_000_000),
                3 => self.range(i64::MIN, i64::MIN + 1_000_000),
                4 => self.range(i64::MAX - 1_000_000, i64::MAX),
                _ => self.any_i64(),
            }
        }
    }

    fn overflow_checks_on() -> bool {
        catch_unwind(|| black_box(i64::MAX) + black_box(1)).is_err()
    }

    /// Checks the comparison model itself: wherever it says the unchecked code does
    /// not overflow, the unchecked code (run for real) returns the model's value.
    #[test]
    fn oracle_matches_pre_fix_on_its_no_overflow_domain() {
        let mut rng = Lcg(0x0B5E_55ED_0001);
        let (mut iso_hits, mut ms_hits) = (0u32, 0u32);
        for i in 0..400_000 {
            let (y, m, d) = if i % 2 == 0 {
                (rng.mixed(), rng.mixed(), rng.mixed())
            } else {
                (
                    rng.range(-300_000_000, 300_000_000),
                    rng.range(-30, 30),
                    rng.range(-1_000_000, 1_000_000),
                )
            };
            if let Some(w) = oracle::iso_week_of_year(y, m, d) {
                assert_eq!(pre_fix::iso_week_of_year(y, m, d), w, "({y}, {m}, {d})");
                iso_hits += 1;
            }
            let (h, mi, s, tz) = (rng.mixed(), rng.mixed(), rng.mixed(), rng.mixed());
            let (h, mi, s, tz) = if i % 3 == 0 {
                (h, mi, s, tz)
            } else {
                (h % 48, mi % 120, s % 120, tz % 86_400)
            };
            if let Some(ms) = oracle::epoch_ms_from_civil(y, m, d, h, mi, s, tz) {
                assert_eq!(
                    pre_fix::epoch_ms_from_civil(y, m, d, h, mi, s, tz),
                    Some(ms),
                    "({y}, {m}, {d}, {h}, {mi}, {s}, {tz})"
                );
                ms_hits += 1;
            }
        }
        // Both domains are well exercised.
        assert!(iso_hits > 150_000, "{iso_hits}");
        assert!(ms_hits > 50_000, "{ms_hits}");
    }

    /// The checked helpers equal the comparison model everywhere: the unchecked
    /// value where the unchecked code does not overflow, and `None` where it does.
    #[test]
    fn fix_equals_pre_fix_off_the_overflow_class_and_none_on_it() {
        let mut rng = Lcg(0x0B5E_55ED_0002);
        let (mut iso_none, mut ms_none) = (0u32, 0u32);
        for i in 0..400_000 {
            let (y, m, d) = if i % 2 == 0 {
                (rng.mixed(), rng.mixed(), rng.mixed())
            } else {
                (
                    rng.range(-300_000_000, 300_000_000),
                    rng.range(-30, 30),
                    rng.range(-1_000_000, 1_000_000),
                )
            };
            let want = oracle::iso_week_of_year(y, m, d);
            iso_none += u32::from(want.is_none());
            assert_eq!(iso_week_of_year(y, m, d), want, "({y}, {m}, {d})");
            let (h, mi, s, tz) = (rng.mixed(), rng.mixed(), rng.mixed(), rng.mixed());
            let (h, mi, s, tz) = if i % 3 == 0 {
                (h, mi, s, tz)
            } else {
                (h % 48, mi % 120, s % 120, tz % 86_400)
            };
            let want = oracle::epoch_ms_from_civil(y, m, d, h, mi, s, tz);
            ms_none += u32::from(want.is_none());
            assert_eq!(
                epoch_ms_from_civil(y, m, d, h, mi, s, tz),
                want,
                "({y}, {m}, {d}, {h}, {mi}, {s}, {tz})"
            );
        }
        // The overflow class is well exercised too.
        assert!(iso_none > 50_000, "{iso_none}");
        assert!(ms_none > 50_000, "{ms_none}");
    }

    /// The script-reachable `weekofyear` path: any bar time and any offset the
    /// VM can resolve. Matches the unchecked code wherever that code is defined, and
    /// `None` (-> `na`) exactly on the dates whose ISO Thursday overflows.
    #[test]
    fn weekofyear_path_unchanged_except_at_the_epoch_extremes() {
        let mut rng = Lcg(0x0B5E_55ED_0003);
        let mut none = 0u32;
        let mut check = |ms: i64, tz: i64| {
            let (y, m, d, ..) = civil_from_epoch_ms(ms, tz);
            let new = iso_week_of_year(y, m, d);
            assert_eq!(new, oracle::iso_week_of_year(y, m, d), "{ms} {tz}");
            match new {
                Some(w) => {
                    assert!((1..=53).contains(&w), "{ms} {tz}: {w}");
                    assert_eq!(pre_fix::iso_week_of_year(y, m, d), w, "{ms} {tz}");
                }
                None => none += 1,
            }
        };
        for _ in 0..200_000 {
            let tz = rng.range(-53_940, 53_940);
            check(rng.any_i64(), tz);
            check(rng.range(i64::MIN, i64::MIN + 30 * 86_400_000), tz);
            check(rng.range(i64::MAX - 30 * 86_400_000, i64::MAX), tz);
            // 1900..2100, the realistic range.
            check(rng.range(-2_208_988_800_000, 4_102_444_800_000), tz);
        }
        // Only the few days at the `i64::MIN` end of the range are affected.
        assert!(none > 0);
        for tz in [-53_940, 0, 53_940] {
            let ms = i64::MIN + 10 * 86_400_000;
            let (y, m, d, ..) = civil_from_epoch_ms(ms, tz);
            assert!(iso_week_of_year(y, m, d).is_some(), "{tz}");
        }
    }

    /// Compares the checked and unchecked functions on extreme inputs, including
    /// the boundary dates: in debug builds the unchecked code panics exactly where
    /// the checked code returns `None`, and the two agree everywhere else.
    #[test]
    fn fix_is_none_exactly_where_pre_fix_panics() {
        if !overflow_checks_on() {
            return;
        }
        let edge = [i64::MIN, i64::MIN + 1, -1, 0, 1, 12, i64::MAX - 1, i64::MAX];
        for &y in &edge {
            for &m in &edge {
                for &d in &edge {
                    let old = catch_unwind(|| pre_fix::iso_week_of_year(y, m, d)).ok();
                    assert_eq!(iso_week_of_year(y, m, d), old, "({y}, {m}, {d})");
                    let old = catch_unwind(|| pre_fix::epoch_ms_from_civil(y, m, d, 0, 0, 0, 0))
                        .ok()
                        .flatten();
                    assert_eq!(
                        epoch_ms_from_civil(y, m, d, 0, 0, 0, 0),
                        old,
                        "({y}, {m}, {d})"
                    );
                }
            }
        }
        for (y, m, d) in [
            (292_278_994, 8, 17),
            (292_278_994, 8, 18),
            (-292_275_055, 5, 16),
            (-292_275_055, 5, 17),
            (2020, i64::MIN, 1),
        ] {
            let old = catch_unwind(|| pre_fix::iso_week_of_year(y, m, d)).ok();
            assert_eq!(iso_week_of_year(y, m, d), old, "({y}, {m}, {d})");
            let old = catch_unwind(|| pre_fix::epoch_ms_from_civil(y, m, d, 1, 2, 3, 0))
                .ok()
                .flatten();
            assert_eq!(
                epoch_ms_from_civil(y, m, d, 1, 2, 3, 0),
                old,
                "({y}, {m}, {d})"
            );
        }
    }
}

/// Comparison tests for the call-site kernels
/// (`fixed_offset_seconds`, `week_of_year_at`, `timestamp_at`) against plain
/// copies of their bodies.
#[cfg(test)]
mod p5_vmlib_differential {
    use super::{
        civil_from_epoch_ms, epoch_ms_from_civil, fixed_offset_seconds, iso_week_of_year,
        timestamp_at, timezone_offset_seconds, week_of_year_at,
    };

    /// Plain copy of `timezone_offset_seconds`, written without `fixed_offset_seconds`.
    fn timezone_offset_seconds_reference(tz: &str) -> i64 {
        let tz = tz.trim();
        if tz.is_empty() {
            return 0;
        }
        let rest = tz
            .strip_prefix("UTC")
            .or_else(|| tz.strip_prefix("GMT"))
            .unwrap_or(tz);
        if rest.is_empty() {
            return 0;
        }
        let (sign, digits) = match rest.as_bytes()[0] {
            b'+' => (1, &rest[1..]),
            b'-' => (-1, &rest[1..]),
            _ => return 0,
        };
        let (hh, mm) = match digits.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok(), m.parse::<i64>().ok()),
            None => match digits.len() {
                1 | 2 => (digits.parse::<i64>().ok(), Some(0)),
                4 if digits.is_char_boundary(2) => (
                    digits[..2].parse::<i64>().ok(),
                    digits[2..].parse::<i64>().ok(),
                ),
                _ => (None, None),
            },
        };
        match (hh, mm) {
            (Some(h), Some(m)) if (0..=14).contains(&h) && (0..=59).contains(&m) => {
                sign * (h * 3600 + m * 60)
            }
            _ => 0,
        }
    }

    /// Plain copy of the `weekofyear` call sites (series and function forms).
    fn week_of_year_reference(time: i64, off: i64) -> Option<i64> {
        let (year, month, day, _, _, _, _) = civil_from_epoch_ms(time, off);
        iso_week_of_year(year, month, day)
    }

    /// Plain copy of the `timestamp(...)` call.
    fn timestamp_reference(nums: &[i64; 6], off: i64) -> Option<i64> {
        epoch_ms_from_civil(nums[0], nums[1], nums[2], nums[3], nums[4], nums[5], off)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        /// An `i64` biased toward the edges and toward small values.
        fn i64(&mut self) -> i64 {
            match self.below(4) {
                0 => self.next() as i64,
                1 => [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX]
                    [self.below(7) as usize],
                2 => self.below(200) as i64 - 100,
                _ => self.below(1 << 40) as i64 - (1 << 39),
            }
        }
    }

    #[test]
    fn timezone_offset_matches_reference() {
        const ALPHABET: &[&str] = &[
            "U", "T", "C", "G", "M", "+", "-", ":", "0", "1", "2", "4", "5", "9", " ", "€", "/",
        ];
        let mut rng = Rng(0x7A0E);
        let mut nonzero = 0usize;
        for i in 0..200_000 {
            let mut tz = String::new();
            if i % 2 == 0 {
                tz.push_str(["UTC", "GMT", ""][rng.below(3) as usize]);
            }
            for _ in 0..rng.below(8) {
                tz.push_str(ALPHABET[rng.below(ALPHABET.len() as u64) as usize]);
            }
            let new = timezone_offset_seconds(&tz);
            assert_eq!(new, timezone_offset_seconds_reference(&tz), "{tz:?}");
            assert!(new.abs() <= 53_940);
            nonzero += usize::from(new != 0);
        }
        for tz in [
            "UTC+14:59",
            "UTC-14:59",
            "GMT+0530",
            "UTC-5",
            "UTC+15",
            "UTC+1:60",
        ] {
            assert_eq!(
                timezone_offset_seconds(tz),
                timezone_offset_seconds_reference(tz)
            );
        }
        assert!(nonzero > 1_000, "only {nonzero} non-zero offsets exercised");
    }

    #[test]
    fn fixed_offset_seconds_is_bounded_and_matches_reference_formula() {
        let mut rng = Rng(0xF1CE);
        for _ in 0..200_000 {
            let negative = rng.below(2) == 0;
            let pick = |rng: &mut Rng| match rng.below(3) {
                0 => None,
                1 => Some(rng.below(80) as i64 - 10),
                _ => Some(rng.i64()),
            };
            let (hh, mm) = (pick(&mut rng), pick(&mut rng));
            let sign = if negative { -1 } else { 1 };
            let expected = match (hh, mm) {
                (Some(h), Some(m)) if (0..=14).contains(&h) && (0..=59).contains(&m) => {
                    sign * (h * 3600 + m * 60)
                }
                _ => 0,
            };
            let got = fixed_offset_seconds(negative, hh, mm);
            assert_eq!(got, expected, "{negative} {hh:?} {mm:?}");
            assert!(got.abs() <= 53_940);
        }
    }

    #[test]
    fn calendar_call_sites_match_reference() {
        let mut rng = Rng(0xCA1E);
        let (mut weeks, mut nas, mut stamps) = (0usize, 0usize, 0usize);
        for _ in 0..200_000 {
            let time = rng.i64();
            let off = rng.below(2 * 53_940 + 1) as i64 - 53_940;
            let new = week_of_year_at(time, off);
            assert_eq!(new, week_of_year_reference(time, off), "{time} {off}");
            match new {
                Some(w) => {
                    assert!((1..=53).contains(&w));
                    weeks += 1;
                }
                None => nas += 1,
            }
            let nums = [
                rng.i64(),
                rng.i64(),
                rng.i64(),
                rng.i64(),
                rng.i64(),
                rng.i64(),
            ];
            let off = if rng.below(2) == 0 { 0 } else { off };
            let ts = timestamp_at(&nums, off);
            assert_eq!(ts, timestamp_reference(&nums, off), "{nums:?} {off}");
            stamps += usize::from(ts.is_some());
        }
        assert!(
            weeks > 1_000 && nas > 100 && stamps > 1_000,
            "{weeks} {nas} {stamps}"
        );
    }
}

#[cfg(test)]
mod session_shape_tests {
    use super::{session_shape, TimeSession};
    use pinewasm_tz::SessionShape as S;

    #[test]
    fn the_shared_parser_accepts_exactly_what_time_session_reads() {
        let mut texts: Vec<String> = [
            "24x7",
            "24X7",
            " 24x7 ",
            "0930-1600",
            "0930-1600:23456",
            "0000-0000",
            "0000-0000:1234567",
            "1700-1600",
            "1700-1700:2345671",
            "0900-1130,1230-1500",
            "0930-1600,1800-0200",
            "",
            " ",
            "0930-1600:",
            ":23456",
            "0930-1600:8",
            "0930-1600:0",
            "093-1600",
            "0930-2400",
            "0960-1600",
            "0930 -1600",
            "0930-1600,",
            "abcd-efgh",
            "0930",
            "24x5",
            "0930-1600:2345a",
            "١٢٣٤-1600",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        // Every HHMM pair on a coarse grid, with and without days.
        for start in (0..2400).step_by(130) {
            for end in (0..2400).step_by(170) {
                texts.push(format!("{start:04}-{end:04}"));
                texts.push(format!("{start:04}-{end:04}:2345"));
            }
        }
        for text in &texts {
            let shared = session_shape(text);
            let blank = text.trim().is_empty();
            assert_eq!(
                shared.is_some_and(|shape| shape != S::Absent),
                TimeSession::parse(text).is_some(),
                "{text:?}"
            );
            assert_eq!(shared == Some(S::Absent), blank, "{text:?}");
        }
        assert_eq!(session_shape("24x7"), Some(S::Continuous));
        assert_eq!(session_shape("0000-0000:23456"), Some(S::Continuous));
        assert_eq!(session_shape("0930-1600"), Some(S::WithinDay));
        assert_eq!(session_shape("0900-1130,1230-1500"), Some(S::WithinDay));
        assert_eq!(session_shape("1700-1600"), Some(S::Overnight));
        assert_eq!(session_shape("1700-1700"), Some(S::Overnight));
        assert_eq!(session_shape("0930-1600,1800-0200"), Some(S::Overnight));
    }
}
