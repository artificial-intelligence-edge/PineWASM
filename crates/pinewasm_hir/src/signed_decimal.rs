//! Checked durable-wire decimal parsing. The Serde token dispatch stays in lib.rs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Empty,
    InvalidDigit,
    PositiveOverflow,
    NegativeOverflow,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "cannot parse integer from empty string",
            Self::InvalidDigit => "invalid digit found in string",
            Self::PositiveOverflow => "number too large to fit in target type",
            Self::NegativeOverflow => "number too small to fit in target type",
        })
    }
}

/// Optional ASCII sign followed by one or more ASCII digits, with exact i64
/// bounds. Negative accumulation represents MIN without an overflowing negate.
/// An invalid current byte wins over multiplication overflow at that byte;
/// overflow reached at an earlier digit wins over a later invalid byte.
#[expect(
    clippy::bool_to_int_with_if,
    clippy::manual_range_contains,
    reason = "Keep scalar branches explicit for the machine-checked Aeneas parser model"
)]
pub(crate) fn parse(bytes: &[u8]) -> Result<i64, Error> {
    if bytes.is_empty() {
        return Err(Error::Empty);
    }
    let negative = bytes[0] == b'-';
    let mut index = if negative || bytes[0] == b'+' { 1 } else { 0 };
    if index == bytes.len() {
        return Err(Error::InvalidDigit);
    }
    let overflow = if negative {
        Error::NegativeOverflow
    } else {
        Error::PositiveOverflow
    };
    let mut value: i64 = 0;
    let mut pending_error = None;
    while index < bytes.len() && pending_error.is_none() {
        let byte = bytes[index];
        if byte < b'0' || byte > b'9' {
            pending_error = Some(Error::InvalidDigit);
        } else {
            let digit = i64::from(byte - b'0');
            match value.checked_mul(10) {
                None => pending_error = Some(overflow),
                Some(multiplied) => {
                    match if negative {
                        multiplied.checked_sub(digit)
                    } else {
                        multiplied.checked_add(digit)
                    } {
                        None => pending_error = Some(overflow),
                        Some(accumulated) => {
                            value = accumulated;
                            index += 1;
                        }
                    }
                }
            }
        }
    }
    match pending_error {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Error};

    #[test]
    fn matches_std_value_and_public_error() {
        let cases = [
            "",
            "+",
            "-",
            "0",
            "-0",
            "+000",
            "01",
            "-01",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "-9223372036854775809",
            "999999999999999999999x",
            "-999999999999999999999x",
            "9223372036854775807x",
            "-9223372036854775808x",
            " 1",
            "1 ",
            "1\n",
            "++1",
            "--1",
            "+-1",
            "١",
            "１",
            "1.0",
            "1e0",
        ];
        for text in cases {
            let actual = parse(text.as_bytes());
            let expected = text.parse::<i64>();
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{text:?}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.to_string(), expected.to_string(), "{text:?}");
                }
                pair => panic!("acceptance mismatch for {text:?}: {pair:?}"),
            }
        }
    }

    #[test]
    fn current_invalid_digit_and_prior_overflow_precedence() {
        assert_eq!(parse(b"9223372036854775807x"), Err(Error::InvalidDigit));
        assert_eq!(parse(b"9223372036854775808x"), Err(Error::PositiveOverflow));
        assert_eq!(parse(b"-9223372036854775808x"), Err(Error::InvalidDigit));
        assert_eq!(
            parse(b"-9223372036854775809x"),
            Err(Error::NegativeOverflow)
        );
    }
    fn assert_std(text: &str) {
        match (parse(text.as_bytes()), text.parse::<i64>()) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{text:?}"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{text:?}"),
            pair => panic!("acceptance mismatch for {text:?}: {pair:?}"),
        }
    }

    #[test]
    fn exhaustive_short_strings_and_long_boundaries() {
        fn visit(text: &mut String, remaining: usize) {
            assert_std(text);
            if remaining == 0 {
                return;
            }
            for ch in ['+', '-', '0', '1', '9', 'x', ' ', '\n'] {
                text.push(ch);
                visit(text, remaining - 1);
                text.pop();
            }
        }
        visit(&mut String::new(), 4);
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            let text = value.to_string();
            for suffix in ["", "x", "0", "9"] {
                assert_std(&(text.clone() + suffix));
            }
            let (sign, digits) = if let Some(rest) = text.strip_prefix('-') {
                ("-", rest)
            } else {
                ("+", text.as_str())
            };
            assert_std(&format!("{sign}{}{digits}", "0".repeat(256)));
        }
        for sign in ["", "+", "-"] {
            for digits in ["0".repeat(1024), "9".repeat(1024)] {
                for suffix in ["", "x", " "] {
                    assert_std(&format!("{sign}{digits}{suffix}"));
                }
            }
        }
    }

    #[test]
    fn actual_serde_dispatch_matches_previous_wrapper() {
        use serde::Deserialize;
        #[derive(Debug, Deserialize)]
        struct Actual {
            #[serde(deserialize_with = "crate::deserialize_i64_number_or_string")]
            value: i64,
        }
        fn old<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum WireI64 {
                Integer(i64),
                Decimal(String),
            }
            match WireI64::deserialize(d)? {
                WireI64::Integer(v) => Ok(v),
                WireI64::Decimal(v) => v.parse().map_err(serde::de::Error::custom),
            }
        }
        #[derive(Debug, Deserialize)]
        struct Previous {
            #[serde(deserialize_with = "old")]
            value: i64,
        }
        for payload in [
            "0",
            "-9223372036854775808",
            "9223372036854775807",
            "9223372036854775808",
            "1.0",
            "1e0",
            "null",
            "true",
            "false",
            "[]",
            "{}",
            "\"+0001\"",
            "\"-0\"",
            "\"\"",
            "\"+\"",
            "\"1.0\"",
            "\"9223372036854775808x\"",
            "\"9223372036854775807x\"",
            "\"١\"",
        ] {
            let json = format!("{{\"value\":{payload}}}");
            match (
                serde_json::from_str::<Actual>(&json),
                serde_json::from_str::<Previous>(&json),
            ) {
                (Ok(a), Ok(b)) => assert_eq!(a.value, b.value, "{json}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{json}"),
                pair => panic!("Serde dispatch mismatch: {json}: {pair:?}"),
            }
        }
    }
}
