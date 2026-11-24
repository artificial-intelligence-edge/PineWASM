//! Exact checked decimal conversion for the integer arm of `parse_primary`.
//!
//! The source parser reports one diagnostic for every conversion failure, so
//! only the accepted value is returned here. Floating literal classification
//! and AST construction remain in the caller.

/// Optional ASCII sign followed by at least one ASCII digit, within i64.
/// Negative accumulation includes MIN without an overflowing final negation.
#[expect(
    clippy::bool_to_int_with_if,
    clippy::manual_range_contains,
    reason = "Keep scalar branches explicit."
)]
pub(crate) fn parse(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let negative = bytes[0] == b'-';
    let mut index = if negative || bytes[0] == b'+' { 1 } else { 0 };
    if index == bytes.len() {
        return None;
    }
    let mut value: i64 = 0;
    let mut failed = false;
    while index < bytes.len() && !failed {
        let byte = bytes[index];
        if byte < b'0' || byte > b'9' {
            failed = true;
        } else {
            let digit = i64::from(byte - b'0');
            match value.checked_mul(10) {
                None => failed = true,
                Some(multiplied) => {
                    match if negative {
                        multiplied.checked_sub(digit)
                    } else {
                        multiplied.checked_add(digit)
                    } {
                        None => failed = true,
                        Some(accumulated) => {
                            value = accumulated;
                            index += 1;
                        }
                    }
                }
            }
        }
    }
    if failed {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::parse_with_source_id;
    use pinewasm_span::{Diagnostic, SourceId, Span};
    use pinewasm_syntax::{Expr, Ident, Item, Stmt};

    // Expected values: the standard library's `i64` parse, which `parse` must match.
    fn previous(text: &str) -> Option<i64> {
        text.parse::<i64>().ok()
    }

    fn assert_previous(text: &str) {
        assert_eq!(parse(text.as_bytes()), previous(text), "{text:?}");
    }

    #[test]
    fn frozen_std_boundaries_and_precision() {
        for text in [
            "",
            "+",
            "-",
            "0",
            "-0",
            "+0",
            "000000",
            "+001",
            "-001",
            "9007199254740993",
            "-9007199254740993",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "-9223372036854775809",
            "9223372036854775807x",
            "-9223372036854775808x",
            "9223372036854775808x",
            "-9223372036854775809x",
            " 1",
            "1 ",
            "1\n",
            "1\t",
            "1\r",
            "1\0",
            "++1",
            "--1",
            "+-1",
            "-+1",
            "1.0",
            "1e0",
            "1E0",
            "١",
            "１",
            "−1",
        ] {
            assert_previous(text);
        }
        assert_eq!(parse(b"9007199254740993"), Some(9_007_199_254_740_993));
        assert_eq!(parse(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse(b"9223372036854775808"), None);
    }

    #[test]
    fn exhaustive_short_reference() {
        fn visit(text: &mut String, remaining: usize, count: &mut usize) {
            assert_previous(text);
            *count += 1;
            if remaining != 0 {
                for ch in ['+', '-', '0', '1', '8', '9', 'x', ' ', '\n', '\0'] {
                    text.push(ch);
                    visit(text, remaining - 1, count);
                    text.pop();
                }
            }
        }
        let mut count = 0;
        visit(&mut String::new(), 5, &mut count);
        assert_eq!(count, 111_111);
    }

    #[test]
    fn long_zeros_overflow_and_late_invalid_reference() {
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            let text = value.to_string();
            for suffix in ["", "x", "0", "9", "\0"] {
                assert_previous(&(text.clone() + suffix));
            }
            let (sign, digits) = text
                .strip_prefix('-')
                .map_or(("+", text.as_str()), |rest| ("-", rest));
            assert_previous(&format!("{sign}{}{digits}", "0".repeat(1024)));
        }
        for sign in ["", "+", "-"] {
            for digits in ["0".repeat(4096), "9".repeat(4096)] {
                for suffix in ["", "x", " ", "\0"] {
                    assert_previous(&format!("{sign}{digits}{suffix}"));
                }
            }
        }
    }

    #[test]
    fn every_non_digit_byte_is_rejected() {
        for byte in 0..=u8::MAX {
            if !byte.is_ascii_digit() {
                assert_eq!(parse(&[b'1', byte]), None, "{byte}");
                assert_eq!(parse(&[b'-', b'1', byte]), None, "{byte}");
            }
        }
    }

    #[test]
    fn actual_assignment_ast_and_source_spans() {
        let source_id = SourceId(73);
        let cases = [
            ("x", "", "", "1", ""),
            ("alpha_9", " \t", "\t ", "0008", " \t"),
            ("x", " ", " ", "9007199254740993", "\n"),
            ("x", "", "", "9223372036854775807", "\r"),
            ("_x", "\t", " ", "0", "\r\n"),
            ("x", "", "\t", "7", " \t\n \t\n"),
            ("x", " ", "", "8", "\r\t\r"),
            ("x", "\t", "", "9", " \r\n \t\r\n"),
        ];
        for (name, before, after, digits, footer) in cases {
            let source = format!("{name}{before}={after}{digits}{footer}");
            let start = u32::try_from(name.len() + before.len() + 1 + after.len()).unwrap();
            let end = start + u32::try_from(digits.len()).unwrap();
            let actual = parse_with_source_id(&source, source_id);
            assert!(
                actual.diagnostics.is_empty(),
                "{source:?}: {:?}",
                actual.diagnostics
            );
            assert_eq!(
                actual.program.items,
                vec![Item::Stmt(Stmt::Assign {
                    name: Ident {
                        name: name.to_string(),
                        span: Span::new(source_id, 0, u32::try_from(name.len()).unwrap())
                    },
                    value: Expr::Int {
                        value: previous(digits).unwrap(),
                        span: Span::new(source_id, start, end)
                    },
                    is_var: false,
                    is_varip: false,
                    annotation: None,
                    span: Span::new(source_id, 0, end),
                })],
                "{source:?}"
            );
        }
    }

    #[test]
    fn actual_overflow_diagnostic_and_recovery() {
        let source_id = SourceId(91);
        for digits in ["9223372036854775808", "999999999999999999999999999999"] {
            for footer in ["", "\n", "\r", "\r\n"] {
                let source = format!("x = {digits}{footer}");
                let end = 4 + u32::try_from(digits.len()).unwrap();
                let actual = parse_with_source_id(&source, source_id);
                assert!(actual.program.items.is_empty(), "{source:?}");
                assert_eq!(
                    actual.diagnostics,
                    vec![Diagnostic::error("E0110", "integer literal out of range")
                        .with_label(Span::new(source_id, 4, end), "unexpected here")],
                    "{source:?}"
                );
            }
        }
        let source = "bad=9223372036854775808\ngood=9007199254740993";
        let actual = parse_with_source_id(source, source_id);
        assert_eq!(actual.diagnostics.len(), 1);
        assert_eq!(actual.program.items.len(), 1);
        let Item::Stmt(Stmt::Assign {
            name,
            value: Expr::Int { value, .. },
            ..
        }) = &actual.program.items[0]
        else {
            panic!("expected recovered exact integer assignment");
        };
        assert_eq!(name.name, "good");
        assert_eq!(*value, 9_007_199_254_740_993);
    }

    #[test]
    fn actual_float_classification_is_unchanged() {
        for (text, expected) in [
            ("1.0", 1.0_f64),
            ("1e2", 100.0),
            ("1E2", 100.0),
            ("8.", 8.0),
        ] {
            let actual = parse_with_source_id(&format!("x={text}"), SourceId(18));
            assert!(actual.diagnostics.is_empty());
            let Item::Stmt(Stmt::Assign {
                value: Expr::Float { value, .. },
                ..
            }) = &actual.program.items[0]
            else {
                panic!("float classification changed for {text}");
            };
            assert_eq!(value.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn actual_unary_min_keeps_positive_magnitude_refusal() {
        // The sign is a separate unary token, so the positive magnitude is
        // refused, as the assertion below checks.
        let source_id = SourceId(36);
        let actual = parse_with_source_id("x=-9223372036854775808", source_id);
        assert!(actual.program.items.is_empty());
        assert_eq!(
            actual.diagnostics,
            vec![Diagnostic::error("E0110", "integer literal out of range")
                .with_label(Span::new(source_id, 3, 22), "unexpected here")]
        );
    }
}
