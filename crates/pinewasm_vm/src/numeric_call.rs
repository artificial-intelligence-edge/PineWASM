//! Scalar calls evaluated directly on the operand stack. The ordinary call
//! handler still handles other types, arities and effects.
//!
//! These cases inspect no strings, create none, and charge no extra work.
//! Keeping the arguments in place avoids initializing and dropping the eight
//! cells of the general call buffer on every inner-loop math operation.

use pinewasm_hir::PureFunc;
use pinewasm_mir::Value;

#[inline]
// The outer option selects the general handler; the inner option is Pine na.
#[allow(clippy::option_option)]
fn number(value: &Value) -> Option<Option<f64>> {
    match value {
        Value::Na => Some(None),
        Value::Int(value) => Some(Some(*value as f64)),
        Value::Float(value) => Some(Some(*value)),
        _ => None,
    }
}

/// `None` delegates to the general handler, including all type errors. A
/// successful result uses the same numeric kernels and na policy as that
/// handler, without moving or cloning any argument.
#[inline]
pub(crate) fn evaluate(func: PureFunc, args: &[Value]) -> Option<Value> {
    match (func, args) {
        (PureFunc::MathAbs, [value]) => Some(match value {
            Value::Na => Value::Na,
            Value::Int(value) => value.checked_abs().map_or(Value::Na, Value::Int),
            Value::Float(value) => Value::from_f64(value.abs()),
            _ => return None,
        }),
        (PureFunc::CastBool, [value]) => Some(Value::Bool(match value {
            Value::Na => false,
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            _ => return None,
        })),
        (PureFunc::MathPow, [base, exponent]) => {
            // Inspect both operands even when one is na. A malformed second
            // operand must still reach the general handler's type error.
            let (base, exponent) = (number(base)?, number(exponent)?);
            Some(match (base, exponent) {
                (Some(base), Some(exponent)) => {
                    Value::from_f64(pinewasm_num::raw::powf(base, exponent))
                }
                _ => Value::Na,
            })
        }
        (func, [value]) => {
            // Choose the function before converting the argument: unsupported
            // calls always delegate, including stateful and string calls.
            let kernel: fn(f64) -> f64 = match func {
                PureFunc::MathSqrt => f64::sqrt,
                PureFunc::MathExp => pinewasm_num::raw::exp,
                PureFunc::MathLog => pinewasm_num::raw::ln,
                PureFunc::MathLog10 => pinewasm_num::raw::log10,
                PureFunc::MathSin => pinewasm_num::raw::sin,
                PureFunc::MathCos => pinewasm_num::raw::cos,
                PureFunc::MathTan => pinewasm_num::raw::tan,
                PureFunc::MathAsin => pinewasm_num::raw::asin,
                PureFunc::MathAcos => pinewasm_num::raw::acos,
                PureFunc::MathAtan => pinewasm_num::raw::atan,
                PureFunc::MathToDegrees => pinewasm_num::raw::to_degrees,
                PureFunc::MathToRadians => pinewasm_num::raw::to_radians,
                _ => return None,
            };
            Some(number(value)?.map_or(Value::Na, |value| Value::from_f64(kernel(value))))
        }
        _ => None,
    }
}
