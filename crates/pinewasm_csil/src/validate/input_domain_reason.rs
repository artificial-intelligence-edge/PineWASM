//! Owned input-domain classification used by native declaration validation.
//!
//! Message selection remains separate from the classifier so a selected
//! classifier root does not return a borrowed static string.

use pinewasm_hir::InputValue;

pub(super) enum InputDomainReason {
    NonFiniteFloat,
    ColorTransparencyOutOfRange,
}

pub(super) fn invalid_reason(value: &InputValue) -> Option<InputDomainReason> {
    match value {
        InputValue::Float(value) if !value.is_finite() => Some(InputDomainReason::NonFiniteFloat),
        InputValue::Color { t, .. } if !t.is_finite() || !(0.0..=100.0).contains(t) => {
            Some(InputDomainReason::ColorTransparencyOutOfRange)
        }
        _ => None,
    }
}

impl InputDomainReason {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::NonFiniteFloat => "float must be finite",
            Self::ColorTransparencyOutOfRange => "color transparency is outside 0..=100",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_hir::{BuiltinSeries, ExternalSourceRef};

    fn compare_with_original(value: &InputValue) {
        let before = super::super::invalid_input_value_domain(value);
        let after = invalid_reason(value).map(InputDomainReason::message);
        assert_eq!(after, before);
    }

    #[test]
    fn ieee_edge_patterns_preserve_original_reason_and_bits() {
        for bits in [
            0,
            0x8000_0000_0000_0000,
            1,
            0x8000_0000_0000_0001,
            0x000f_ffff_ffff_ffff,
            0x800f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x8010_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0xffef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
            0xfff8_0000_0000_0001,
            0x7ff0_0000_0000_0001,
            0xfff0_0000_0000_0001,
        ] {
            let value = InputValue::Float(f64::from_bits(bits));
            compare_with_original(&value);
            if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                assert!(matches!(
                    invalid_reason(&value),
                    Some(InputDomainReason::NonFiniteFloat)
                ));
            } else {
                assert!(invalid_reason(&value).is_none());
            }
            let InputValue::Float(after) = value else {
                unreachable!()
            };
            assert_eq!(after.to_bits(), bits);
        }
    }

    #[test]
    fn every_transparency_byte_preserves_threshold_and_ordered_rgb() {
        for t in 0..=u8::MAX {
            for (r, g, b) in [(0, 0, 0), (255, 255, 255), (1, 2, 3), (255, 0, 7)] {
                let value = InputValue::Color {
                    r,
                    g,
                    b,
                    t: f64::from(t),
                };
                compare_with_original(&value);
                if t > 100 {
                    assert!(matches!(
                        invalid_reason(&value),
                        Some(InputDomainReason::ColorTransparencyOutOfRange)
                    ));
                } else {
                    assert!(invalid_reason(&value).is_none());
                }
                let InputValue::Color {
                    r: rr,
                    g: gg,
                    b: bb,
                    t: tt,
                } = value
                else {
                    unreachable!()
                };
                assert_eq!((rr, gg, bb, tt), (r, g, b, f64::from(t)));
            }
        }
    }

    #[test]
    fn other_variants_are_not_domain_failures() {
        for value in [
            InputValue::Int(i64::MIN),
            InputValue::Int(i64::MAX),
            InputValue::Bool(false),
            InputValue::Bool(true),
            InputValue::String(String::new()),
            InputValue::String("χ".into()),
            InputValue::Source(BuiltinSeries::Close),
            InputValue::ExternalSource(ExternalSourceRef {
                script_id: "χ".into(),
                plot_slot: 9,
                plot_label: "label".into(),
            }),
            InputValue::Enum {
                name: "E".into(),
                index: 1,
                member: "first".into(),
                title: None,
            },
            InputValue::Enum {
                name: "E".into(),
                index: 1,
                member: "alias".into(),
                title: Some("χ".into()),
            },
        ] {
            compare_with_original(&value);
            assert!(invalid_reason(&value).is_none());
        }
    }

    #[test]
    fn messages_are_exact_and_reason_specific() {
        assert_eq!(
            InputDomainReason::NonFiniteFloat.message(),
            "float must be finite"
        );
        assert_eq!(
            InputDomainReason::ColorTransparencyOutOfRange.message(),
            "color transparency is outside 0..=100"
        );
    }
}
