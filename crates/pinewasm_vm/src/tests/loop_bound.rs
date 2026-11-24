//! Full-frame comparison against the unfused zero/compare/read/select arms.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, vm_ops, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram, F64Bits};
use pinewasm_mir::{HandleId, HeapObj, Value};

struct ReferenceHost<'a>(RunBarHost<'a>);

impl vm_dispatch::DispatchHost for ReferenceHost<'_> {
    type Instr = CsilInstr;
    type Error = RuntimeError;
    fn remaining(&mut self) -> &mut u64 {
        &mut self.0.vm.instructions_remaining
    }
    fn limit_error(&mut self, limit: vm_charge::ChargeLimit, pc: usize) -> RuntimeError {
        self.0.limit_error(limit, pc)
    }
    fn past_end(&mut self, pc: usize) -> RuntimeError {
        self.0.past_end(pc)
    }
    fn exec(
        &mut self,
        instr: &CsilInstr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<vm_dispatch::Flow, RuntimeError> {
        let value = match instr {
            CsilInstr::PushI64(value) => Some(vm_ops::const_value(vm_ops::VmConst::Int(*value))),
            CsilInstr::PushF64(value) => {
                Some(vm_ops::const_value(vm_ops::VmConst::Float(value.get())))
            }
            _ => None,
        };
        if let Some(value) = value {
            let vm = &mut *self.0.vm;
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            vm.push(value, pc)?;
            Ok(vm_dispatch::Flow::Next)
        } else {
            self.0.exec(instr, pc, budget)
        }
    }
}

fn instructions(zero: CsilInstr) -> Vec<CsilInstr> {
    vec![
        zero,
        CsilInstr::CmpGe,
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
        CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
        CsilInstr::CmpLe,
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
        CsilInstr::ReadUserSlot { slot: 1, offset: 0 },
        CsilInstr::CmpGe,
        CsilInstr::Select,
        CsilInstr::Return,
    ]
}

fn program(code: Vec<CsilInstr>) -> CsilProgram {
    let mut p = compile("indicator(\"Loop comparisons\")\na = close\nb = open\nplot(a + b)\nplot(a)\nplot(b)\nplot(a - b)\nplot(a * b)\n");
    assert!(p.series_slots.len() >= 2);
    let main = &mut p.functions[0];
    assert!(main.source_map.len() >= code.len());
    main.source_map.truncate(code.len());
    main.budget_reference = None;
    main.max_stack = 8;
    main.code = code;
    p
}

fn same_values(a: &[Value], b: &[Value]) {
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(b) {
        match (a, b) {
            (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => assert_eq!(a, b),
        }
    }
}

// Keep the frame, fuel, peak and missing-cell cases explicit in the comparison
// matrix. The lint allowance below applies only to this test helper.
#[allow(clippy::too_many_arguments)]
fn compare(
    p: &CsilProgram,
    stack: &[Value],
    counter: &Value,
    end: &Value,
    bar: u64,
    run: u64,
    limit: usize,
    empty_cell: bool,
) -> u64 {
    let data = data_from_close(&[1.0, 2.0, 3.0]);
    let options = RunOptions {
        profile: true,
        max_stack_depth: limit,
        ..RunOptions::default()
    };
    let mut actual = Vm::new(p, &data, &options).unwrap();
    let mut original = Vm::new(p, &data, &options).unwrap();
    for vm in [&mut actual, &mut original] {
        vm.heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Int(37)],
            })
            .unwrap();
        vm.stack = stack.to_vec();
        vm.stack.reserve(16);
        vm.user_slots[0].fill(counter.clone());
        vm.user_slots[1].fill(end.clone());
        if empty_cell {
            vm.user_slots[0].clear();
        }
        vm.written[0] = true;
        vm.instructions_remaining = run;
        vm.call_args = vec![Value::String("retained scratch".into()), Value::Int(42)];
        vm.string_bytes_allocated = 37;
    }
    let (mut b, mut r) = (bar, bar);
    let a_result = vm_dispatch::dispatch_loop(
        &mut RunBarHost {
            vm: &mut actual,
            program: p,
        },
        &p.main().code,
        &mut b,
    );
    let r_result = vm_dispatch::dispatch_loop(
        &mut ReferenceHost(RunBarHost {
            vm: &mut original,
            program: p,
        }),
        &p.main().code,
        &mut r,
    );
    assert_eq!(a_result, r_result);
    same_values(&actual.stack, &original.stack);
    assert_eq!(actual.stack.capacity(), original.stack.capacity());
    assert_eq!(actual.heap, original.heap);
    for (a, b) in actual.user_slots.iter().zip(&original.user_slots) {
        same_values(a, b);
    }
    assert_eq!(actual.written, original.written);
    same_values(&actual.call_args, &original.call_args);
    assert_eq!(
        actual.string_bytes_allocated,
        original.string_bytes_allocated
    );
    assert_eq!(
        (b, actual.instructions_remaining),
        (r, original.instructions_remaining)
    );
    let a = actual.profile.as_ref().unwrap();
    let b = original.profile.as_ref().unwrap();
    assert_eq!(a.logical_counts(), b.logical_counts());
    assert_eq!(a.units, b.units);
    assert_eq!(b.combined_dispatches(), 0);
    assert!([0, 8].contains(&a.combined_dispatches()));
    a.combined_dispatches()
}

fn values() -> Vec<Value> {
    vec![
        Value::Na,
        Value::Int(i64::MIN),
        Value::Int(-1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(i64::MAX),
        Value::Float(-f64::MAX),
        Value::Float(-0.0),
        Value::Float(0.0),
        Value::Float(f64::from_bits(1)),
        Value::Float(0.5),
        Value::Float(f64::MAX),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Bool(true),
        Value::String("read payload".into()),
        Value::Ref(HandleId(0)),
        Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 4.0,
            input_origin: None,
        },
    ]
}

#[test]
fn loop_bound_preserves_numeric_bits_errors_peak_fuel_and_complete_frame() {
    let p = program(instructions(CsilInstr::PushI64(0)));
    let values = values();
    let mut accepted = 0;
    for operand in 0..3 {
        for value in &values {
            let mut operands = [Value::Int(1), Value::Int(0), Value::Int(3)];
            operands[operand] = value.clone();
            for prefix in [false, true] {
                let mut stack = if prefix {
                    vec![Value::String("prefix".into())]
                } else {
                    vec![]
                };
                stack.push(operands[0].clone());
                for bar in 0..=10 {
                    for run in 0..=10 {
                        for limit in [0, 1, 3, 4, 5, 8] {
                            accepted += compare(
                                &p,
                                &stack,
                                &operands[1],
                                &operands[2],
                                bar,
                                run,
                                limit,
                                false,
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(accepted > 0);
}

#[test]
fn loop_bound_mixed_numeric_directions_signed_zero_aliases_and_missing_cells() {
    for zero in [
        CsilInstr::PushI64(0),
        CsilInstr::PushF64(F64Bits::new(0.0)),
        CsilInstr::PushF64(F64Bits::new(-0.0)),
    ] {
        let p = program(instructions(zero.clone()));
        for step in values() {
            for counter in values() {
                for end in values() {
                    compare(
                        &p,
                        std::slice::from_ref(&step),
                        &counter,
                        &end,
                        20,
                        20,
                        8,
                        false,
                    );
                }
            }
        }
        assert_eq!(
            compare(
                &p,
                &[Value::Int(1)],
                &Value::Int(0),
                &Value::Int(3),
                20,
                20,
                8,
                true
            ),
            8
        );
        let mut code = instructions(zero);
        for pc in [3, 6] {
            code[pc] = CsilInstr::ReadUserSlot { slot: 0, offset: 0 };
        }
        let p = program(code);
        assert_eq!(
            compare(
                &p,
                &[Value::Int(-1)],
                &Value::Int(0),
                &Value::Int(3),
                20,
                20,
                8,
                false
            ),
            8
        );
    }
}

#[test]
fn loop_bound_preserves_malformed_truncated_and_standalone_tail_entries() {
    let original = instructions(CsilInstr::PushI64(0));
    for length in 1..9 {
        let p = program(original[..length].to_vec());
        assert_eq!(
            compare(
                &p,
                &[Value::Int(1)],
                &Value::Int(0),
                &Value::Int(3),
                20,
                20,
                8,
                false
            ),
            0
        );
    }
    for pc in 1..9 {
        let mut code = original.clone();
        code[pc] = CsilInstr::PushI64(7);
        let p = program(code);
        assert_eq!(
            compare(
                &p,
                &[Value::Int(1)],
                &Value::Int(0),
                &Value::Int(3),
                20,
                20,
                8,
                false
            ),
            0
        );
    }
    for pc in [2, 3, 5, 6] {
        let mut code = original.clone();
        code[pc] = CsilInstr::ReadUserSlot { slot: 0, offset: 1 };
        let p = program(code);
        assert_eq!(
            compare(
                &p,
                &[Value::Int(1)],
                &Value::Int(0),
                &Value::Int(3),
                20,
                20,
                8,
                false
            ),
            0
        );
    }
    for target in 2..=9 {
        let mut code = vec![CsilInstr::Jump { target }];
        code.extend(original.clone());
        let p = program(code);
        for stack in [
            vec![],
            vec![Value::Int(1)],
            vec![Value::Bool(true), Value::Int(0), Value::Int(3)],
        ] {
            assert_eq!(
                compare(&p, &stack, &Value::Int(0), &Value::Int(3), 20, 20, 8, false),
                0
            );
        }
    }
}

#[test]
fn loop_bound_delegates_when_original_stack_pushes_would_allocate() {
    let p = program(instructions(CsilInstr::PushI64(0)));
    let data = data_from_close(&[1.0]);
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    for prefix in 0..=3 {
        let mut vm = Vm::new(&p, &data, &options).unwrap();
        vm.stack = vec![Value::Int(1); prefix + 1];
        vm.stack.shrink_to_fit();
        assert_eq!(vm.stack.capacity(), vm.stack.len());
        vm.user_slots[0].fill(Value::Int(0));
        vm.user_slots[1].fill(Value::Int(3));
        let before = vm.stack.clone();
        let capacity = vm.stack.capacity();
        let mut budget = 20;
        assert!(vm
            .exec_loop_bound(&p, Value::Int(0), 0, &mut budget)
            .is_none());
        same_values(&vm.stack, &before);
        assert_eq!(vm.stack.capacity(), capacity);
        assert_eq!(budget, 20);
        assert_eq!(vm.profile.as_ref().unwrap().combined_dispatches(), 0);
    }
}
