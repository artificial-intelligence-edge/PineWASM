//! Successful float arithmetic without moving two tagged operands through
//! temporary values and pushing the result back. Malformed operands and stack
//! limits keep the general handler's exact pop and error order.

use crate::vm_ops::{float_arith, FloatArithOp};
use pinewasm_mir::Value;

/// Replace the top two cells by one result. A short stack is unchanged.
#[inline(always)]
pub(crate) fn replace_pair<T>(stack: &mut Vec<T>, result: T) -> bool {
    let len = stack.len();
    if len < 2 {
        false
    } else {
        stack[len - 2] = result;
        let _ = stack.pop();
        true
    }
}

#[inline(always)]
#[allow(clippy::option_option)]
fn operand(value: &Value) -> Option<Option<f64>> {
    match value {
        Value::Na => Some(None),
        Value::Float(value) => Some(Some(*value)),
        _ => None,
    }
}

/// Inspect both operands before changing the stack; even `na` must not hide a
/// malformed operand. Use exactly the existing floating-point kernel.
#[inline(always)]
pub(crate) fn evaluate(op: FloatArithOp, stack: &mut Vec<Value>, limit: usize) -> bool {
    let len = stack.len();
    if len < 2 || len - 2 >= limit {
        return false;
    }
    let Some(lhs) = operand(&stack[len - 2]) else {
        return false;
    };
    let Some(rhs) = operand(&stack[len - 1]) else {
        return false;
    };
    replace_pair(stack, float_arith(op, lhs, rhs))
}

#[cfg(test)]
mod tests {
    use super::replace_pair;

    #[test]
    fn replacement_preserves_prefix_and_short_stacks() {
        for stack in [vec![], vec![1], vec![1, 2], vec![1, 2, 3], vec![1, 2, 3, 4]] {
            let mut actual = stack.clone();
            let changed = replace_pair(&mut actual, 99);
            if stack.len() < 2 {
                assert!(!changed);
                assert_eq!(actual, stack);
            } else {
                let mut expected = stack[..stack.len() - 2].to_vec();
                expected.push(99);
                assert!(changed);
                assert_eq!(actual, expected);
            }
        }
    }
}
