//! Preserve the public validator's final diagnostic vector without rebuilding it.
pub(super) fn finish_errors<E>(errors: Vec<E>) -> Result<(), Vec<E>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::finish_errors;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn empty_errors_succeed() {
        let errors: Vec<String> = Vec::with_capacity(17);
        assert_eq!(finish_errors(errors), Ok(()));
    }

    #[test]
    fn nonclone_payload_order_allocation_and_drop_are_preserved() {
        struct Diagnostic {
            code: u32,
            text: String,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for Diagnostic {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        let mut errors = Vec::with_capacity(17);
        for (code, text) in [(7, "prefix χ"), (23, "second\0payload")] {
            errors.push(Diagnostic {
                code,
                text: text.into(),
                drops: Rc::clone(&drops),
            });
        }
        let pointer = errors.as_ptr();
        let capacity = errors.capacity();
        let retained = match finish_errors(errors) {
            Ok(()) => panic!("diagnostics were discarded"),
            Err(retained) => retained,
        };
        assert_eq!(drops.get(), 0);
        assert_eq!(retained.as_ptr(), pointer);
        assert_eq!(retained.capacity(), capacity);
        assert_eq!(retained.len(), 2);
        assert_eq!(
            (retained[0].code, retained[0].text.as_str()),
            (7, "prefix χ")
        );
        assert_eq!(
            (retained[1].code, retained[1].text.as_str()),
            (23, "second\0payload")
        );
        drop(retained);
        assert_eq!(drops.get(), 2);
    }
}
