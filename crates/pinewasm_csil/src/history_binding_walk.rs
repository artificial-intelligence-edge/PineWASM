//! History observer and operand-specialization loops used by the lowering.
//! Generic lookup keeps `HashMap`'s behavior at the adapter boundary;
//! it does not build a second override table or change iteration order.
use crate::{history, CsilFunction, CsilInstr};
use pinewasm_hir::TaInputTarget;
use std::collections::HashMap;

pub(crate) trait Lookup {
    fn value(&self, target: TaInputTarget) -> Option<i64>;
}

#[inline]
fn lookup_ta_override<S: std::hash::BuildHasher>(
    values: &HashMap<TaInputTarget, i64, S>,
    target: TaInputTarget,
) -> Option<i64> {
    values.get(&target).copied()
}

impl Lookup for HashMap<TaInputTarget, i64> {
    #[inline]
    fn value(&self, target: TaInputTarget) -> Option<i64> {
        lookup_ta_override(self, target)
    }
}

pub(crate) fn observer<L: Lookup>(instr: &CsilInstr, values: &L) -> Option<history::DepthObserver> {
    match instr {
        CsilInstr::ReadUserSlot { slot, offset } => {
            let change = if *offset == 0 {
                None
            } else {
                values
                    .value(TaInputTarget::Change { slot: *slot })
                    .map(history::ta_override_u32)
            };
            Some(history::DepthObserver::ReadUserSlot {
                offset: *offset,
                change,
            })
        }
        CsilInstr::WindowMean { slot, length } => Some(history::DepthObserver::WindowMean {
            length: *length,
            sma: values
                .value(TaInputTarget::Sma { slot: *slot })
                .map(history::ta_override_u32),
        }),
        CsilInstr::WindowMeanDynamic { .. }
        | CsilInstr::WindowExtremumDynamic { .. }
        | CsilInstr::WindowStatisticDynamic { .. } => Some(history::DepthObserver::DynamicWindow),
        _ => None,
    }
}

// End the optional branch before borrowing the destination Vec for push.
// Keeping this join outside the walk also makes its receiver call uniform.
#[inline]
fn append_observer(
    result: &mut Vec<history::DepthObserver>,
    observation: Option<history::DepthObserver>,
) {
    let item = match observation {
        None => return,
        Some(item) => item,
    };
    result.push(item);
}

pub(crate) fn observers<L: Lookup>(
    functions: &[CsilFunction],
    values: &L,
) -> Vec<history::DepthObserver> {
    let mut result = Vec::new();
    let mut f = 0;
    while f < functions.len() {
        let mut i = 0;
        while i < functions[f].code.len() {
            let observation = observer(&functions[f].code[i], values);
            append_observer(&mut result, observation);
            i += 1;
        }
        f += 1;
    }
    result
}

/// Whether any instruction reads a user slot at a runtime offset
/// (`ReadUserSlotDynamic`), the only read `max_bars_back` bounds.
pub(crate) fn reads_slot_dynamically(functions: &[CsilFunction]) -> bool {
    let mut f = 0;
    while f < functions.len() {
        let mut i = 0;
        while i < functions[f].code.len() {
            if matches!(functions[f].code[i], CsilInstr::ReadUserSlotDynamic { .. }) {
                return true;
            }
            i += 1;
        }
        f += 1;
    }
    false
}

pub(crate) fn specialize_code<L: Lookup>(code: &mut [CsilInstr], values: &L) {
    let mut i = 0;
    while i < code.len() {
        match &mut code[i] {
            CsilInstr::WindowMean { slot, length } => {
                if let Some(value) = values.value(TaInputTarget::Sma { slot: *slot }) {
                    // Outside validated input bounds the cast truncates, by design.
                    *length = value as u32;
                }
            }
            CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => {
                if let Some(value) = values.value(TaInputTarget::Change { slot: *slot }) {
                    *offset = value as u32;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

pub(crate) fn specialize_functions<L: Lookup>(functions: &mut [CsilFunction], values: &L) {
    let mut f = 0;
    while f < functions.len() {
        specialize_code(&mut functions[f].code, values);
        if let Some(reference) = &mut functions[f].budget_reference {
            specialize_code(&mut reference.code, values);
        }
        f += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BudgetReference, F64Bits};
    use pinewasm_hir::TaFunc;
    use std::cell::RefCell;

    #[derive(Default)]
    struct ConstantHasher;

    impl std::hash::Hasher for ConstantHasher {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, _bytes: &[u8]) {}
    }

    fn function(code: Vec<CsilInstr>, reference: Option<Vec<CsilInstr>>) -> CsilFunction {
        CsilFunction {
            name: "preserve".into(),
            code,
            source_map: vec![],
            max_stack: 17,
            budget_reference: reference.map(|code| BudgetReference {
                version: 2,
                code,
                source_map: vec![],
                max_stack: 19,
            }),
        }
    }

    // Plain re-implementation of the match and iterator loops, used by the differential test.
    fn observers_plain(
        functions: &[CsilFunction],
        values: &HashMap<TaInputTarget, i64>,
    ) -> Vec<history::DepthObserver> {
        let resolve = |target| values.get(&target).map(|v| history::ta_override_u32(*v));
        let mut out = Vec::new();
        for f in functions {
            for instr in &f.code {
                match instr {
                    CsilInstr::ReadUserSlot { slot, offset } => {
                        out.push(history::DepthObserver::ReadUserSlot {
                            offset: *offset,
                            change: if *offset == 0 {
                                None
                            } else {
                                resolve(TaInputTarget::Change { slot: *slot })
                            },
                        });
                    }
                    CsilInstr::WindowMean { slot, length } => {
                        out.push(history::DepthObserver::WindowMean {
                            length: *length,
                            sma: resolve(TaInputTarget::Sma { slot: *slot }),
                        });
                    }
                    CsilInstr::WindowMeanDynamic { .. }
                    | CsilInstr::WindowExtremumDynamic { .. }
                    | CsilInstr::WindowStatisticDynamic { .. } => {
                        out.push(history::DepthObserver::DynamicWindow);
                    }
                    _ => {}
                }
            }
        }
        out
    }

    fn baseline_code(code: &mut [CsilInstr], values: &HashMap<TaInputTarget, i64>) {
        for instr in code {
            match instr {
                CsilInstr::WindowMean { slot, length } => {
                    if let Some(v) = values.get(&TaInputTarget::Sma { slot: *slot }) {
                        *length = *v as u32;
                    }
                }
                CsilInstr::ReadUserSlot { slot, offset } if *offset != 0 => {
                    if let Some(v) = values.get(&TaInputTarget::Change { slot: *slot }) {
                        *offset = *v as u32;
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn append_observer_preserves_none_capacity_and_some_order() {
        let mut values = Vec::with_capacity(4);
        let capacity = values.capacity();
        append_observer(&mut values, None);
        assert!(values.is_empty());
        assert_eq!(values.capacity(), capacity);
        append_observer(&mut values, Some(history::DepthObserver::DynamicWindow));
        append_observer(&mut values, None);
        append_observer(
            &mut values,
            Some(history::DepthObserver::ReadUserSlot {
                offset: 7,
                change: None,
            }),
        );
        assert_eq!(
            values,
            vec![
                history::DepthObserver::DynamicWindow,
                history::DepthObserver::ReadUserSlot {
                    offset: 7,
                    change: None
                }
            ]
        );
        assert_eq!(values.capacity(), capacity);
    }

    #[test]
    fn differential_boundaries_and_all_function_paths() {
        for literal in [0, 1, 100000, u32::MAX] {
            for value in [
                None,
                Some(-1),
                Some(0),
                Some(1),
                Some(100000),
                Some(u32::MAX as i64),
                Some(u32::MAX as i64 + 1),
                Some(i64::MIN),
                Some(i64::MAX),
            ] {
                let mut values = HashMap::new();
                if let Some(v) = value {
                    values.insert(TaInputTarget::Sma { slot: 4 }, v);
                    values.insert(TaInputTarget::Change { slot: 4 }, v);
                }
                values.insert(TaInputTarget::Sma { slot: 5 }, 7);
                values.insert(
                    TaInputTarget::State {
                        state: 999,
                        param: 999,
                    },
                    123,
                );
                let code = vec![
                    CsilInstr::ReadUserSlot {
                        slot: 4,
                        offset: literal,
                    },
                    CsilInstr::WindowMean {
                        slot: 4,
                        length: literal,
                    },
                    CsilInstr::WindowMeanDynamic { slot: 4 },
                    CsilInstr::WindowExtremumDynamic {
                        slot: 4,
                        highest: false,
                    },
                    CsilInstr::WindowStatisticDynamic {
                        slot: 4,
                        func: TaFunc::Median,
                    },
                    CsilInstr::PushF64(F64Bits::new(f64::from_bits(0x8000000000000000))),
                    CsilInstr::Return,
                ];
                let functions = vec![
                    function(vec![], None),
                    function(code.clone(), Some(code)),
                    function(
                        vec![CsilInstr::WindowMean { slot: 5, length: 2 }],
                        Some(vec![]),
                    ),
                ];
                assert_eq!(
                    observers(&functions, &values),
                    observers_plain(&functions, &values)
                );
                let mut expected = functions.clone();
                for f in &mut expected {
                    baseline_code(&mut f.code, &values);
                    if let Some(r) = &mut f.budget_reference {
                        baseline_code(&mut r.code, &values);
                    }
                }
                let mut actual = functions;
                specialize_functions(&mut actual, &values);
                assert_eq!(actual, expected);
                let CsilInstr::PushF64(bits) = actual[1].code[5] else {
                    panic!("float payload arm changed")
                };
                assert_eq!(bits.to_bits(), 0x8000000000000000);
                assert_eq!(actual[1].name, "preserve");
                assert_eq!(actual[1].max_stack, 17);
                assert_eq!(actual[1].budget_reference.as_ref().unwrap().max_stack, 19);
                assert_eq!(
                    history::history_depth_scan(2, &observers(&actual, &HashMap::new())),
                    history::history_depth_scan(2, &observers_plain(&expected, &HashMap::new()))
                );
            }
        }
        assert!(observers(&[], &HashMap::new()).is_empty());
    }

    struct Spy {
        calls: RefCell<Vec<TaInputTarget>>,
    }
    impl Lookup for Spy {
        fn value(&self, target: TaInputTarget) -> Option<i64> {
            self.calls.borrow_mut().push(target);
            Some(match target {
                TaInputTarget::Sma { .. } => 6,
                _ => 3,
            })
        }
    }

    #[test]
    fn zero_offset_and_target_kind_controls() {
        let spy = Spy {
            calls: RefCell::new(vec![]),
        };
        let zero = CsilInstr::ReadUserSlot { slot: 4, offset: 0 };
        assert_eq!(
            observer(&zero, &spy),
            Some(history::DepthObserver::ReadUserSlot {
                offset: 0,
                change: None
            })
        );
        let mut code = vec![
            zero,
            CsilInstr::WindowMean { slot: 4, length: 1 },
            CsilInstr::ReadUserSlot { slot: 4, offset: 2 },
        ];
        specialize_code(&mut code, &spy);
        assert_eq!(
            *spy.calls.borrow(),
            vec![
                TaInputTarget::Sma { slot: 4 },
                TaInputTarget::Change { slot: 4 }
            ]
        );
        assert_eq!(
            code,
            vec![
                CsilInstr::ReadUserSlot { slot: 4, offset: 0 },
                CsilInstr::WindowMean { slot: 4, length: 6 },
                CsilInstr::ReadUserSlot { slot: 4, offset: 3 }
            ]
        );
    }

    #[test]
    fn hashmap_lookup_preserves_signed_values_missing_entries_and_map_state() {
        let min_target = TaInputTarget::Sma { slot: 1 };
        let negative_target = TaInputTarget::Change { slot: 2 };
        let max_target = TaInputTarget::State { state: 3, param: 4 };
        let absent_target = TaInputTarget::Sma { slot: 5 };
        let values = HashMap::from([
            (min_target, i64::MIN),
            (negative_target, -1),
            (max_target, i64::MAX),
        ]);
        let before = values.clone();

        assert_eq!(lookup_ta_override(&values, min_target), Some(i64::MIN));
        assert_eq!(values.value(negative_target), Some(-1));
        assert_eq!(values.value(max_target), Some(i64::MAX));
        assert_eq!(lookup_ta_override(&values, absent_target), None);
        assert_eq!(values, before);
    }

    #[test]
    fn hashmap_lookup_survives_forced_hash_collisions_and_separates_target_tags() {
        type CollidingMap =
            HashMap<TaInputTarget, i64, std::hash::BuildHasherDefault<ConstantHasher>>;

        let sma = TaInputTarget::Sma { slot: 7 };
        let change = TaInputTarget::Change { slot: 7 };
        let state = TaInputTarget::State { state: 7, param: 1 };
        let mut values = CollidingMap::default();
        values.insert(sma, i64::MIN);
        values.insert(change, -1);
        values.insert(state, i64::MAX);

        assert_eq!(lookup_ta_override(&values, sma), Some(i64::MIN));
        assert_eq!(lookup_ta_override(&values, change), Some(-1));
        assert_eq!(lookup_ta_override(&values, state), Some(i64::MAX));
        assert_eq!(
            lookup_ta_override(&values, TaInputTarget::Sma { slot: 8 }),
            None
        );
    }

    #[test]
    fn planner_ignores_reference_and_checked_cast_differs_from_patch() {
        let values = HashMap::from([(TaInputTarget::Sma { slot: 4 }, 4294967296)]);
        let mut functions = vec![function(
            vec![CsilInstr::WindowMean { slot: 4, length: 2 }],
            Some(vec![CsilInstr::ReadUserSlot {
                slot: 8,
                offset: 999,
            }]),
        )];
        assert_eq!(
            observers(&functions, &values),
            vec![history::DepthObserver::WindowMean {
                length: 2,
                sma: Some(u32::MAX)
            }]
        );
        specialize_functions(&mut functions, &values);
        assert_eq!(
            functions[0].code,
            vec![CsilInstr::WindowMean { slot: 4, length: 0 }]
        );
        assert_eq!(
            functions[0].budget_reference.as_ref().unwrap().code,
            vec![CsilInstr::ReadUserSlot {
                slot: 8,
                offset: 999
            }]
        );
    }
}
