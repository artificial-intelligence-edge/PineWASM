//! Time zone rules shared by the interpreter and every emitted artifact, so
//! both turn an instant into an exchange's local time the same way.
//!
//! A [`TimeZone`] is either a fixed offset, in one of the forms PineWASM
//! accepts (`"UTC"`, `"UTC-5"`, `"GMT+0530"`, `"+05:30"`), or the rules of
//! an IANA zone (`"America/New_York"`): every historical offset change, then
//! the zone's current daylight-saving rule. The rules are read from `TZif` data
//! (RFC 8536, version 2 or later): explicit transitions, then the POSIX `TZ`
//! string in the footer for every instant after the last one.
//!
//! With the `bundled` feature, [`resolve`] reads the IANA Time Zone Database
//! release [`TZDATA_VERSION`], embedded through the exactly pinned
//! `jiff-tzdb` crate. Without it, the crate is the fixed-offset and session
//! readers ([`parse_fixed`], [`session_shape`]) and the `TZif` reader
//! ([`Zone::from_tzif`]) over bytes its caller supplies; emitted runtimes
//! link it this way and carry no zone data.
//!
//! Nothing here allocates or panics; arithmetic that could overflow
//! saturates, so the crate can run inside a `panic = "abort"` artifact.
#![no_std]
#![forbid(unsafe_code)]

/// The IANA Time Zone Database release that [`resolve`] reads. A new release
/// is a change of the `jiff-tzdb` pin in this crate's manifest, and a test
/// keeps this constant equal to the release that crate reports.
pub const TZDATA_VERSION: &str = "2026e";

/// Seconds in a day.
const DAY: i64 = 86_400;

/// A time zone: a fixed offset, or the rules of an IANA zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeZone<'a> {
    /// A fixed offset in seconds east of UTC.
    Fixed(i32),
    /// The rules of an IANA zone.
    Rules(Zone<'a>),
}

impl TimeZone<'_> {
    /// The zone's offset from UTC, in seconds east, at `utc_seconds`.
    pub fn offset_at(&self, utc_seconds: i64) -> i32 {
        match self {
            TimeZone::Fixed(offset) => *offset,
            TimeZone::Rules(zone) => zone.offset_at(utc_seconds),
        }
    }

    /// The UTC instant of the local wall-clock time `local_seconds` (local
    /// seconds since 1970-01-01T00:00 on the zone's clock).
    ///
    /// A local time that occurs once has one answer. Where clocks go back and
    /// a local time occurs twice, this is the earlier instant, read with the
    /// offset in force before the change. Where clocks go forward and a local
    /// time never occurs, it is read with the offset in force before the
    /// change too, which lands after the gap: 02:30 on a night New York skips
    /// from 02:00 to 03:00 becomes 03:30. Both follow the usual convention for
    /// local date-times.
    pub fn utc_from_local(&self, local_seconds: i64) -> i64 {
        match self {
            TimeZone::Fixed(offset) => local_seconds.saturating_sub(i64::from(*offset)),
            TimeZone::Rules(zone) => zone.utc_from_local(local_seconds),
        }
    }
}

/// The offset of a fixed-offset timezone string, in seconds east of UTC, in
/// the forms PineWASM accepts: `"UTC"` or `"GMT"` alone (0), or optionally
/// prefixed by either, a sign and `H`, `HH`, `HH:MM` or `HHMM` (`"UTC-5"`,
/// `"GMT+0530"`, `"+05:30"`), with `HH` in `0..=14` and `MM` in `0..=59`.
/// Surrounding whitespace is ignored. `None` for anything else, an IANA name
/// included.
pub fn parse_fixed(text: &str) -> Option<i32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let rest = text
        .strip_prefix("UTC")
        .or_else(|| text.strip_prefix("GMT"))
        .unwrap_or(text);
    if rest.is_empty() {
        return Some(0);
    }
    let (negative, digits) = match rest.as_bytes()[0] {
        b'+' => (false, &rest[1..]),
        b'-' => (true, &rest[1..]),
        _ => return None,
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (two_digits(h)?, two_digits(m)?),
        None => match digits.len() {
            1 | 2 => (two_digits(digits)?, 0),
            4 => (two_digits(digits.get(..2)?)?, two_digits(digits.get(2..)?)?),
            _ => return None,
        },
    };
    if hours > 14 || minutes > 59 {
        return None;
    }
    let magnitude = hours * 3600 + minutes * 60;
    Some(if negative { -magnitude } else { magnitude })
}

/// One or two ASCII digits.
fn two_digits(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(bytes.iter().fold(0, |n, b| n * 10 + i32::from(b - b'0')))
}

/// The time zone a name denotes: a fixed offset ([`parse_fixed`]) or a zone
/// of the embedded IANA Time Zone Database release [`TZDATA_VERSION`],
/// matched without regard to ASCII case after surrounding whitespace is
/// removed. `None` for any other name: there is no fallback.
#[cfg(feature = "bundled")]
pub fn resolve(name: &str) -> Option<TimeZone<'static>> {
    if let Some(offset) = parse_fixed(name) {
        return Some(TimeZone::Fixed(offset));
    }
    let (_, tzif) = bundled_tzif(name)?;
    let zone = Zone::from_tzif(tzif)?;
    // A zone that has only ever kept one offset (`Etc/UTC`, `Etc/GMT+5`) is
    // that fixed offset.
    Some(match zone.constant_offset() {
        Some(offset) => TimeZone::Fixed(offset),
        None => TimeZone::Rules(zone),
    })
}

/// The canonical IANA name and the `TZif` bytes the embedded database holds
/// for `name`, matched as [`resolve`] matches it. The compiler copies these
/// bytes into an artifact that needs the zone.
#[cfg(feature = "bundled")]
pub fn bundled_tzif(name: &str) -> Option<(&'static str, &'static [u8])> {
    jiff_tzdb::get(name.trim())
}

/// The names of every zone in the embedded database.
#[cfg(feature = "bundled")]
pub fn bundled_names() -> impl Iterator<Item = &'static str> {
    jiff_tzdb::available()
}

/// The rules of one IANA zone, read from its `TZif` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone<'a> {
    /// The transition instants, big-endian `i64` seconds, ascending.
    times: &'a [u8],
    /// For each transition, the index of the local time type it starts.
    indices: &'a [u8],
    /// The local time types: a big-endian `i32` offset, an is-DST byte and
    /// an abbreviation index, six bytes each.
    types: &'a [u8],
    /// The footer's rule for instants after the last transition.
    footer: Option<Posix>,
}

impl<'a> Zone<'a> {
    /// Read a zone from `TZif` data of version 2 or later (RFC 8536). `None`
    /// when the bytes are not well-formed `TZif`, the version 1 data has no
    /// 64-bit section, the data declares leap seconds, a transition is out of
    /// order or names a missing type, or the footer is not a rule this crate
    /// evaluates.
    pub fn from_tzif(bytes: &'a [u8]) -> Option<Self> {
        let (v1, after_v1) = Header::read(bytes)?;
        if v1.version < b'2' {
            return None;
        }
        let v1_len = v1.data_len(4, 8)?;
        let second = after_v1.get(v1_len..)?;
        let (header, data) = Header::read(second)?;
        if header.leapcnt != 0 || header.typecnt == 0 {
            return None;
        }
        let time_bytes = header.timecnt.checked_mul(8)?;
        let type_bytes = header.typecnt.checked_mul(6)?;
        let (times, rest) = split(data, time_bytes)?;
        let (indices, rest) = split(rest, header.timecnt)?;
        let (types, rest) = split(rest, type_bytes)?;
        let (_, rest) = split(rest, header.charcnt)?;
        let (_, rest) = split(rest, header.isstdcnt)?;
        let (_, footer) = split(rest, header.isutcnt)?;

        if indices.iter().any(|&i| usize::from(i) >= header.typecnt) {
            return None;
        }
        let mut previous = i64::MIN;
        for (n, chunk) in times.chunks_exact(8).enumerate() {
            let time = be_i64(chunk);
            if n > 0 && time <= previous {
                return None;
            }
            previous = time;
        }
        // The footer is a newline, a POSIX `TZ` string (possibly empty), and
        // a newline.
        let footer = footer.strip_prefix(b"\n")?;
        let end = footer.iter().position(|&b| b == b'\n')?;
        let rule = &footer[..end];
        let footer = if rule.is_empty() {
            None
        } else {
            Some(Posix::parse(rule)?)
        };
        Some(Zone {
            times,
            indices,
            types,
            footer,
        })
    }

    /// The one offset the zone has always kept, or `None` when its offset
    /// has ever changed or may change.
    pub fn constant_offset(&self) -> Option<i32> {
        let first = self.type_offset(0);
        let types_agree = self
            .types
            .chunks_exact(6)
            .all(|chunk| i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) == first);
        match self.footer {
            Some(Posix {
                standard,
                daylight: None,
            }) if types_agree && standard == first => Some(first),
            None if types_agree => Some(first),
            _ => None,
        }
    }

    /// The offset in seconds east of UTC at `utc_seconds`.
    pub fn offset_at(&self, utc_seconds: i64) -> i32 {
        let count = self.indices.len();
        if count == 0 {
            return match self.footer {
                Some(rule) => rule.offset_at(utc_seconds),
                None => self.type_offset(0),
            };
        }
        // The number of transitions at or before `utc_seconds`.
        let (mut low, mut high) = (0, count);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.time(mid) <= utc_seconds {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low == 0 {
            // Before the first transition: the first local time type.
            return self.type_offset(0);
        }
        if low == count {
            if let Some(rule) = self.footer {
                return rule.offset_at(utc_seconds);
            }
        }
        self.type_offset(usize::from(self.indices[low - 1]))
    }

    /// [`TimeZone::utc_from_local`] for this zone.
    fn utc_from_local(&self, local_seconds: i64) -> i64 {
        // No offset exceeds a day, so the offsets in force a day either side
        // of the local reading bracket any change it falls near.
        let before = i64::from(self.offset_at(local_seconds.saturating_sub(DAY)));
        let after = i64::from(self.offset_at(local_seconds.saturating_add(DAY)));
        let candidate = local_seconds.saturating_sub(before);
        if i64::from(self.offset_at(candidate)) == before {
            return candidate;
        }
        let candidate = local_seconds.saturating_sub(after);
        if i64::from(self.offset_at(candidate)) == after {
            return candidate;
        }
        // A gap: read with the offset before the change.
        local_seconds.saturating_sub(before)
    }

    fn time(&self, index: usize) -> i64 {
        be_i64(&self.times[index * 8..index * 8 + 8])
    }

    fn type_offset(&self, index: usize) -> i32 {
        let at = index * 6;
        i32::from_be_bytes([
            self.types[at],
            self.types[at + 1],
            self.types[at + 2],
            self.types[at + 3],
        ])
    }
}

/// The counts of a `TZif` header.
struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Header {
    /// Read the 44-byte header at the start of `bytes`; the rest follows.
    fn read(bytes: &[u8]) -> Option<(Header, &[u8])> {
        let (head, rest) = split(bytes, 44)?;
        if &head[..4] != b"TZif" {
            return None;
        }
        let count = |at: usize| -> Option<usize> {
            let word = u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
            usize::try_from(word).ok()
        };
        Some((
            Header {
                version: head[4],
                isutcnt: count(20)?,
                isstdcnt: count(24)?,
                leapcnt: count(28)?,
                timecnt: count(32)?,
                typecnt: count(36)?,
                charcnt: count(40)?,
            },
            rest,
        ))
    }

    /// The length of the data block after this header, with transition
    /// times and leap-second records of `time_size` bytes (4 in version 1, 8
    /// after) and leap counts of 4 bytes after each record's time.
    fn data_len(&self, time_size: usize, leap_size: usize) -> Option<usize> {
        let times = self.timecnt.checked_mul(time_size)?;
        let types = self.typecnt.checked_mul(6)?;
        let leaps = self.leapcnt.checked_mul(leap_size)?;
        times
            .checked_add(self.timecnt)?
            .checked_add(types)?
            .checked_add(self.charcnt)?
            .checked_add(leaps)?
            .checked_add(self.isstdcnt)?
            .checked_add(self.isutcnt)
    }
}

fn split(bytes: &[u8], at: usize) -> Option<(&[u8], &[u8])> {
    if at > bytes.len() {
        return None;
    }
    Some(bytes.split_at(at))
}

fn be_i64(bytes: &[u8]) -> i64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[..8]);
    i64::from_be_bytes(word)
}

/// A POSIX `TZ` rule from a `TZif` footer: a standard offset, and possibly a
/// daylight-saving offset with the rules that start and end it each year.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Posix {
    /// The standard offset, in seconds east of UTC.
    standard: i32,
    daylight: Option<Daylight>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Daylight {
    /// The daylight-saving offset, in seconds east of UTC.
    offset: i32,
    /// When daylight-saving time starts, on the standard clock.
    start: RuleDate,
    /// When it ends, on the daylight-saving clock.
    end: RuleDate,
}

/// A day of the year and a local time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuleDate {
    day: RuleDay,
    /// Seconds after local midnight, possibly negative or past a day
    /// (RFC 8536 allows -167 to 167 hours).
    time: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleDay {
    /// `Jn`: day 1 to 365, never counting February 29.
    Julian(u16),
    /// `n`: day 0 to 365, counting February 29.
    Zero(u16),
    /// `Mm.w.d`: weekday `d` (0 is Sunday) of week `w` (5 is the last) of
    /// month `m`.
    Month { month: u8, week: u8, weekday: u8 },
}

impl Posix {
    fn parse(text: &[u8]) -> Option<Posix> {
        let mut cursor = Cursor { text, at: 0 };
        cursor.name()?;
        // POSIX offsets are hours west of UTC; these are seconds east.
        let standard = -cursor.hms(24)?;
        if cursor.done() {
            return Some(Posix {
                standard,
                daylight: None,
            });
        }
        cursor.name()?;
        let offset = if cursor.peek().is_some_and(|b| b != b',') {
            -cursor.hms(24)?
        } else {
            standard.checked_add(3600)?
        };
        // A daylight-saving name without rules leaves its dates to the
        // implementation; TZif footers always give them.
        cursor.eat(b',')?;
        let start = cursor.rule_date()?;
        cursor.eat(b',')?;
        let end = cursor.rule_date()?;
        if !cursor.done() {
            return None;
        }
        Some(Posix {
            standard,
            daylight: Some(Daylight { offset, start, end }),
        })
    }

    fn offset_at(&self, utc_seconds: i64) -> i32 {
        let Some(daylight) = self.daylight else {
            return self.standard;
        };
        let local = utc_seconds.saturating_add(i64::from(self.standard));
        let (year, _, _) = civil_from_days(local.div_euclid(DAY));
        let start = daylight.start.utc_in(year, self.standard);
        let end = daylight.end.utc_in(year, daylight.offset);
        let in_daylight = if start < end {
            utc_seconds >= start && utc_seconds < end
        } else {
            // Daylight-saving time spans the new year (southern hemisphere).
            !(utc_seconds >= end && utc_seconds < start)
        };
        if in_daylight {
            daylight.offset
        } else {
            self.standard
        }
    }
}

impl RuleDate {
    /// The UTC instant this rule names in `year`, read on a clock `offset`
    /// seconds east of UTC.
    fn utc_in(self, year: i64, offset: i32) -> i64 {
        let jan1 = days_from_civil(year, 1, 1);
        let leap = is_leap(year);
        let day = match self.day {
            RuleDay::Julian(n) => {
                let n = i64::from(n) - 1;
                jan1.saturating_add(if leap && n >= 59 { n + 1 } else { n })
            }
            RuleDay::Zero(n) => jan1.saturating_add(i64::from(n)),
            RuleDay::Month {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, i64::from(month), 1);
                let first_weekday = weekday_of(first);
                let mut day = 1
                    + (i64::from(weekday) - first_weekday).rem_euclid(7)
                    + 7 * (i64::from(week) - 1);
                let length = days_in_month(year, i64::from(month));
                while day > length {
                    day -= 7;
                }
                first.saturating_add(day - 1)
            }
        };
        day.saturating_mul(DAY)
            .saturating_add(i64::from(self.time))
            .saturating_sub(i64::from(offset))
    }
}

/// A reader over a POSIX `TZ` string.
struct Cursor<'a> {
    text: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn done(&self) -> bool {
        self.at == self.text.len()
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek()? == byte).then(|| self.at += 1)
    }

    /// A zone abbreviation: three or more letters, or `<...>`.
    fn name(&mut self) -> Option<()> {
        if self.peek()? == b'<' {
            self.at += 1;
            let start = self.at;
            while self.peek()? != b'>' {
                self.at += 1;
            }
            if self.at - start < 3 {
                return None;
            }
            self.at += 1;
            return Some(());
        }
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            self.at += 1;
        }
        (self.at - start >= 3).then_some(())
    }

    /// `[+-]hh[:mm[:ss]]` in seconds, with hours up to `max_hours`.
    fn hms(&mut self, max_hours: i32) -> Option<i32> {
        let negative = match self.peek()? {
            b'+' => {
                self.at += 1;
                false
            }
            b'-' => {
                self.at += 1;
                true
            }
            _ => false,
        };
        let hours = self.number(3)?;
        if hours > max_hours {
            return None;
        }
        let mut seconds = hours * 3600;
        if self.peek() == Some(b':') {
            self.at += 1;
            let minutes = self.number(2)?;
            if minutes > 59 {
                return None;
            }
            seconds += minutes * 60;
            if self.peek() == Some(b':') {
                self.at += 1;
                let secs = self.number(2)?;
                if secs > 59 {
                    return None;
                }
                seconds += secs;
            }
        }
        Some(if negative { -seconds } else { seconds })
    }

    /// One to `digits` ASCII digits.
    fn number(&mut self, digits: usize) -> Option<i32> {
        let start = self.at;
        let mut value: i32 = 0;
        while self.at - start < digits {
            match self.peek() {
                Some(b) if b.is_ascii_digit() => {
                    value = value * 10 + i32::from(b - b'0');
                    self.at += 1;
                }
                _ => break,
            }
        }
        (self.at > start).then_some(value)
    }

    /// `Jn`, `n` or `Mm.w.d`, then an optional `/time` (02:00 by default).
    fn rule_date(&mut self) -> Option<RuleDate> {
        let day = match self.peek()? {
            b'J' => {
                self.at += 1;
                let n = self.number(3)?;
                if !(1..=365).contains(&n) {
                    return None;
                }
                RuleDay::Julian(u16::try_from(n).ok()?)
            }
            b'M' => {
                self.at += 1;
                let month = self.number(2)?;
                self.eat(b'.')?;
                let week = self.number(1)?;
                self.eat(b'.')?;
                let weekday = self.number(1)?;
                if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
                    return None;
                }
                RuleDay::Month {
                    month: u8::try_from(month).ok()?,
                    week: u8::try_from(week).ok()?,
                    weekday: u8::try_from(weekday).ok()?,
                }
            }
            _ => {
                let n = self.number(3)?;
                if n > 365 {
                    return None;
                }
                RuleDay::Zero(u16::try_from(n).ok()?)
            }
        };
        let time = if self.peek() == Some(b'/') {
            self.at += 1;
            self.hms(167)?
        } else {
            2 * 3600
        };
        Some(RuleDate { day, time })
    }
}

/// How a trading session lies in the exchange's day, which is what
/// grouping bars into days and higher timeframes depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionShape {
    /// No session declared (an empty or blank specification).
    Absent,
    /// Trading around the clock, each day from local midnight to midnight
    /// (`24x7`, `0000-0000`).
    Continuous,
    /// Every period opens and closes within one local day (`0930-1600`,
    /// `0900-1130,1230-1500`).
    WithinDay,
    /// A period crosses local midnight, or a 24-hour day starts at another
    /// time (`1700-1600`, `1700-1700`).
    Overnight,
}

/// The shape of a session specification, or `None` when it is
/// not one: `24x7` (any case), or comma-separated `HHMM-HHMM` periods
/// optionally followed by `:` and the trading days (`1` Sunday to `7`),
/// surrounding whitespace ignored. The interpreter's `time(timeframe, session)`
/// built-in uses this parser to read session strings.
pub fn session_shape(hours: &str) -> Option<SessionShape> {
    let hours = hours.trim();
    if hours.is_empty() {
        return Some(SessionShape::Absent);
    }
    if hours.eq_ignore_ascii_case("24x7") {
        return Some(SessionShape::Continuous);
    }
    let (periods, days) = hours.split_once(':').unwrap_or((hours, "1234567"));
    if periods.is_empty() || days.is_empty() {
        return None;
    }
    if !days.bytes().all(|b| (b'1'..=b'7').contains(&b)) {
        return None;
    }
    let (mut continuous, mut within_day) = (true, true);
    for period in periods.split(',') {
        let (start, end) = period.split_once('-')?;
        let (start, end) = (session_minute(start)?, session_minute(end)?);
        continuous &= start == 0 && end == 0;
        within_day &= start < end;
    }
    Some(if continuous {
        SessionShape::Continuous
    } else if within_day {
        SessionShape::WithinDay
    } else {
        SessionShape::Overnight
    })
}

/// `HHMM`, four ASCII digits, as minutes after midnight.
fn session_minute(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    if bytes.len() != 4 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let digit = |at: usize| u16::from(bytes[at] - b'0');
    let (hour, minute) = (digit(0) * 10 + digit(1), digit(2) * 10 + digit(3));
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`), saturating far outside any market's history.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 {
        year.saturating_sub(1)
    } else {
        year
    };
    let era = year.div_euclid(400);
    let year_of_era = year - era.saturating_mul(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.saturating_mul(146_097)
        .saturating_add(day_of_era)
        .saturating_sub(719_468)
}

/// The proleptic Gregorian `(year, month, day)` of `days` since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era.saturating_add(era.saturating_mul(400));
    (
        if month <= 2 {
            year.saturating_add(1)
        } else {
            year
        },
        month,
        day,
    )
}

fn is_leap(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The weekday of `days` since 1970-01-01, 0 for Sunday.
fn weekday_of(days: i64) -> i64 {
    // 1970-01-01 was a Thursday.
    (days + 4).rem_euclid(7)
}

#[cfg(test)]
mod tests;
