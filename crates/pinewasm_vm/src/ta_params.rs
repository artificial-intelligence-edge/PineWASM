//! Bounded run-constant parameter resolution for the VM's TA footprint reader.
//!
//! This prepares the stack buffer only. Declaration normalization, footprint
//! arithmetic and state construction remain in their existing callers.

/// Read one already narrowed State target. Native map storage remains in the
/// dictionary; this interface exposes the exact lookup used by admission.
pub(crate) trait StateLookup {
    fn state_value(&self, state: u32, param: u32) -> Option<i64>;
}

impl StateLookup for std::collections::HashMap<pinewasm_hir::TaInputTarget, i64> {
    fn state_value(&self, state: u32, param: u32) -> Option<i64> {
        self.get(&pinewasm_hir::TaInputTarget::State { state, param })
            .copied()
    }
}

/// Read the allocation planner's State override or use the default value.
/// Both keys keep the native `as u32` behavior, including truncation on a
/// wider target. Perform exactly one lookup, even when no override is present.
pub(crate) fn state_param_value<V: StateLookup>(
    values: &V,
    state: usize,
    param: usize,
    default: i64,
) -> i64 {
    match values.state_value(state as u32, param as u32) {
        Some(value) => value,
        None => default,
    }
}

/// The parameter reader used by the bounded resolver. The native closure adapter
/// preserves each call's state, parameter index and original default value.
pub(crate) trait ParamValues {
    fn value_at(&self, state: usize, param: usize, default: i64) -> i64;
}

impl<F: Fn(usize, usize, i64) -> i64> ParamValues for F {
    fn value_at(&self, state: usize, param: usize, default: i64) -> i64 {
        self(state, param, default)
    }
}

/// Resolve every representable parameter in order. An oversized source becomes
/// an empty entry without invoking the reader, matching `Decl::new`'s fallback.
pub(crate) fn resolve_params<V: ParamValues>(
    values: &V,
    state: usize,
    source: &[i64],
) -> ([i64; pinewasm_ta::MAX_PARAMS], usize) {
    let len = if source.len() <= pinewasm_ta::MAX_PARAMS {
        source.len()
    } else {
        0
    };
    // Resolve each possible entry in order without loop-carried state.
    let p0 = if len > 0 {
        values.value_at(state, 0, source[0])
    } else {
        0
    };
    let p1 = if len > 1 {
        values.value_at(state, 1, source[1])
    } else {
        0
    };
    let p2 = if len > 2 {
        values.value_at(state, 2, source[2])
    } else {
        0
    };
    let p3 = if len > 3 {
        values.value_at(state, 3, source[3])
    } else {
        0
    };
    ([p0, p1, p2, p3], len)
}

#[cfg(test)]
mod tests {
    use super::{resolve_params, state_param_value, ParamValues, StateLookup};
    use crate::ta::TaStates;
    use pinewasm_csil::TaStateDecl;
    use pinewasm_hir::{TaFunc, TaInputTarget};
    use pinewasm_ta::{cells_for, scratch_for, Decl};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    // Test-only model of the parameter-buffer loop.
    fn previous(
        state: usize,
        source: &[i64],
        value_at: impl Fn(usize, usize, i64) -> i64,
    ) -> ([i64; pinewasm_ta::MAX_PARAMS], usize) {
        let mut params = [0; pinewasm_ta::MAX_PARAMS];
        let len = if source.len() <= params.len() {
            source.len()
        } else {
            0
        };
        for (param, value) in source.iter().take(len).enumerate() {
            params[param] = value_at(state, param, *value);
        }
        (params, len)
    }

    // Test-only model of the numeric allocation sizing. The color sidecar's
    // representation is handled outside this buffer.
    fn previous_allocation(
        decls: &[TaStateDecl],
        value_at: impl Fn(usize, usize, i64) -> i64,
    ) -> Option<usize> {
        let mut cells = 0usize;
        let mut scratch = 0usize;
        assert!(decls.iter().all(|decl| decl.func != TaFunc::ValueWhenColor));
        for (state, decl) in decls.iter().enumerate() {
            let (params, len) = previous(state, &decl.params, &value_at);
            let normalized = Decl::new(decl.func, &params[..len])
                .or_else(|| Decl::new(decl.func, &[]))
                .unwrap_or_default();
            cells = cells.checked_add(cells_for(&normalized) as usize)?;
            scratch = scratch.max(scratch_for(&normalized) as usize);
        }
        decls
            .len()
            .checked_mul(std::mem::size_of::<Decl>())?
            .checked_add(decls.len().checked_mul(pinewasm_ta::STATE_BYTES as usize)?)?
            .checked_add(cells.checked_add(scratch)?.checked_mul(8)?)
    }

    #[test]
    fn exact_zero_one_four_and_oversized_buffers() {
        for source in [
            &[][..],
            &[i64::MIN][..],
            &[i64::MIN, -1, 0, i64::MAX][..],
            &[1, 2, 3, 4, 5][..],
        ] {
            let reader = |_: usize, _: usize, default: i64| default;
            assert_eq!(
                resolve_params(&reader, 17, source),
                previous(17, source, reader)
            );
        }
        assert_eq!(resolve_params(&|_, _, v| v, 0, &[]), ([0; 4], 0));
        assert_eq!(resolve_params(&|_, _, v| v, 0, &[9]), ([9, 0, 0, 0], 1));
        assert_eq!(
            resolve_params(&|_, _, v| v, 0, &[1, 2, 3, 4, 5]),
            ([0; 4], 0)
        );
    }

    #[test]
    fn exhaustive_short_parameter_reference() {
        fn visit(source: &mut Vec<i64>, remaining: usize, count: &mut usize) {
            for state in [0, 3, usize::MAX] {
                let identity = |_: usize, _: usize, v: i64| v;
                let selected = |s: usize, p: usize, v: i64| {
                    v.wrapping_neg()
                        .wrapping_add(s as i64)
                        .wrapping_sub(p as i64)
                };
                assert_eq!(
                    resolve_params(&identity, state, source),
                    previous(state, source, identity)
                );
                assert_eq!(
                    resolve_params(&selected, state, source),
                    previous(state, source, selected)
                );
            }
            *count += 1;
            if remaining != 0 {
                for v in [i64::MIN, -1, 0, 1, i64::MAX] {
                    source.push(v);
                    visit(source, remaining - 1, count);
                    source.pop();
                }
            }
        }
        let mut count = 0;
        visit(&mut Vec::new(), 6, &mut count);
        assert_eq!(count, 19_531);
    }

    #[test]
    fn callback_arguments_order_count_and_outputs() {
        let calls = RefCell::new(Vec::new());
        let reader = |s, p, v| {
            calls.borrow_mut().push((s, p, v));
            [i64::MAX, i64::MIN, 42, -9][p]
        };
        let source = [11, 22, 33, 44];
        assert_eq!(
            resolve_params(&reader, usize::MAX, &source),
            ([i64::MAX, i64::MIN, 42, -9], 4)
        );
        assert_eq!(
            *calls.borrow(),
            vec![
                (usize::MAX, 0, 11),
                (usize::MAX, 1, 22),
                (usize::MAX, 2, 33),
                (usize::MAX, 3, 44)
            ]
        );
    }

    #[test]
    fn empty_and_oversized_sources_never_call_reader() {
        let reader = |_, _, _| -> i64 { panic!("reader must not run") };
        for source in [vec![], vec![7; 5], vec![i64::MAX; 1024]] {
            assert_eq!(resolve_params(&reader, 8, &source), ([0; 4], 0));
        }
    }

    #[test]
    fn callback_panic_keeps_exact_original_prefix() {
        for fail_at in 0..4 {
            let actual_calls = RefCell::new(Vec::new());
            let previous_calls = RefCell::new(Vec::new());
            let actual = catch_unwind(AssertUnwindSafe(|| {
                resolve_params(
                    &|s, p, v| {
                        actual_calls.borrow_mut().push((s, p, v));
                        assert_ne!(p, fail_at, "selected callback panic");
                        v
                    },
                    9,
                    &[1, 2, 3, 4],
                )
            }));
            let old = catch_unwind(AssertUnwindSafe(|| {
                previous(9, &[1, 2, 3, 4], |s, p, v| {
                    previous_calls.borrow_mut().push((s, p, v));
                    assert_ne!(p, fail_at, "selected callback panic");
                    v
                })
            }));
            assert!(actual.is_err());
            assert!(old.is_err());
            assert_eq!(*actual_calls.borrow(), *previous_calls.borrow());
            assert_eq!(actual_calls.borrow().len(), fail_at + 1);
        }
    }

    #[test]
    fn every_short_prefix_has_exact_reads_and_panic_prefix() {
        let defaults = [i64::MIN, -17, 0, i64::MAX];
        for len in 0..=4 {
            let source = &defaults[..len];
            let calls = RefCell::new(Vec::new());
            let actual = resolve_params(
                &|s: usize, p: usize, v: i64| {
                    assert!(p < len, "reader reached an absent parameter");
                    calls.borrow_mut().push((s, p, v));
                    v.wrapping_add(p as i64)
                },
                usize::MAX,
                source,
            );
            assert_eq!(
                actual,
                previous(usize::MAX, source, |_, p, v| v.wrapping_add(p as i64))
            );
            assert_eq!(
                *calls.borrow(),
                source
                    .iter()
                    .enumerate()
                    .map(|(p, v)| (usize::MAX, p, *v))
                    .collect::<Vec<_>>()
            );
            for fail_at in 0..len {
                let calls = RefCell::new(Vec::new());
                let result = catch_unwind(AssertUnwindSafe(|| {
                    resolve_params(
                        &|s: usize, p: usize, v: i64| {
                            calls.borrow_mut().push((s, p, v));
                            assert_ne!(p, fail_at, "selected short-prefix callback panic");
                            v
                        },
                        usize::MAX,
                        source,
                    )
                }));
                assert!(result.is_err());
                assert_eq!(
                    *calls.borrow(),
                    source[..=fail_at]
                        .iter()
                        .enumerate()
                        .map(|(p, v)| (usize::MAX, p, *v))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn actual_allocator_matches_frozen_reference() {
        for (index, func) in pinewasm_ta::TaFunc::ALL.iter().copied().enumerate() {
            if func == TaFunc::ValueWhenColor {
                continue;
            }
            for params in [
                vec![],
                vec![i64::MIN],
                vec![100_000, -1, 0, i64::MAX],
                vec![7; 5],
            ] {
                let decls = [TaStateDecl { func, params }];
                let reader = |s: usize, p: usize, v: i64| v.saturating_add((index + s + p) as i64);
                assert_eq!(
                    TaStates::allocation_bytes_with_params(&decls, reader),
                    previous_allocation(&decls, reader)
                );
            }
        }
    }

    #[test]
    fn actual_allocator_keeps_repeated_color_callback_order() {
        let decls = [
            TaStateDecl {
                func: TaFunc::Ema,
                params: vec![3, 4],
            },
            TaStateDecl {
                func: TaFunc::ValueWhenColor,
                params: vec![2],
            },
            TaStateDecl {
                func: TaFunc::Ema,
                params: vec![7; 5],
            },
        ];
        let actual_calls = RefCell::new(Vec::new());
        let old_calls = RefCell::new(Vec::new());
        let actual = TaStates::allocation_bytes_with_params(&decls, |s, p, v| {
            actual_calls.borrow_mut().push((s, p, v));
            v
        });
        let old_reader = |s, p, v| {
            old_calls.borrow_mut().push((s, p, v));
            v
        };
        for (s, decl) in decls.iter().enumerate() {
            previous(s, &decl.params, old_reader);
        }
        previous(1, &decls[1].params, old_reader);
        assert!(actual.is_some());
        assert_eq!(*actual_calls.borrow(), *old_calls.borrow());
        assert_eq!(
            *actual_calls.borrow(),
            vec![(0, 0, 3), (0, 1, 4), (1, 0, 2), (1, 0, 2)]
        );
    }

    #[test]
    fn explicit_reader_and_native_closure_adapter_agree() {
        struct Reader;
        impl ParamValues for Reader {
            fn value_at(&self, s: usize, p: usize, v: i64) -> i64 {
                v.wrapping_add(s as i64).wrapping_sub(p as i64)
            }
        }
        let closure = |s: usize, p: usize, v: i64| v.wrapping_add(s as i64).wrapping_sub(p as i64);
        for source in [&[][..], &[i64::MIN, 2, i64::MAX][..], &[0; 5][..]] {
            assert_eq!(
                resolve_params(&Reader, usize::MAX, source),
                resolve_params(&closure, usize::MAX, source)
            );
        }
    }

    // Test-only model of the allocation callback, used to check the allocator.
    fn previous_state_value(
        values: &HashMap<TaInputTarget, i64>,
        state: usize,
        param: usize,
        default: i64,
    ) -> i64 {
        values
            .get(&TaInputTarget::State {
                state: state as u32,
                param: param as u32,
            })
            .copied()
            .unwrap_or(default)
    }

    fn lookup_indices() -> Vec<usize> {
        let mut indices = vec![0, 1, 3, u32::MAX as usize, usize::MAX];
        if let Ok(wide) = usize::try_from(u64::from(u32::MAX) + 1) {
            indices.extend([wide, wide + 1]);
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    #[test]
    fn state_lookup_matches_frozen_callback_at_both_key_boundaries() {
        let indices = lookup_indices();
        for &state in &indices {
            for &param in &indices {
                for default in [i64::MIN, -1, 0, 1, i64::MAX] {
                    for value in [None, Some(i64::MIN), Some(-1), Some(0), Some(i64::MAX)] {
                        let mut values = HashMap::from([
                            (TaInputTarget::Sma { slot: state as u32 }, 61),
                            (TaInputTarget::Change { slot: param as u32 }, 62),
                            (
                                TaInputTarget::State {
                                    state: (state as u32).wrapping_add(1),
                                    param: param as u32,
                                },
                                63,
                            ),
                        ]);
                        if let Some(value) = value {
                            values.insert(
                                TaInputTarget::State {
                                    state: state as u32,
                                    param: param as u32,
                                },
                                value,
                            );
                        }
                        let before = values.clone();
                        assert_eq!(
                            state_param_value(&values, state, param, default),
                            previous_state_value(&values, state, param, default),
                            "state={state} param={param} default={default} value={value:?}"
                        );
                        assert_eq!(values, before, "lookup must not mutate the map");
                    }
                }
            }
        }
    }

    #[test]
    fn state_lookup_keeps_target_variant_and_both_fields_distinct() {
        let mut values = HashMap::from([
            (TaInputTarget::Sma { slot: 2 }, 70),
            (TaInputTarget::Change { slot: 2 }, 71),
            (TaInputTarget::State { state: 2, param: 3 }, i64::MIN),
            (TaInputTarget::State { state: 3, param: 2 }, 0),
            (TaInputTarget::State { state: 2, param: 2 }, i64::MAX),
        ]);
        assert_eq!(state_param_value(&values, 2, 3, 89), i64::MIN);
        assert_eq!(state_param_value(&values, 3, 2, 89), 0);
        assert_eq!(state_param_value(&values, 2, 2, 89), i64::MAX);
        assert_eq!(state_param_value(&values, 2, 0, 89), 89);
        values.insert(TaInputTarget::State { state: 2, param: 3 }, 97);
        assert_eq!(state_param_value(&values, 2, 3, 89), 97);
    }

    #[test]
    fn state_lookup_retains_native_modulo_casts_on_wide_targets() {
        if let Ok(wide) = usize::try_from(u64::from(u32::MAX) + 1) {
            let values = HashMap::from([
                (TaInputTarget::State { state: 0, param: 0 }, 17),
                (TaInputTarget::State { state: 1, param: 1 }, 19),
            ]);
            assert_eq!(state_param_value(&values, wide, 0, -1), 17);
            assert_eq!(state_param_value(&values, 0, wide, -1), 17);
            assert_eq!(state_param_value(&values, wide + 1, wide + 1, -1), 19);
            assert_eq!(state_param_value(&values, 0, 0, -1), 17);
        }
    }

    #[test]
    fn state_lookup_calls_dictionary_once_and_propagates_its_panic() {
        struct Recorded {
            calls: RefCell<Vec<(u32, u32)>>,
            value: Option<i64>,
            panic: bool,
        }
        impl StateLookup for Recorded {
            fn state_value(&self, state: u32, param: u32) -> Option<i64> {
                self.calls.borrow_mut().push((state, param));
                assert!(!self.panic, "selected State dictionary panic");
                self.value
            }
        }
        for value in [None, Some(i64::MIN), Some(0), Some(i64::MAX)] {
            let reader = Recorded {
                calls: RefCell::new(Vec::new()),
                value,
                panic: false,
            };
            assert_eq!(state_param_value(&reader, 2, 3, -91), value.unwrap_or(-91));
            assert_eq!(*reader.calls.borrow(), [(2, 3)]);
        }
        let reader = Recorded {
            calls: RefCell::new(Vec::new()),
            value: None,
            panic: true,
        };
        let error = catch_unwind(AssertUnwindSafe(|| state_param_value(&reader, 2, 3, -91)))
            .expect_err("lookup panic must propagate");
        assert_eq!(
            error.downcast_ref::<&str>(),
            Some(&"selected State dictionary panic")
        );
        assert_eq!(*reader.calls.borrow(), [(2, 3)]);
    }

    #[test]
    fn state_lookup_resolver_reads_short_prefixes_and_suppresses_oversized_sources() {
        struct Recorded(RefCell<Vec<(u32, u32)>>);
        impl StateLookup for Recorded {
            fn state_value(&self, state: u32, param: u32) -> Option<i64> {
                self.0.borrow_mut().push((state, param));
                (param != 2).then_some(1000 + i64::from(param))
            }
        }
        let source = [i64::MIN, -1, i64::MAX, 0, 7];
        for len in 0..=5 {
            let reader = Recorded(RefCell::new(Vec::new()));
            let actual = resolve_params(
                &|s, p, v| state_param_value(&reader, s, p, v),
                7,
                &source[..len],
            );
            let effective = if len <= 4 { len } else { 0 };
            let mut expected = [0; 4];
            for p in 0..effective {
                expected[p] = if p == 2 { source[p] } else { 1000 + p as i64 };
            }
            assert_eq!(actual, (expected, effective));
            assert_eq!(
                *reader.0.borrow(),
                (0..effective).map(|p| (7, p as u32)).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn state_lookup_actual_allocator_matches_frozen_native_callback() {
        let values = HashMap::from([
            (TaInputTarget::State { state: 0, param: 0 }, 11),
            (TaInputTarget::State { state: 0, param: 1 }, -7),
            (TaInputTarget::State { state: 1, param: 0 }, 3),
            (TaInputTarget::Sma { slot: 0 }, 100_000),
        ]);
        for func in TaFunc::ALL {
            for params in [vec![], vec![i64::MIN], vec![2, 0, i64::MAX, -1], vec![7; 5]] {
                let decls = [
                    TaStateDecl { func, params },
                    TaStateDecl {
                        func: TaFunc::ValueWhenColor,
                        params: vec![2],
                    },
                ];
                let actual = TaStates::allocation_bytes_with_params(&decls, |s, p, v| {
                    state_param_value(&values, s, p, v)
                });
                let expected = TaStates::allocation_bytes_with_params(&decls, |s, p, v| {
                    previous_state_value(&values, s, p, v)
                });
                assert_eq!(actual, expected, "{decls:?}");
            }
        }
        assert_eq!(
            TaStates::allocation_bytes_with_params(&[], |s, p, v| {
                state_param_value(&values, s, p, v)
            }),
            Some(0)
        );
    }
}
