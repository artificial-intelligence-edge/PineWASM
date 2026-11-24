//! Slot-history preamble shared with the extraction boundary.

/// Reset the current ring cell or carry its predecessor for a persistent slot.
/// The predecessor is read and cloned before the destination is indexed.
/// Written flags are owned by the caller and are deliberately outside this cell
/// operation.
pub(crate) fn history_cell_current<T: Clone>(
    row: &mut [T],
    persistent: bool,
    bar: usize,
    mask: usize,
    na: T,
) {
    let cur = bar & mask;
    let prev = bar.wrapping_sub(1) & mask;
    row[cur] = if persistent && bar > 0 {
        row[prev].clone()
    } else {
        na
    };
}

#[cfg(test)]
mod tests {
    use super::history_cell_current;
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::rc::Rc;

    #[test]
    fn original_assignment_matches_reset_carry_and_wrap() {
        for size in [1, 2, 512] {
            for bar in [0, 1, 2, 511, 512, 513, usize::MAX] {
                for persistent in [false, true] {
                    let mut expected: Vec<_> = (0..size).map(Some).collect();
                    let mut actual = expected.clone();
                    let mask = size - 1;
                    expected[bar & mask] = if persistent && bar > 0 {
                        expected[bar.wrapping_sub(1) & mask]
                    } else {
                        None
                    };
                    history_cell_current(&mut actual, persistent, bar, mask, None);
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    struct Tracked {
        clones: Rc<Cell<usize>>,
        panic_on_clone: bool,
    }

    impl Clone for Tracked {
        fn clone(&self) -> Self {
            self.clones.set(self.clones.get() + 1);
            assert!(!self.panic_on_clone, "predecessor clone");
            Self {
                clones: self.clones.clone(),
                panic_on_clone: false,
            }
        }
    }

    #[test]
    fn reset_does_not_clone_and_persistent_self_alias_clones_once() {
        for (persistent, bar, wanted) in [(false, 1, 0), (true, 0, 0), (true, 1, 1)] {
            let clones = Rc::new(Cell::new(0));
            let mut row = [Tracked {
                clones: clones.clone(),
                panic_on_clone: false,
            }];
            let na = Tracked {
                clones: clones.clone(),
                panic_on_clone: true,
            };
            history_cell_current(&mut row, persistent, bar, 0, na);
            assert_eq!(clones.get(), wanted);
        }
    }

    #[test]
    fn clone_precedes_bad_destination_and_clone_failure_does_not_write() {
        let clones = Rc::new(Cell::new(0));
        let mut row = [Tracked {
            clones: clones.clone(),
            panic_on_clone: false,
        }];
        let na = Tracked {
            clones: clones.clone(),
            panic_on_clone: false,
        };
        assert!(catch_unwind(AssertUnwindSafe(|| history_cell_current(
            &mut row, true, 1, 1, na
        )))
        .is_err());
        assert_eq!(
            clones.get(),
            1,
            "clone must precede invalid destination index"
        );
        row[0].panic_on_clone = true;
        let na = Tracked {
            clones: clones.clone(),
            panic_on_clone: false,
        };
        assert!(catch_unwind(AssertUnwindSafe(|| history_cell_current(
            &mut row, true, 1, 0, na
        )))
        .is_err());
        assert!(
            row[0].panic_on_clone,
            "failed clone must leave the existing cell intact"
        );
    }

    #[test]
    fn invalid_predecessor_and_empty_reset_panic_without_mutation() {
        let mut row = [Some(17)];
        assert!(catch_unwind(AssertUnwindSafe(|| history_cell_current(
            &mut row, true, 2, 1, None
        )))
        .is_err());
        assert_eq!(row, [Some(17)]);
        let mut empty: [Option<usize>; 0] = [];
        assert!(catch_unwind(AssertUnwindSafe(|| history_cell_current(
            &mut empty, false, 0, 0, None
        )))
        .is_err());
    }
}
