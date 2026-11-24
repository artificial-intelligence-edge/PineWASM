//! Ordered resolution and installation of run-constant TA input bindings.
//! The generic fold exposes the production map construction without building
//! a second table or changing the resolver's integer/float parameter rules.

use pinewasm_csil::CsilProgram;
use pinewasm_hir::{InputValue, TaInputBinding, TaInputTarget};
use std::collections::HashMap;

pub(crate) trait Resolve {
    fn resolve(&self, binding: &TaInputBinding) -> Option<i64>;
}

pub(crate) trait Insert {
    fn insert(&mut self, target: TaInputTarget, value: i64);
}

pub(crate) struct ProgramInputs<'a> {
    pub(crate) program: &'a CsilProgram,
    pub(crate) inputs: &'a HashMap<u32, InputValue>,
}

impl Resolve for ProgramInputs<'_> {
    #[inline]
    fn resolve(&self, binding: &TaInputBinding) -> Option<i64> {
        self.program
            .resolve_ta_input(binding, self.inputs.get(&binding.input))
    }
}

impl Insert for HashMap<TaInputTarget, i64> {
    #[inline]
    fn insert(&mut self, target: TaInputTarget, value: i64) {
        HashMap::insert(self, target, value);
    }
}

/// Install in binding order, stopping before inserting the first rejected
/// binding. Earlier insertions remain; duplicate targets retain the last
/// successful value. The returned index identifies the binding that failed.
/// Production owns a fresh map and drops it on failure.
pub(crate) fn install<R: Resolve, M: Insert>(
    bindings: &[TaInputBinding],
    resolver: &R,
    values: &mut M,
) -> Result<(), usize> {
    let mut index = 0;
    let mut failed = None;
    while index < bindings.len() && failed.is_none() {
        let binding = &bindings[index];
        match resolver.resolve(binding) {
            Some(value) => {
                values.insert(binding.target, value);
                index += 1;
            }
            None => failed = Some(index),
        }
    }
    match failed {
        Some(index) => Err(index),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinewasm_hir::{TaFunc, TaStateDecl};
    use std::cell::RefCell;

    fn binding(input: u32, target: TaInputTarget) -> TaInputBinding {
        TaInputBinding {
            input,
            negated: false,
            target,
        }
    }

    fn compile(source: &str) -> CsilProgram {
        let parsed = pinewasm_parse::parse(source);
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            !pinewasm_span::has_errors(&analyzed.diagnostics),
            "{:?}",
            analyzed.diagnostics
        );
        let program = pinewasm_csil::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).unwrap();
        pinewasm_csil::validate(&program).unwrap();
        program
    }

    fn program() -> CsilProgram {
        compile("//@version=6\nindicator('Bindings')\nn=input.int(2)\nm=input.int(3)\nplot(ta.sma(close,n))\nplot(ta.change(close,m))\nplot(ta.ema(close,n))\n")
    }

    // Test-only sequential copy of the resolution/insertion loop, as the comparison baseline.
    fn original(
        program: &CsilProgram,
        inputs: &HashMap<u32, InputValue>,
    ) -> (Result<(), usize>, HashMap<TaInputTarget, i64>) {
        let mut values = HashMap::new();
        for (index, binding) in program.ta_input_bindings.iter().enumerate() {
            let Some(value) = program.resolve_ta_input(binding, inputs.get(&binding.input)) else {
                return (Err(index), values);
            };
            values.insert(binding.target, value);
        }
        (Ok(()), values)
    }

    #[test]
    fn ordered_trace_first_failure_and_retained_prefix() {
        struct Trace<'a> {
            events: &'a RefCell<Vec<(char, u32)>>,
        }
        impl Resolve for Trace<'_> {
            fn resolve(&self, b: &TaInputBinding) -> Option<i64> {
                self.events.borrow_mut().push(('r', b.input));
                if b.input == 2 {
                    None
                } else {
                    Some(i64::from(b.input))
                }
            }
        }
        impl Insert for Trace<'_> {
            fn insert(&mut self, _: TaInputTarget, v: i64) {
                self.events.borrow_mut().push(('i', v as u32));
            }
        }
        let events = RefCell::new(Vec::new());
        let bindings: Vec<_> = (0..4)
            .map(|i| binding(i, TaInputTarget::Sma { slot: i }))
            .collect();
        assert_eq!(
            install(
                &bindings,
                &Trace { events: &events },
                &mut Trace { events: &events }
            ),
            Err(2)
        );
        assert_eq!(
            *events.borrow(),
            [('r', 0), ('i', 0), ('r', 1), ('i', 1), ('r', 2)]
        );
    }

    #[test]
    fn real_resolver_original_loop_differential_and_duplicate_last_wins() {
        let mut p = program();
        for first in [
            None,
            Some(InputValue::Int(7)),
            Some(InputValue::Bool(true)),
            Some(InputValue::Int(0)),
            Some(InputValue::Int(100001)),
            Some(InputValue::Int(i64::MIN)),
        ] {
            for second in [None, Some(InputValue::Int(9)), Some(InputValue::Float(4.0))] {
                let mut inputs = HashMap::new();
                if let Some(v) = first.clone() {
                    inputs.insert(0, v);
                }
                if let Some(v) = second.clone() {
                    inputs.insert(1, v);
                }
                let expected = original(&p, &inputs);
                let mut actual = HashMap::new();
                let status = install(
                    &p.ta_input_bindings,
                    &ProgramInputs {
                        program: &p,
                        inputs: &inputs,
                    },
                    &mut actual,
                );
                assert_eq!((status, actual), expected);
            }
        }
        let target = p.ta_input_bindings[0].target;
        p.ta_input_bindings = vec![binding(0, target), binding(1, target)];
        let inputs = HashMap::from([(0, InputValue::Int(7)), (1, InputValue::Int(9))]);
        let mut values = HashMap::new();
        assert_eq!(
            install(
                &p.ta_input_bindings,
                &ProgramInputs {
                    program: &p,
                    inputs: &inputs
                },
                &mut values
            ),
            Ok(())
        );
        assert_eq!(values.get(&target), Some(&9));
        p.ta_input_bindings[1].input = 0;
        p.ta_input_bindings[1].target = TaInputTarget::Change { slot: 77 };
        values.clear();
        install(
            &p.ta_input_bindings,
            &ProgramInputs {
                program: &p,
                inputs: &inputs,
            },
            &mut values,
        )
        .unwrap();
        assert_eq!(values.len(), 2);
        assert!(values.values().all(|v| *v == 7));
        p.ta_input_bindings[0].negated = true;
        let inputs = HashMap::from([(0, InputValue::Int(i64::MIN))]);
        assert_eq!(
            install(
                &p.ta_input_bindings,
                &ProgramInputs {
                    program: &p,
                    inputs: &inputs
                },
                &mut values
            ),
            Err(0)
        );
    }

    #[test]
    fn float_bits_percent_endpoints_and_wrong_kind_default() {
        let mut p = program();
        p.inputs[0].defval = InputValue::Float(0.25);
        p.ta_states = vec![TaStateDecl {
            func: TaFunc::Alma,
            params: vec![2, 0.25f64.to_bits() as i64, 6.0f64.to_bits() as i64, 0],
        }];
        let b = binding(0, TaInputTarget::State { state: 0, param: 1 });
        for (input, expected) in [
            (InputValue::Float(-0.0), Some((-0.0f64).to_bits() as i64)),
            (InputValue::Float(0.0), Some(0)),
            (InputValue::Float(f64::NAN), None),
            (InputValue::Float(f64::INFINITY), None),
            (InputValue::Bool(true), Some(0.25f64.to_bits() as i64)),
        ] {
            let inputs = HashMap::from([(0, input)]);
            let mut values = HashMap::new();
            let status = install(
                &[b],
                &ProgramInputs {
                    program: &p,
                    inputs: &inputs,
                },
                &mut values,
            );
            assert_eq!(status.is_ok(), expected.is_some());
            assert_eq!(values.get(&b.target).copied(), expected);
        }
        p.ta_states[0].func = TaFunc::PercentileNearestRank;
        for v in [0.0, 100.0, -0.0, -1.0, 100.01, f64::NAN] {
            let inputs = HashMap::from([(0, InputValue::Float(v))]);
            let mut values = HashMap::new();
            let status = install(
                &[b],
                &ProgramInputs {
                    program: &p,
                    inputs: &inputs,
                },
                &mut values,
            );
            let valid = v.is_finite() && (0.0..=100.0).contains(&v);
            assert_eq!(status.is_ok(), valid);
            assert_eq!(
                values.get(&b.target).copied(),
                valid.then_some(v.to_bits() as i64)
            );
        }
    }

    #[test]
    fn actual_vm_planning_specialization_zero_change_and_errors() {
        let p = program();
        let data = crate::MarketData {
            open: vec![1.0; 8],
            high: vec![1.0; 8],
            low: vec![1.0; 8],
            close: vec![1.0; 8],
            volume: vec![1.0; 8],
            ..Default::default()
        };
        for n in [2, 7] {
            let options = crate::RunOptions {
                inputs: HashMap::from([(0, InputValue::Int(n)), (1, InputValue::Int(4))]),
                ..Default::default()
            };
            let (_, values) = original(&p, &options.inputs);
            let depth = p.history_depth_with_ta_inputs(&values);
            let vm = crate::Vm::new_owned(&p, None, &data, &options).unwrap();
            assert_eq!(
                vm.slot_mask + 1,
                crate::vm_layout::ring_cells(depth).unwrap()
            );
            assert_eq!(vm.program.history_depth(), depth);
            let mut expected = p.clone();
            expected.set_ta_inputs(&values);
            assert_eq!(*vm.program, expected);
            for (before, after) in p
                .functions
                .iter()
                .flat_map(|f| &f.code)
                .zip(vm.program.functions.iter().flat_map(|f| &f.code))
            {
                if matches!(
                    before,
                    pinewasm_csil::CsilInstr::ReadUserSlot { offset: 0, .. }
                ) {
                    assert_eq!(before, after);
                }
            }
        }
        for bad in [0, 100001, i64::MIN] {
            let options = crate::RunOptions {
                inputs: HashMap::from([(0, InputValue::Int(bad))]),
                ..Default::default()
            };
            let error = match crate::Vm::new_owned(&p, None, &data, &options) {
                Ok(_) => panic!("invalid length accepted"),
                Err(e) => e,
            };
            assert_eq!(error.kind, crate::RuntimeErrorKind::InvalidTaLength);
            assert_eq!(
                error.message,
                "input 0 is outside the legal TA parameter range 1..=100000"
            );
            assert_eq!(
                (error.bar_index, error.instruction_offset, error.span),
                (None, None, None)
            );
        }
    }
}
