//! str.* helpers: pure string formatting and parsing for
//! `str.tostring`/`str.format`/`str.*` builtins — no `Vm` state.

use pinewasm_mir::Value;

/// Default numeric format `str.tostring`/`str.format` use when no format is
/// given: up to 8 fractional digits, trailing zeros trimmed (`"#.########"`).
pub(crate) fn default_num_string(v: f64) -> String {
    if !v.is_finite() {
        return "NaN".to_string();
    }
    // Round to 8 decimals, then trim trailing zeros and a bare '.'.
    let mut s = format!("{v:.8}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    // Normalize "-0" to "0".
    if s == "-0" {
        s = "0".to_string();
    }
    s
}

/// Stringify a non-`na` scalar `Value` for `str.tostring`/`str.format`.
/// `format` (when `Some`) applies to numeric values: `format.mintick`,
/// `format.percent`, `format.volume`, or a `#`/`0`/`.`/`,` numeric pattern.
pub(crate) fn tostring_value(value: &Value, format: Option<&str>, mintick: f64) -> String {
    match value {
        Value::Na | Value::DisplacedTable(_) => "NaN".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Int(i) => match format {
            None => i.to_string(),
            Some(f) => format_number_int(*i, f, mintick),
        },
        Value::Float(v) => match format {
            None => default_num_string(*v),
            Some(f) => format_number(*v, f, mintick),
        },
        // Colors are not `Stringable` (the checker rejects them as
        // `str.tostring`/`str.format` args), so this is unreachable; render
        // the hex form defensively rather than panicking on an invalid CSIL.
        Value::Color { r, g, b, t, .. } => Value::color_hex(*r, *g, *b, *t),
        // Arrays and matrices are written by the VM, which owns the heap
        // (`Vm::collection_string`); render the handle form defensively for
        // any other caller rather than panicking on an invalid CSIL.
        Value::Ref(_) => value.render(),
    }
}

/// Apply a numeric format string/constant to a value.
///
/// Supported subset:
/// - `format.mintick` → round to the instrument's mintick, trailing zeros
///   trimmed;
/// - `format.percent` → multiply by 100 and append `%` (2 decimals);
/// - `format.volume` → K/M/B/T abbreviation;
/// - anything else → a `DecimalFormat`-style pattern (see `format_pattern`).
pub(crate) fn format_number(v: f64, format: &str, mintick: f64) -> String {
    if !v.is_finite() {
        return "NaN".to_string();
    }
    match format {
        // `format.inherit`/`format.price` use the chart's default formatting:
        // fall back to the default numeric string. (`format.price`'s exact
        // price-scale precision is not implemented.)
        "format.inherit" | "format.price" => default_num_string(v),
        "format.mintick" => default_num_string((v / mintick).round() * mintick),
        "format.percent" => format!("{:.2}%", v * 100.0),
        "format.volume" => format_volume(v),
        pattern => format_pattern_with_rounding(v, pattern, true),
    }
}

/// `format_number` for an exact `i64`. Pattern formatting keeps every digit
/// (`Value::Int` guarantees exact semantics and Java's `MessageFormat`
/// formats longs exactly — a round-trip through `f64` would corrupt values
/// beyond 2^53). The inherently fractional `format.*` constants
/// (mintick/percent/volume) keep their `f64` semantics.
pub(crate) fn format_number_int(i: i64, format: &str, mintick: f64) -> String {
    match format {
        "format.inherit" | "format.price" => i.to_string(),
        "format.mintick" | "format.percent" | "format.volume" => {
            format_number(i as f64, format, mintick)
        }
        pattern => format_pattern_int(i, pattern),
    }
}

/// K/M/B/T abbreviation used by `format.volume` (3 significant
/// digits).
pub(crate) fn format_volume(v: f64) -> String {
    let abs = v.abs();
    let (scaled, suffix) = if abs >= 1e12 {
        (v / 1e12, "T")
    } else if abs >= 1e9 {
        (v / 1e9, "B")
    } else if abs >= 1e6 {
        (v / 1e6, "M")
    } else if abs >= 1e3 {
        (v / 1e3, "K")
    } else {
        (v, "")
    };
    if suffix.is_empty() {
        default_num_string(scaled)
    } else {
        format!("{scaled:.3}{suffix}")
    }
}

/// Apply an ICU `DecimalFormat`-style numeric pattern (the subset shared by
/// `str.tostring` formats and `{N,number,…}` placeholders). Fractional digits
/// come from the `0`/`#` slots after the `.` (`0` always prints, trailing `#`
/// trims); integer-part `0`s zero-pad; a `,` in the integer part enables
/// thousands grouping (group size = digit slots after the last `,`, so Java's
/// `#,##,###` still groups by 3); `%` multiplies by 100; any other characters
/// are literal prefix/suffix text. The minus sign renders before the prefix
/// (`-$1.34`), matching Java's default negative subpattern.
pub(crate) fn format_pattern(v: f64, pattern: &str) -> String {
    format_pattern_with_rounding(v, pattern, false)
}

fn format_pattern_with_rounding(v: f64, pattern: &str, ties_away: bool) -> String {
    let spec = DecimalPattern::parse(pattern);
    let mut value = if spec.percent { v * 100.0 } else { v };
    if ties_away && spec.max_frac <= 15 {
        let scaled = value.abs() * 10_f64.powi(spec.max_frac as i32);
        if scaled.is_finite() && scaled.fract() == 0.5 {
            // Move a decimal tie one representable value away from zero;
            // preserve the ordinary formatter for non-ties and large values.
            value = if value.is_sign_negative() {
                value.next_down()
            } else {
                value.next_up()
            };
        }
    }

    // MessageFormat retains HALF_EVEN; tostring adjusts ties above.
    // then split into sign / integer digits / fraction digits.
    let rounded = format!("{value:.prec$}", prec = spec.max_frac);
    let (negative, magnitude) = match rounded.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, rounded.as_str()),
    };
    let (int_digits, frac_digits) = match magnitude.split_once('.') {
        Some((i, f)) => (i, f),
        None => (magnitude, ""),
    };
    spec.render(negative, int_digits, frac_digits)
}

/// `format_pattern` for an exact `i64`: the digits come straight from the
/// integer, never through `f64` (which would corrupt values beyond 2^53).
/// A `%` pattern falls back to the float path when multiplying by 100
/// overflows `i64`.
pub(crate) fn format_pattern_int(i: i64, pattern: &str) -> String {
    let spec = DecimalPattern::parse(pattern);
    let value = if spec.percent {
        match i.checked_mul(100) {
            Some(v) => v,
            None => return format_pattern(i as f64, pattern),
        }
    } else {
        i
    };
    // All-zero fraction digits: `render` trims them down to the pattern's
    // `0` slots exactly like a whole-valued float's.
    let frac = "0".repeat(spec.max_frac);
    spec.render(value < 0, &value.unsigned_abs().to_string(), &frac)
}

/// A parsed ICU `DecimalFormat`-style pattern (see `format_pattern` for the
/// supported subset).
pub(crate) struct DecimalPattern<'a> {
    prefix: &'a str,
    suffix: &'a str,
    percent: bool,
    max_frac: usize,
    min_frac: usize,
    min_int: usize,
    group: Option<usize>,
}

impl<'a> DecimalPattern<'a> {
    fn parse(pattern: &'a str) -> Self {
        // Split into literal prefix, numeric core (first through last special
        // char), literal suffix. A pattern with no digit slots is invalid in
        // ICU (Java throws); degrade to literal text plus the bare digits.
        let is_special = |c: char| matches!(c, '#' | '0' | '.' | ',');
        let (prefix, core, suffix) = match (pattern.find(is_special), pattern.rfind(is_special)) {
            (Some(start), Some(end)) => (
                &pattern[..start],
                &pattern[start..=end],
                &pattern[end + 1..],
            ),
            _ => (pattern, "", ""),
        };

        let (int_pat, frac_pat) = match core.split_once('.') {
            Some((i, f)) => (i, f),
            None => (core, ""),
        };
        let digit_slots = |s: &str| s.chars().filter(|c| matches!(c, '0' | '#')).count();
        DecimalPattern {
            prefix,
            suffix,
            percent: prefix.contains('%') || suffix.contains('%'),
            max_frac: digit_slots(frac_pat),
            min_frac: frac_pat.chars().filter(|c| *c == '0').count(),
            min_int: int_pat.chars().filter(|c| *c == '0').count(),
            group: int_pat
                .rsplit_once(',')
                .map(|(_, tail)| digit_slots(tail))
                .filter(|g| *g > 0),
        }
    }

    /// Assemble sign, literal prefix, zero-padded/grouped integer digits,
    /// the fraction trimmed down to the `0` slots, and the literal suffix.
    /// `int_digits`/`frac_digits` are bare ASCII digit strings.
    fn render(&self, negative: bool, int_digits: &str, frac_digits: &str) -> String {
        // Trim trailing fraction zeros down to min_frac (the `#` slots).
        let mut frac = frac_digits;
        while frac.len() > self.min_frac && frac.ends_with('0') {
            frac = &frac[..frac.len() - 1];
        }

        // Zero-pad the integer digits to min_int, then insert separators.
        let mut int_str = "0".repeat(self.min_int.saturating_sub(int_digits.len()));
        int_str.push_str(int_digits);
        if let Some(g) = self.group {
            int_str = group_separated(&int_str, g);
        }

        let mut out = String::with_capacity(
            1 + self.prefix.len() + int_str.len() + 1 + frac.len() + self.suffix.len(),
        );
        if negative {
            out.push('-');
        }
        out.push_str(self.prefix);
        out.push_str(&int_str);
        if !frac.is_empty() {
            out.push('.');
            out.push_str(frac);
        }
        out.push_str(self.suffix);
        out
    }
}

/// Insert `,` separators into a bare digit string every `group` digits,
/// counting from the right (`group_separated("1234567", 3)` → `"1,234,567"`).
pub(crate) fn group_separated(digits: &str, group: usize) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / group);
    let mut remaining = digits.len();
    for c in digits.chars() {
        out.push(c);
        remaining -= 1;
        if remaining > 0 && remaining % group == 0 {
            out.push(',');
        }
    }
    out
}

/// Substitute `{N}` and `{N,number[,style]}` placeholders in a format string
/// with stringified args (see `render_placeholder` for the `MessageFormat`
/// subset). Apostrophes quote literal text (`'{'` is a `{`, `''` an
/// apostrophe), and `{{`/`}}` escape literal braces. Out-of-range indices, malformed
/// bodies, and unsupported format types are left verbatim — Java throws on
/// most of those; degrading to the literal text keeps rendering total. `na`
/// args stringify to `"NaN"`.
pub(crate) fn format_placeholders(fmt: &str, subs: &[Value], mintick: f64) -> String {
    let mut out = String::with_capacity(fmt.len());
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // In format strings an apostrophe starts or ends a quoted
            // section: text between two apostrophes is copied literally, so `'{'` writes a
            // `{`, and `''` writes one apostrophe, inside or outside a quoted
            // span (Java `MessageFormat`). An unclosed span runs to the end.
            b'\'' if i + 1 < bytes.len() && bytes[i + 1] == b'\'' => {
                out.push('\'');
                i += 2;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            out.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    let ch_len = utf8_len(bytes[i]);
                    out.push_str(&fmt[i..i + ch_len]);
                    i += ch_len;
                }
            }
            b'{' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                out.push('{');
                i += 2;
            }
            b'}' if i + 1 < bytes.len() && bytes[i + 1] == b'}' => {
                out.push('}');
                i += 2;
            }
            b'{' => {
                // Parse `{` index [`,` type [`,` style]] `}`.
                if let Some(close) = fmt[i..].find('}') {
                    let body = &fmt[i + 1..i + close];
                    if let Some(text) = render_placeholder(body, subs, mintick) {
                        out.push_str(&text);
                        i += close + 1;
                        continue;
                    }
                }
                // Malformed/unsupported placeholder: copy the brace verbatim
                // (its body then flows through as literal text).
                out.push('{');
                i += 1;
            }
            _ => {
                // Copy one UTF-8 character.
                let ch_len = utf8_len(bytes[i]);
                out.push_str(&fmt[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    out
}

/// Render one `{…}` placeholder body (the text between the braces) for
/// `format_placeholders`. Supports `N`, `N,number`, and `N,number,style`
/// where style is `integer`/`currency`/`percent` (Java's en-US keyword
/// formats, expressed as their `DecimalFormat` patterns) or a custom pattern
/// (see `format_pattern`). Java trims and lowercases the type and style when
/// matching keywords but keeps a custom pattern's whitespace as literal text
/// (`{0, number, #}` renders `" 3"`); we do the same. Returns `None` — leave
/// the text verbatim — for malformed or out-of-range indices and for
/// non-`number` format types (`date`, `time` and `choice` are not
/// supported).
pub(crate) fn render_placeholder(body: &str, subs: &[Value], mintick: f64) -> Option<String> {
    let (index, spec) = match body.split_once(',') {
        Some((index, spec)) => (index, Some(spec)),
        None => (body, None),
    };
    let value = subs.get(index.parse::<usize>().ok()?)?;
    let Some(spec) = spec else {
        return Some(tostring_value(value, None, mintick));
    };
    let (ty, style) = match spec.split_once(',') {
        Some((ty, style)) => (ty, Some(style)),
        None => (spec, None),
    };
    if !ty.trim().eq_ignore_ascii_case("number") {
        return None;
    }
    let pattern = match style {
        // Bare `{N,number}` (or an empty style): Java's en-US default
        // number format.
        None => "#,##0.###",
        Some(style) => match style.trim() {
            "" => "#,##0.###",
            s if s.eq_ignore_ascii_case("integer") => "#,##0",
            s if s.eq_ignore_ascii_case("currency") => "$#,##0.00",
            s if s.eq_ignore_ascii_case("percent") => "#,##0%",
            _ => style,
        },
    };
    Some(match value {
        Value::Float(v) => format_pattern(*v, pattern),
        Value::Int(i) => format_pattern_int(*i, pattern),
        _ => tostring_value(value, Some(pattern), mintick),
    })
}

/// Byte length of the UTF-8 character whose leading byte is `b`.
pub(crate) fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// `str.tonumber` grammar: an optional sign, decimal digits with a single
/// optional `.`. Returns `Value::Float` or `na` (never `Int`, so the result
/// is always a float). Leading/trailing spaces are tolerated.
pub(crate) fn parse_pine_number(s: &str) -> Value {
    let t = s.trim();
    if t.is_empty() {
        return Value::Na;
    }
    // Reject exponents and other forms the Rust parser accepts but this
    // grammar does not: only
    // [+-]?digits(.digits)? with at most one dot.
    let mut chars = t.chars().peekable();
    if matches!(chars.peek(), Some('+' | '-')) {
        chars.next();
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for c in chars {
        match c {
            '0'..='9' => saw_digit = true,
            '.' if !saw_dot => saw_dot = true,
            _ => return Value::Na,
        }
    }
    if !saw_digit {
        return Value::Na;
    }
    match t.parse::<f64>() {
        Ok(v) => Value::from_f64(v),
        Err(_) => Value::Na,
    }
}

/// `str.substring(source, begin[, end])` over Unicode code points. `begin`
/// is inclusive and `end` exclusive. `str.substring` rejects
/// invalid starts and reversed ranges; an omitted or oversized end uses len.
pub(crate) fn str_substring(source: &str, begin: f64, end: Option<f64>) -> Result<Value, String> {
    let chars: Vec<char> = source.chars().collect();
    let len = chars.len() as i64;
    let b = begin as i64;
    if b < 0 || b > len {
        return Err(format!("str.substring begin_pos {b} is outside [0, {len}]"));
    }
    let e = end.map_or(len, |value| value as i64);
    if e < b {
        return Err(format!(
            "str.substring end_pos {e} is smaller than begin_pos {b}"
        ));
    }
    Ok(Value::String(
        chars[b as usize..e.min(len) as usize].iter().collect(),
    ))
}

/// Defensive upper bound (in bytes) on any string a single VM operation will
/// build. This cap is generous for ordinary scripts while ensuring
/// string-growing ops (`str.repeat`, `+`/`StrConcat`,
/// `str.replace[_all]`, …) can never allocate unboundedly — e.g. a doubling
/// loop like `s := s + s` yields `na` instead of exhausting memory.
pub(crate) const MAX_STRING_LEN: usize = 1_000_000;

/// `str.replace_all`: replace every non-overlapping occurrence. An empty
/// target is a no-op (avoids an infinite match). A result that would exceed
/// the defensive size cap yields `na` rather
/// than allocating unboundedly — otherwise `s := str.replace_all(s, "a", "aa")`
/// in a loop doubles the string every iteration.
pub(crate) fn str_replace_all(source: &str, target: &str, replacement: &str) -> Value {
    if target.is_empty() {
        return Value::String(source.to_string());
    }
    // Project the grown length without allocating (only grows when the
    // replacement is longer than the target).
    if replacement.len() > target.len() {
        let matches = source.matches(target).count();
        let growth = matches.saturating_mul(replacement.len() - target.len());
        if source.len().saturating_add(growth) > MAX_STRING_LEN {
            return Value::Na;
        }
    }
    Value::String(source.replace(target, replacement))
}

/// `str.replace`: replace the `occurrence`-th (0-based) non-overlapping
/// occurrence of `target`. A missing occurrence or empty target returns the
/// source unchanged. A single replacement grows the string by at most
/// `replacement.len() - target.len()`; a result past the defensive size cap
/// yields `na` so a `var` string cannot grow without bound across bars.
pub(crate) fn str_replace_nth(
    source: &str,
    target: &str,
    replacement: &str,
    occurrence: f64,
) -> Value {
    if target.is_empty() || occurrence < 0.0 {
        return Value::String(source.to_string());
    }
    let occ = occurrence as usize;
    let mut count = 0;
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find(target) {
        let at = search_from + rel;
        if count == occ {
            // Only cap once a match is actually found: a missing occurrence
            // returns the source unchanged (no growth), so the cap must not
            // pre-empt that case with `na`.
            if replacement.len() > target.len()
                && source
                    .len()
                    .saturating_add(replacement.len() - target.len())
                    > MAX_STRING_LEN
            {
                return Value::Na;
            }
            let mut out = String::with_capacity(source.len());
            out.push_str(&source[..at]);
            out.push_str(replacement);
            out.push_str(&source[at + target.len()..]);
            return Value::String(out);
        }
        count += 1;
        search_from = at + target.len();
    }
    Value::String(source.to_string())
}

/// `str.repeat(source, count[, separator])`. `count = 0` → empty string;
/// negative counts are treated as 0. A large count that would exceed a
/// defensive size cap yields `na` rather than allocating unboundedly.
///
/// The loop below runs `count` times, so `count` must be bounded by the size
/// cap before it starts. The cap bounds it only when a repeat contributes at
/// least one byte: with an empty source and separator the projection is zero
/// for every count, so that case returns the (empty) result directly.
pub(crate) fn str_repeat(source: &str, count: f64, separator: &str) -> Value {
    if count < 0.0 || (source.is_empty() && separator.is_empty()) {
        return Value::String(String::new());
    }
    let n = count as usize;
    if n == 0 {
        return Value::String(String::new());
    }
    // Defensive cap: strings are bounded at MAX_STRING_LEN bytes; refuse
    // to build anything wildly larger.
    let approx = source
        .len()
        .saturating_mul(n)
        .saturating_add(separator.len().saturating_mul(n));
    if approx > MAX_STRING_LEN {
        return Value::Na;
    }
    let mut out = String::with_capacity(approx);
    for i in 0..n {
        if i > 0 {
            out.push_str(separator);
        }
        out.push_str(source);
    }
    Value::String(out)
}

#[cfg(test)]
mod repeat_saturating_sum_tests {
    use super::{str_repeat, MAX_STRING_LEN};
    use crate::Value;

    #[test]
    fn two_nonempty_contributors_refuse_an_overflowing_sum() {
        // Both supported pointer widths (32 and 64) have an exactly
        // representable power of two whose doubled byte count overflows usize.
        let n = 1usize << (usize::BITS - 1);
        let count = n as f64;
        assert_eq!(count as usize, n, "the regression count must remain exact");
        assert_eq!(str_repeat("x", count, ","), Value::Na);
    }

    #[test]
    fn cap_and_separator_policy_remain_conservative() {
        let count = MAX_STRING_LEN / 2;
        let Value::String(result) = str_repeat("x", count as f64, ",") else {
            panic!("the conservative projected length equals the cap");
        };
        assert_eq!(result.len(), count * 2 - 1);
        assert!(result.starts_with("x,x") && result.ends_with(",x"));
        assert_eq!(str_repeat("x", (count + 1) as f64, ","), Value::Na);
        // Exact output length would equal the cap here, but the established
        // projection counts n separators rather than n-1 and must still refuse.
        assert_eq!(str_repeat("", (MAX_STRING_LEN + 1) as f64, "-"), Value::Na);
        assert_eq!(
            str_repeat("x", MAX_STRING_LEN as f64, ""),
            Value::String("x".repeat(MAX_STRING_LEN))
        );
    }

    #[test]
    fn empty_and_zero_results_do_not_change() {
        for count in [f64::INFINITY, f64::MAX, i64::MAX as f64] {
            assert_eq!(str_repeat("", count, ""), Value::String(String::new()));
        }
        for count in [0.0, -1.0, 0.5, f64::NAN] {
            assert_eq!(str_repeat("x", count, ","), Value::String(String::new()));
        }
        assert_eq!(str_repeat("ab", 3.0, "-"), Value::String("ab-ab-ab".into()));
        assert_eq!(str_repeat("", 3.0, "-"), Value::String("--".into()));
    }
}
