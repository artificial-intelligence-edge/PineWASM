//! Opt-in roots for extracting the actual current derived `Value::clone`.
//!
//! The Color root filters tags without normalizing any channel. `FloatModel`
//! remains a typed marker for the producer's external F64 representation;
//! opacity of that marker does not make `Value` or its derived Clone opaque.
use crate::value::Value;

/// Typed producer marker arranging the external F64 representation import.
pub struct FloatModel;

/// Call the actual Rust-derived Clone on every Value variant.
pub fn clone_value(_model: &FloatModel, input: &Value) -> Value {
    input.clone()
}

/// Clone Color with all four raw channels, excluding every other Value tag.
pub fn color_clone(_model: &FloatModel, input: &Value) -> Option<Value> {
    match input {
        Value::Color { .. } => Some(input.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::HandleId;

    #[test]
    fn actual_derived_color_clone() {
        let model = FloatModel;
        for (r, g, b, t) in [
            (0, 0, 0, 0.0),
            (255, 0, 0, 0.0),
            (0, 255, 0, 0.0),
            (0, 0, 255, 0.0),
            (0, 0, 0, 100.0),
            (255, 128, 1, 100.0),
            (17, 29, 43, 67.0),
            (17, 29, 43, 49.6),
            // Raw representation beyond runtime transparency <= 100.
            (1, 2, 3, 255.0),
        ] {
            let input = Value::Color {
                r,
                g,
                b,
                t,
                input_origin: None,
            };
            assert_eq!(clone_value(&model, &input), input);
            assert_eq!(color_clone(&model, &input), Some(input.clone()));
        }
        let input = Value::Color {
            r: 17,
            g: 29,
            b: 43,
            t: 67.0,
            input_origin: None,
        };
        for wrong in [
            Value::Na,
            Value::String("color".into()),
            Value::Ref(HandleId(0)),
            Value::Color {
                r: 29,
                g: 17,
                b: 43,
                t: 67.0,
                input_origin: None,
            },
            Value::Color {
                r: 17,
                g: 29,
                b: 43,
                t: 0.0,
                input_origin: None,
            },
        ] {
            assert_ne!(clone_value(&model, &input), wrong);
        }
        for excluded in [
            Value::Na,
            Value::Bool(false),
            Value::Int(7),
            Value::Float(-0.0),
            Value::String("color".into()),
            Value::Ref(HandleId(0)),
        ] {
            assert!(color_clone(&model, &excluded).is_none());
        }
        for input_origin in [None, Some(0), Some(u32::MAX)] {
            let input = Value::Color {
                r: 1,
                g: 2,
                b: 3,
                t: 50.01,
                input_origin,
            };
            for actual in [
                clone_value(&model, &input),
                color_clone(&model, &input).unwrap(),
            ] {
                let Value::Color {
                    input_origin: actual_origin,
                    ..
                } = actual
                else {
                    panic!("clone changed Color tag")
                };
                assert_eq!(actual_origin, input_origin);
            }
        }
        // Ref clones the handle, not the heap object identified by it.
        assert_eq!(
            clone_value(&model, &Value::Ref(HandleId(73))),
            Value::Ref(HandleId(73))
        );
        let Value::Float(out) = clone_value(&model, &Value::Float(-0.0)) else {
            panic!("clone changed Float tag")
        };
        assert_eq!(out.to_bits(), (-0.0f64).to_bits());
    }
}
