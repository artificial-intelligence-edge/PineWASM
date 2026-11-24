//! Exact scalar-literal spelling for the first proof-wire profile.
//!
//! This is an opt-in building block, not a complete CSIL proof artifact.
//! It starts from typed instructions and never routes `i64` or `f64` through
//! an ordinary JSON number before choosing the proof representation.

use crate::CsilInstr;
use serde::Serialize;

/// Version-1 scalar literal form. Float bits use exactly sixteen lower-case
/// hexadecimal digits; signed integers use their canonical decimal spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "tag", rename_all = "snake_case")]
pub enum ProofLiteral {
    Na,
    Bool { value: bool },
    I64 { decimal: String },
    F64 { bits: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProofLiteralError {
    #[error("opcode is not an admitted scalar literal")]
    UnsupportedOpcode,
    #[error("non-finite f64 is outside the first scalar profile")]
    NonFiniteFloat,
}

/// The exact sixteen lower-case ASCII digits of a raw 64-bit payload.
///
/// This numeric production kernel is separate from native String construction;
/// leading zeros are retained, and non-finite bit patterns are valid inputs here.
pub(crate) fn hex64_ascii_digits(mut bits: u64) -> [u8; 16] {
    let mut digits = [0_u8; 16];
    let mut index = 16;
    while index > 0 {
        index -= 1;
        let nibble = (bits & 15) as u8;
        digits[index] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        bits >>= 4;
    }
    digits
}

fn hex64_string(bits: u64) -> String {
    let digits = hex64_ascii_digits(bits);
    let mut rendered = String::with_capacity(16);
    for digit in digits {
        rendered.push(char::from(digit));
    }
    rendered
}

/// Canonical signed decimal ASCII in the suffix `digits[start..20]`.
///
/// Numeric kernel only: zero has one digit, and signed MIN is handled without
/// negating it in signed arithmetic. String resources are a separate boundary.
pub(crate) fn i64_ascii_digits(value: i64) -> ([u8; 20], usize) {
    let mut magnitude = if value < 0 {
        (-(value + 1)) as u64 + 1
    } else {
        value as u64
    };
    let mut digits = [0_u8; 20];
    let mut index = 20;
    while magnitude >= 10 {
        index -= 1;
        digits[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
    }
    index -= 1;
    digits[index] = b'0' + magnitude as u8;
    if value < 0 {
        index -= 1;
        digits[index] = b'-';
    }
    (digits, index)
}

fn i64_string(value: i64) -> String {
    let (digits, mut index) = i64_ascii_digits(value);
    let mut rendered = String::with_capacity(20);
    while index < 20 {
        rendered.push(char::from(digits[index]));
        index += 1;
    }
    rendered
}

impl TryFrom<&CsilInstr> for ProofLiteral {
    type Error = ProofLiteralError;

    fn try_from(instr: &CsilInstr) -> Result<Self, Self::Error> {
        match instr {
            CsilInstr::PushNa => Ok(Self::Na),
            CsilInstr::PushBool(value) => Ok(Self::Bool { value: *value }),
            CsilInstr::PushI64(value) => Ok(Self::I64 {
                decimal: i64_string(*value),
            }),
            CsilInstr::PushF64(value) if value.get().is_finite() => Ok(Self::F64 {
                bits: hex64_string(value.to_bits()),
            }),
            CsilInstr::PushF64(_) => Err(ProofLiteralError::NonFiniteFloat),
            _ => Err(ProofLiteralError::UnsupportedOpcode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copy of the direct match, kept as the expected side. It uses the standard
    // Rust formatter and never calls the digit/string helpers.
    fn original_literal(instr: &CsilInstr) -> Result<ProofLiteral, ProofLiteralError> {
        match instr {
            CsilInstr::PushNa => Ok(ProofLiteral::Na),
            CsilInstr::PushBool(value) => Ok(ProofLiteral::Bool { value: *value }),
            CsilInstr::PushI64(value) => Ok(ProofLiteral::I64 {
                decimal: value.to_string(),
            }),
            CsilInstr::PushF64(value) if value.get().is_finite() => Ok(ProofLiteral::F64 {
                bits: format!("{:016x}", value.to_bits()),
            }),
            CsilInstr::PushF64(_) => Err(ProofLiteralError::NonFiniteFloat),
            _ => Err(ProofLiteralError::UnsupportedOpcode),
        }
    }

    fn check_raw_bits(bits: u64) {
        let expected = format!("{bits:016x}");
        let actual = hex64_ascii_digits(bits);
        assert_eq!(
            actual.as_slice(),
            expected.as_bytes(),
            "raw payload {bits:016x}"
        );
        assert!(actual
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)));
        assert_eq!(hex64_string(bits), expected);
    }

    fn check_literal(instr: CsilInstr) {
        let expected = original_literal(&instr);
        let actual = ProofLiteral::try_from(&instr);
        assert_eq!(actual, expected, "typed instruction {instr:?}");
        if let (Ok(actual), Ok(expected)) = (actual, expected) {
            assert_eq!(
                serde_json::to_string(&actual).unwrap(),
                serde_json::to_string(&expected).unwrap()
            );
        }
    }

    #[test]
    fn numeric_hex_digits_match_original_formatter_systematically() {
        for bits in 0..=u16::MAX {
            check_raw_bits(u64::from(bits));
        }
        for shift in (0..64).step_by(4) {
            for nibble in 0..16_u64 {
                check_raw_bits(nibble << shift);
                check_raw_bits(!(nibble << shift));
            }
        }
        for shift in 0..64 {
            check_raw_bits(1_u64 << shift);
            check_raw_bits(!(1_u64 << shift));
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            check_raw_bits(state);
        }
    }

    #[test]
    fn production_literal_matches_original_at_ieee_boundaries() {
        for bits in [
            0,
            0x8000_0000_0000_0000,
            1,
            0x8000_0000_0000_0001,
            0x000f_ffff_ffff_ffff,
            0x800f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x8010_0000_0000_0000,
            0x3fef_ffff_ffff_ffff,
            0x3ff0_0000_0000_0000,
            0x3ff0_0000_0000_0001,
            0x7fef_ffff_ffff_ffff,
            0xffef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff0_0000_0000_0001,
            0xfff0_0000_0000_0001,
            0x7ff8_0000_0000_0001,
            0xfff8_0000_0000_0001,
            u64::MAX,
        ] {
            check_raw_bits(bits);
            check_literal(CsilInstr::PushF64(crate::F64Bits::new(f64::from_bits(
                bits,
            ))));
        }
    }

    #[test]
    fn production_literal_matches_original_across_exponents_and_payloads() {
        for sign in [0, 1_u64 << 63] {
            for exponent in 0..2048_u64 {
                for fraction in [0, 1, 0x0005_5555_5555_5555, 0x000f_ffff_ffff_ffff] {
                    let bits = sign | (exponent << 52) | fraction;
                    check_literal(CsilInstr::PushF64(crate::F64Bits::new(f64::from_bits(
                        bits,
                    ))));
                }
            }
        }
    }

    #[test]
    fn unchanged_other_literals_and_refusal_messages() {
        for instr in [
            CsilInstr::PushNa,
            CsilInstr::PushBool(false),
            CsilInstr::PushBool(true),
            CsilInstr::PushI64(i64::MIN),
            CsilInstr::PushI64(0),
            CsilInstr::PushI64(i64::MAX),
            CsilInstr::Return,
        ] {
            check_literal(instr);
        }
        assert_eq!(
            ProofLiteralError::NonFiniteFloat.to_string(),
            "non-finite f64 is outside the first scalar profile"
        );
        assert_eq!(
            ProofLiteralError::UnsupportedOpcode.to_string(),
            "opcode is not an admitted scalar literal"
        );
    }

    fn check_signed_digits(value: i64) {
        let expected = value.to_string();
        let (digits, start) = i64_ascii_digits(value);
        assert!(start < 20, "suffix start for {value}");
        let selected = &digits[start..];
        assert_eq!(selected, expected.as_bytes(), "numeric suffix for {value}");
        assert_eq!(i64_string(value), expected, "assembled text for {value}");
        assert!(digits[..start].iter().all(|byte| *byte == 0));
        assert!(selected.is_ascii());
        let magnitude = if value < 0 {
            assert_eq!(selected[0], b'-');
            &selected[1..]
        } else {
            selected
        };
        assert!(magnitude.iter().all(u8::is_ascii_digit));
        assert!(magnitude.len() == 1 || magnitude[0] != b'0');
        if value == 0 {
            assert_eq!(selected, b"0");
        }
    }

    #[test]
    fn signed_decimal_numeric_matches_formatter_systematically() {
        for value in -65_536..=65_535 {
            check_signed_digits(value);
        }
        for bit in 0..64 {
            check_signed_digits((1_u64 << bit) as i64);
            check_signed_digits(!(1_u64 << bit) as i64);
        }
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            check_signed_digits(state as i64);
        }
    }

    #[test]
    fn signed_decimal_production_matches_original_at_boundaries() {
        for value in [
            i64::MIN,
            i64::MIN + 1,
            -1,
            0,
            1,
            i64::MAX - 1,
            i64::MAX,
            -(1_i64 << 53) - 1,
            -(1_i64 << 53),
            -(1_i64 << 53) + 1,
            (1_i64 << 53) - 1,
            1_i64 << 53,
            (1_i64 << 53) + 1,
        ] {
            check_signed_digits(value);
            check_literal(CsilInstr::PushI64(value));
        }
        let mut power = 1_i64;
        for _ in 0..19 {
            for value in [power - 1, power, power + 1] {
                check_signed_digits(value);
                check_signed_digits(-value);
                check_literal(CsilInstr::PushI64(value));
                check_literal(CsilInstr::PushI64(-value));
            }
            if let Some(next) = power.checked_mul(10) {
                power = next;
            }
        }
    }

    // Output equality cannot show which code produced the output. This
    // separate source-binding check asserts that the production code is reachable.
    #[test]
    fn signed_decimal_production_source_binding() {
        let source = include_str!("proof_literal.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("decimal: i64_string(*value),"));
        assert!(production.contains("let (digits, mut index) = i64_ascii_digits(value);"));
    }

    // Bounded native timing check only: no allocation/panic identity or stable
    // performance guarantee follows from these timings on generated values.
    #[test]
    fn signed_decimal_native_cost_observation() {
        use std::{hint::black_box, time::Instant};
        let mut state = 0x1319_8a2e_0370_7344_u64;
        let mut values = vec![i64::MIN, i64::MAX, 0, -1, 1];
        for _ in 0..2048 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            values.push(state as i64);
        }
        let mut differing_capacities = 0;
        for &value in &values {
            let old = value.to_string();
            let new = i64_string(value);
            assert_eq!(old, new);
            differing_capacities += usize::from(old.capacity() != new.capacity());
        }
        let start = Instant::now();
        let mut old_length = 0;
        for _ in 0..16 {
            for &value in &values {
                old_length += black_box(black_box(value).to_string()).len();
            }
        }
        let old_elapsed = start.elapsed();
        let start = Instant::now();
        let mut new_length = 0;
        for _ in 0..16 {
            for &value in &values {
                new_length += black_box(i64_string(black_box(value))).len();
            }
        }
        let new_elapsed = start.elapsed();
        assert_eq!(old_length, new_length);
        eprintln!("signed-decimal timing: values={} rounds=16 old_ns={} new_ns={} differing_capacities={} total_bytes={}",
            values.len(), old_elapsed.as_nanos(), new_elapsed.as_nanos(), differing_capacities, old_length);
    }

    #[test]
    fn typed_literals_have_exact_proof_spellings() {
        let cases = [
            (CsilInstr::PushNa, r#"{"tag":"na"}"#),
            (CsilInstr::PushBool(true), r#"{"tag":"bool","value":true}"#),
            (
                CsilInstr::PushI64(i64::MIN),
                r#"{"tag":"i64","decimal":"-9223372036854775808"}"#,
            ),
            (
                CsilInstr::PushI64(i64::MAX),
                r#"{"tag":"i64","decimal":"9223372036854775807"}"#,
            ),
            (
                CsilInstr::PushF64(crate::F64Bits::new(-0.0)),
                r#"{"tag":"f64","bits":"8000000000000000"}"#,
            ),
            (
                CsilInstr::PushF64(crate::F64Bits::new(f64::from_bits(1))),
                r#"{"tag":"f64","bits":"0000000000000001"}"#,
            ),
        ];
        for (instr, expected) in cases {
            let literal = ProofLiteral::try_from(&instr).expect("admitted literal");
            assert_eq!(serde_json::to_string(&literal).unwrap(), expected);
        }
    }

    #[test]
    fn unsupported_and_nonfinite_literals_fail_closed() {
        assert_eq!(
            ProofLiteral::try_from(&CsilInstr::PushF64(crate::F64Bits::new(f64::INFINITY))),
            Err(ProofLiteralError::NonFiniteFloat)
        );
        assert_eq!(
            ProofLiteral::try_from(&CsilInstr::PushF64(crate::F64Bits::new(f64::NAN))),
            Err(ProofLiteralError::NonFiniteFloat)
        );
        assert_eq!(
            ProofLiteral::try_from(&CsilInstr::Return),
            Err(ProofLiteralError::UnsupportedOpcode)
        );
    }
}
