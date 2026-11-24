//! Bounded operand-stack mutation shared by the interpreter and its Aeneas root.

/// Append one value if the logical stack-depth limit permits it.
///
/// On refusal, the input stack is unchanged. The VM wrapper constructs the
/// source-located `StackOverflow` error; this helper only captures the actual
/// stack transition.
#[inline(always)]
pub(crate) fn push_within_limit<T>(stack: &mut Vec<T>, max_stack_depth: usize, value: T) -> bool {
    if stack.len() >= max_stack_depth {
        return false;
    }
    stack.push(value);
    true
}

#[cfg(test)]
mod tests {
    use super::push_within_limit;
    use pinewasm_mir::Value;

    #[test]
    fn appends_color_and_preserves_the_existing_prefix() {
        let prefix = Value::Int(41);
        let color = Value::Color {
            r: 17,
            g: 83,
            b: 229,
            t: 37.5,
            input_origin: None,
        };
        let mut stack = vec![prefix.clone()];

        assert!(push_within_limit(&mut stack, 2, color.clone()));
        assert_eq!(stack, vec![prefix, color]);
    }

    #[test]
    fn refuses_exact_and_over_limit_without_mutating_the_stack() {
        let at_limit = vec![Value::Int(1)];
        let over_limit = vec![Value::Int(1), Value::Int(2)];

        for mut stack in [at_limit.clone(), over_limit.clone()] {
            let before = stack.clone();
            assert!(!push_within_limit(&mut stack, 1, Value::Bool(true)));
            assert_eq!(stack, before);
        }

        let mut empty = Vec::new();
        assert!(!push_within_limit(&mut empty, 0, Value::Bool(false)));
        assert!(empty.is_empty());
    }
}
