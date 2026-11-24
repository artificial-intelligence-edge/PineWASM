//! Pure arithmetic used by the concrete reference storage and write counters.
//! These counts are admission estimates, not allocator or RSS measurements.

pub(super) fn checked_span_storage(capacity: usize, element_size: usize) -> Option<usize> {
    capacity.checked_mul(element_size)
}

pub(super) fn checked_counter_step(current: usize, chunk_length: usize) -> Option<usize> {
    current
        .checked_add(chunk_length)
        .filter(|size| *size <= crate::MAX_BUDGET_REFERENCE_BYTES)
}

/// Private streaming counter; refusal leaves the previous count unchanged.
pub(super) struct Counter(pub(super) usize);

pub(super) fn checked_writer_transition(
    counter: &mut Counter,
    chunk_length: usize,
) -> Option<usize> {
    let next = checked_counter_step(counter.0, chunk_length)?;
    counter.0 = next;
    Some(chunk_length)
}

/// Count the byte slice presented to the streaming writer.
pub(super) fn checked_writer_bytes_transition(
    counter: &mut Counter,
    bytes: &[u8],
) -> Option<usize> {
    checked_writer_transition(counter, bytes.len())
}

impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        checked_writer_bytes_transition(self, bytes)
            .ok_or_else(|| std::io::Error::other("budget reference exceeds size limit"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BudgetReference, CsilInstr};
    use pinewasm_span::{SourceId, Span};

    #[test]
    fn span_storage_zero_exact_and_overflow_boundaries() {
        assert_eq!(checked_span_storage(0, usize::MAX), Some(0));
        assert_eq!(checked_span_storage(usize::MAX, 0), Some(0));
        assert_eq!(checked_span_storage(usize::MAX, 1), Some(usize::MAX));
        assert_eq!(
            checked_span_storage(usize::MAX / 2, 2),
            Some(usize::MAX - 1)
        );
        assert_eq!(checked_span_storage(usize::MAX / 2 + 1, 2), None);
    }

    #[test]
    fn span_storage_matches_original_checked_mul() {
        let values = [
            0,
            1,
            2,
            12,
            crate::MAX_BUDGET_REFERENCE_BYTES,
            usize::MAX / 2,
            usize::MAX,
        ];
        for capacity in values {
            for element_size in values {
                assert_eq!(
                    checked_span_storage(capacity, element_size),
                    capacity.checked_mul(element_size)
                );
            }
        }
    }

    #[test]
    fn counter_zero_exact_cap_one_past_and_overflow() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        assert_eq!(checked_counter_step(0, 0), Some(0));
        assert_eq!(checked_counter_step(0, cap), Some(cap));
        assert_eq!(checked_counter_step(cap, 0), Some(cap));
        assert_eq!(checked_counter_step(cap - 1, 1), Some(cap));
        assert_eq!(checked_counter_step(cap, 1), None);
        assert_eq!(checked_counter_step(cap + 1, 0), None);
        assert_eq!(checked_counter_step(usize::MAX, 1), None);
    }

    #[test]
    fn counter_step_matches_original_checked_add_and_filter() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        let values = [0, 1, cap - 1, cap, cap + 1, usize::MAX];
        for current in values {
            for chunk_length in values {
                let original = current
                    .checked_add(chunk_length)
                    .filter(|size| *size <= cap);
                assert_eq!(checked_counter_step(current, chunk_length), original);
            }
        }
    }

    #[test]
    fn counter_sequence_rejects_later_chunk_without_committing_it() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        let mut current = 0;
        for chunk in [0, cap - 1, 1] {
            current = checked_counter_step(current, chunk).expect("within cap");
        }
        let before = current;
        if let Some(next) = checked_counter_step(current, 1) {
            current = next;
        }
        assert_eq!(current, before);
        assert_eq!(current, cap);
        assert_eq!(checked_counter_step(current, 1), None);
    }

    #[test]
    fn actual_reference_storage_uses_reserved_capacities_for_unit_opcodes() {
        for instruction in [CsilInstr::PushNa, CsilInstr::Pop, CsilInstr::Return] {
            let mut code = Vec::with_capacity(5);
            code.push(instruction);
            let mut source_map = Vec::with_capacity(7);
            source_map.push(Span::new(SourceId::MAIN, 0, 1));
            assert!(code.capacity() > code.len());
            assert!(source_map.capacity() > source_map.len());
            let expected = code
                .capacity()
                .checked_mul(std::mem::size_of::<CsilInstr>())
                .unwrap()
                .checked_add(
                    source_map
                        .capacity()
                        .checked_mul(std::mem::size_of::<Span>())
                        .unwrap(),
                );
            let reference = BudgetReference {
                version: 2,
                code,
                source_map,
                max_stack: 1,
            };
            assert_eq!(
                crate::budget_reference_allocated_bytes(&reference),
                expected
            );
        }
    }

    #[test]
    fn actual_within_limit_matches_original_counter_on_small_references() {
        struct OriginalCounter(usize);
        impl std::io::Write for OriginalCounter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|size| *size <= crate::MAX_BUDGET_REFERENCE_BYTES)
                    .ok_or_else(|| std::io::Error::other("budget reference exceeds size limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for version in [0, 1, 2, u32::MAX] {
            for max_stack in [0, 1, u32::MAX] {
                let reference = BudgetReference {
                    version,
                    code: vec![CsilInstr::PushNa, CsilInstr::Pop, CsilInstr::Return],
                    source_map: vec![Span::new(SourceId::MAIN, 0, u32::MAX); 3],
                    max_stack,
                };
                let original = crate::budget_reference_allocated_bytes(&reference)
                    .is_some_and(|size| size <= crate::MAX_BUDGET_REFERENCE_BYTES)
                    && serde_json::to_writer(OriginalCounter(0), &reference).is_ok();
                assert_eq!(crate::budget_reference_within_limit(&reference), original);
            }
        }
    }

    #[test]
    fn actual_encoded_limit_rejects_spans_only_reference_with_small_resident_count() {
        struct CountWrites(usize);
        impl std::io::Write for CountWrites {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        let reference = BudgetReference {
            version: 2,
            code: Vec::new(),
            source_map: vec![Span::new(SourceId(u32::MAX), u32::MAX, u32::MAX); cap / 32 + 1],
            max_stack: u32::MAX,
        };
        let resident =
            crate::budget_reference_allocated_bytes(&reference).expect("resident arithmetic fits");
        assert!(resident <= cap);
        let mut encoded = CountWrites(0);
        serde_json::to_writer(&mut encoded, &reference).expect("streamed size measurement");
        assert!(encoded.0 > cap);
        assert!(!crate::budget_reference_within_limit(&reference));
        // Deliberately mismatched code/span lengths: this tests only size
        // admission, not validator acceptance. No encoded Vec is allocated.
    }
}

#[cfg(test)]
mod writer_transition_tests {
    use super::{checked_writer_bytes_transition, checked_writer_transition, Counter};
    use std::io::Write;

    #[test]
    fn exact_cap_success_returns_length_and_updates_state() {
        let mut counter = Counter(crate::MAX_BUDGET_REFERENCE_BYTES - 1);
        assert_eq!(checked_writer_transition(&mut counter, 1), Some(1));
        assert_eq!(counter.0, crate::MAX_BUDGET_REFERENCE_BYTES);
    }

    #[test]
    fn over_cap_and_overflow_refusals_preserve_state() {
        for initial in [crate::MAX_BUDGET_REFERENCE_BYTES, usize::MAX] {
            let mut counter = Counter(initial);
            assert_eq!(checked_writer_transition(&mut counter, 1), None);
            assert_eq!(counter.0, initial);
        }
    }

    #[test]
    fn empty_chunk_at_cap_succeeds_but_above_cap_refuses() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        let mut at_cap = Counter(cap);
        assert_eq!(checked_writer_transition(&mut at_cap, 0), Some(0));
        assert_eq!(at_cap.0, cap);
        let mut above_cap = Counter(cap + 1);
        assert_eq!(checked_writer_transition(&mut above_cap, 0), None);
        assert_eq!(above_cap.0, cap + 1);
    }

    #[test]
    fn actual_write_preserves_error_message_state_and_flush() {
        let mut counter = Counter(crate::MAX_BUDGET_REFERENCE_BYTES);
        let error = counter.write(&[7]).expect_err("over cap");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "budget reference exceeds size limit");
        assert_eq!(counter.0, crate::MAX_BUDGET_REFERENCE_BYTES);
        counter.flush().expect("flush succeeds");
        assert_eq!(counter.0, crate::MAX_BUDGET_REFERENCE_BYTES);
    }

    #[test]
    fn actual_write_partition_preserves_total_and_return_lengths() {
        let mut whole = Counter(0);
        let mut split = Counter(0);
        assert_eq!(whole.write(b"abcdef").expect("whole"), 6);
        for chunk in [b"ab".as_slice(), b"".as_slice(), b"cdef".as_slice()] {
            assert_eq!(split.write(chunk).expect("chunk"), chunk.len());
        }
        assert_eq!(whole.0, 6);
        assert_eq!(split.0, whole.0);
    }

    #[test]
    fn byte_slice_transition_counts_raw_bytes_at_the_cap() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        let bytes = [0xC3, 0xA9, 0x00];
        let mut counter = Counter(cap - bytes.len());
        assert_eq!(
            checked_writer_bytes_transition(&mut counter, &bytes),
            Some(3)
        );
        assert_eq!(counter.0, cap);

        let error = counter
            .write(&[0xFF])
            .expect_err("one more byte exceeds cap");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(counter.0, cap);
    }

    #[test]
    fn actual_write_matches_original_transition_at_boundaries() {
        let cap = crate::MAX_BUDGET_REFERENCE_BYTES;
        for initial in [0, cap - 1, cap, cap + 1, usize::MAX] {
            for chunk in [b"".as_slice(), b"x".as_slice(), b"xy".as_slice()] {
                let expected = initial.checked_add(chunk.len()).filter(|n| *n <= cap);
                let mut counter = Counter(initial);
                let result = counter.write(chunk);
                match expected {
                    Some(next) => {
                        assert_eq!(result.expect("success"), chunk.len());
                        assert_eq!(counter.0, next);
                    }
                    None => {
                        assert!(result.is_err());
                        assert_eq!(counter.0, initial);
                    }
                }
            }
        }
    }
}
