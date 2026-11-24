//! Exact arithmetic for cumulative VM accounting admission.
//!
//! The peak here is reserved accounting bytes plus the admitted transient
//! payload. It is not a measurement or bound on the allocator's physical peak.

/// The caller builds the runtime error and its source context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservationError {
    Overflow,
    LimitExceeded(usize),
}

/// Return the new persistent reservation only if both sums fit and the
/// accounted peak is at most the configured limit. No counter is mutated here.
#[inline]
pub(crate) fn project(
    reserved: usize,
    requested: usize,
    transient: usize,
    limit: usize,
) -> Result<usize, ReservationError> {
    let attempted = match reserved.checked_add(requested) {
        Some(attempted) => attempted,
        None => return Err(ReservationError::Overflow),
    };
    let peak = match attempted.checked_add(transient) {
        Some(peak) => peak,
        None => return Err(ReservationError::Overflow),
    };
    if peak > limit {
        return Err(ReservationError::LimitExceeded(peak));
    }
    Ok(attempted)
}

/// Reserve accounted bytes through the same mutation seam used by
/// `Vm::reserve_vm_bytes`. Failure preserves the prior persistent counter and
/// its exact error payload; transient bytes count toward the peak but are never accumulated.
#[inline]
pub(crate) fn reserve(
    reserved: &mut usize,
    requested: usize,
    transient: usize,
    limit: usize,
) -> Result<(), ReservationError> {
    match project(*reserved, requested, transient, limit) {
        Ok(next) => {
            *reserved = next;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{project, reserve, ReservationError};

    /// Direct form of the inline arithmetic and control flow of `Vm::reserve_vm_bytes`, used as the test expectation.
    fn reference(
        reserved: usize,
        requested: usize,
        transient: usize,
        limit: usize,
    ) -> Result<usize, ReservationError> {
        let attempted = reserved
            .checked_add(requested)
            .ok_or(ReservationError::Overflow)?;
        let peak = attempted
            .checked_add(transient)
            .ok_or(ReservationError::Overflow)?;
        if peak > limit {
            return Err(ReservationError::LimitExceeded(peak));
        }
        Ok(attempted)
    }

    const EDGES: [usize; 12] = [
        0,
        1,
        2,
        7,
        127,
        128,
        511,
        512,
        usize::MAX / 2,
        usize::MAX / 2 + 1,
        usize::MAX - 1,
        usize::MAX,
    ];

    #[test]
    fn admission_matches_frozen_reference_on_boundary_grid() {
        for reserved in EDGES {
            for requested in EDGES {
                for transient in EDGES {
                    for limit in EDGES {
                        assert_eq!(
                            project(reserved, requested, transient, limit),
                            reference(reserved, requested, transient, limit),
                            "reserved={reserved}, requested={requested}, transient={transient}, limit={limit}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mutating_caller_matches_frozen_assignment_on_boundary_grid() {
        for before in EDGES {
            for requested in EDGES {
                for transient in EDGES {
                    for limit in EDGES {
                        let expected = reference(before, requested, transient, limit);
                        let mut reserved = before;
                        let actual = reserve(&mut reserved, requested, transient, limit);
                        assert_eq!(
                            actual,
                            expected.map(|_| ()),
                            "before={before}, requested={requested}, transient={transient}, limit={limit}"
                        );
                        assert_eq!(
                            reserved,
                            expected.unwrap_or(before),
                            "counter: before={before}, requested={requested}, transient={transient}, limit={limit}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mutating_caller_preserves_the_first_refused_prefix() {
        let mut reserved = 4;
        assert_eq!(reserve(&mut reserved, 3, 2, 10), Ok(()));
        assert_eq!(reserved, 7);
        assert_eq!(reserve(&mut reserved, 1, 2, 10), Ok(()));
        assert_eq!(reserved, 8);
        assert_eq!(
            reserve(&mut reserved, 1, 2, 10),
            Err(ReservationError::LimitExceeded(11))
        );
        assert_eq!(reserved, 8);
        assert_eq!(
            reserve(&mut reserved, usize::MAX, 0, usize::MAX),
            Err(ReservationError::Overflow)
        );
        assert_eq!(reserved, 8);
        assert_eq!(reserve(&mut reserved, 0, 2, 10), Ok(()));
        assert_eq!(reserved, 8);
    }

    #[test]
    fn either_overflow_precedes_the_limit_check() {
        assert_eq!(
            project(usize::MAX, 1, 0, 0),
            Err(ReservationError::Overflow)
        );
        assert_eq!(
            project(usize::MAX - 1, 1, 1, 0),
            Err(ReservationError::Overflow)
        );
        assert_eq!(
            project(0, usize::MAX, 1, usize::MAX),
            Err(ReservationError::Overflow)
        );
    }

    #[test]
    fn equality_at_the_cap_and_machine_maximum_is_admitted() {
        assert_eq!(project(4, 3, 2, 9), Ok(7));
        assert_eq!(project(0, 0, 0, 0), Ok(0));
        assert_eq!(project(usize::MAX, 0, 0, usize::MAX), Ok(usize::MAX));
        assert_eq!(project(usize::MAX - 1, 1, 0, usize::MAX), Ok(usize::MAX));
    }

    #[test]
    fn transient_payload_counts_toward_peak_but_not_persistent_total() {
        assert_eq!(project(4, 3, 2, 8), Err(ReservationError::LimitExceeded(9)));
        assert_eq!(project(4, 3, 2, 9), Ok(7));
        assert_eq!(
            project(4, 0, 7, 10),
            Err(ReservationError::LimitExceeded(11))
        );
        assert_eq!(project(4, 0, 6, 10), Ok(4));
    }

    #[test]
    fn actual_vm_caller_preserves_errors_and_updates_counter_only_on_success() {
        use crate::{MarketData, RunOptions, RuntimeErrorKind, Vm};

        let parsed = pinewasm_parse::parse("indicator(\"Reservation\")\nplot(close)\n");
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        let mir = pinewasm_mir::lower(&analysis.hir);
        let mut program = pinewasm_csil::lower_mir(&mir).expect("test source lowers");
        program.functions[0].source_map[0] =
            pinewasm_span::Span::new(pinewasm_span::SourceId(17), 23, 29);
        if program.functions[0].source_map.len() > 1 {
            program.functions[0].source_map[1] = pinewasm_span::Span::dummy();
        }
        pinewasm_csil::validate(&program).expect("test program validates");
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![1.0],
            ..MarketData::default()
        };
        let options = RunOptions::default();
        let mut vm = Vm::new(&program, &data, &options).expect("test VM starts");
        vm.stack.push(pinewasm_mir::value::Value::Int(19));
        vm.instructions_remaining = 11;
        vm.string_bytes_allocated = 13;
        let stack_before = vm.stack.clone();
        let cases = [
            (4, 3, 2, 9),
            (4, 3, 2, 8),
            (4, 0, 7, 10),
            (4, 0, 6, 10),
            (0, 0, 0, 0),
            (usize::MAX, 1, 0, 0),
            (usize::MAX - 1, 1, 1, 0),
            (usize::MAX, 0, 0, usize::MAX),
        ];
        for pc in [0, 1, program.main().source_map.len(), usize::MAX] {
            for (reserved, requested, transient, limit) in cases {
                vm.bytes_reserved = reserved;
                vm.transient_vm_bytes = transient;
                std::sync::Arc::make_mut(&mut vm.options).max_vm_bytes_per_run = limit;
                vm.bar = 7;
                let expected = reference(reserved, requested, transient, limit);
                let result = vm.reserve_vm_bytes(requested, pc);
                match expected {
                    Ok(attempted) => {
                        assert_eq!(result, Ok(()));
                        assert_eq!(vm.bytes_reserved, attempted);
                    }
                    Err(error) => {
                        let message = match error {
                            ReservationError::Overflow => {
                                format!("VM memory accounting overflowed; limit is {limit} bytes")
                            }
                            ReservationError::LimitExceeded(peak) => format!(
                                "run exceeded the VM memory limit of {limit} bytes (attempted {peak})"
                            ),
                        };
                        assert_eq!(
                            result,
                            Err(vm.fail(RuntimeErrorKind::VmMemoryLimitExceeded, message, pc))
                        );
                        assert_eq!(vm.bytes_reserved, reserved);
                        let actual = result.as_ref().expect_err("direct model refused");
                        assert_eq!(actual.bar_index, Some(7));
                        assert_eq!(actual.instruction_offset, Some(pc as u32));
                        assert_eq!(actual.span, program.main().source_map.get(pc).copied());
                    }
                }
                assert_eq!(vm.transient_vm_bytes, transient);
                assert_eq!(vm.stack, stack_before);
                assert_eq!(vm.instructions_remaining, 11);
                assert_eq!(vm.string_bytes_allocated, 13);
                assert_eq!(vm.bar, 7);
            }
        }
    }
}
