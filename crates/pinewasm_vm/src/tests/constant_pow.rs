//! Constant-pow pairs compared with the original push and full frames.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, vm_ops, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram, F64Bits, PureFunc};
use pinewasm_mir::{HandleId, HeapObj, Value};

/// Pushes `PushF64` operands with the plain constant push; other instructions
/// go to `RunBarHost`.
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
        if let CsilInstr::PushF64(value) = instr {
            let vm = &mut *self.0.vm;
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            vm.push(vm_ops::const_value(vm_ops::VmConst::Float(value.get())), pc)?;
            Ok(vm_dispatch::Flow::Next)
        } else {
            self.0.exec(instr, pc, budget)
        }
    }
}

fn values() -> Vec<Value> {
    vec![
        Value::Na,
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Float(-0.0),
        Value::Float(f64::from_bits(1)),
        Value::Float(f64::MAX),
        Value::Float(f64::INFINITY),
        Value::Float(f64::from_bits(0x7ff8_0000_0000_0021)),
        Value::Color {
            r: 1,
            g: 2,
            b: 3,
            t: 4.0,
            input_origin: None,
        },
        Value::String("retained string".into()),
        Value::Ref(HandleId(0)),
        Value::Ref(HandleId(u32::MAX)),
    ]
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

fn program(code: Vec<CsilInstr>) -> CsilProgram {
    let mut p =
        compile("indicator(\"Constant pow\")\na = close\nb = open\nplot(math.pow(a, 2) + b)\n");
    let main = &mut p.functions[0];
    main.source_map.truncate(code.len());
    main.max_stack = 6;
    main.budget_reference = None;
    main.code = code;
    p
}

fn compare(
    program: &CsilProgram,
    first: &Value,
    capacity: usize,
    bar_budget: u64,
    run_budget: u64,
    limit: usize,
    incoming: usize,
) -> (u64, u64) {
    let data = data_from_close(&[1.0, 2.0, 3.0]);
    let options = RunOptions {
        profile: true,
        max_stack_depth: limit,
        ..RunOptions::default()
    };
    let mut actual = Vm::new(program, &data, &options).unwrap();
    let mut original = Vm::new(program, &data, &options).unwrap();
    for vm in [&mut actual, &mut original] {
        vm.heap
            .alloc(HeapObj::Object {
                table_identity: None,
                type_id: 0,
                fields: vec![Value::Int(37)],
            })
            .unwrap();
        vm.user_slots[0].fill(Value::String("overwritten cell".into()));
        vm.user_slots[1].fill(Value::Int(17));
        vm.bar = 2;
        vm.stack = Vec::with_capacity(capacity);
        if incoming >= 2 {
            vm.stack.push(Value::Float(-0.0));
        }
        if incoming >= 1 {
            vm.stack.push(first.clone());
        }
        vm.call_args = vec![Value::String("untouched scratch".into())];
        vm.instructions_remaining = run_budget;
    }
    let (mut a, mut b) = (bar_budget, bar_budget);
    let ra = vm_dispatch::dispatch_loop(
        &mut RunBarHost {
            vm: &mut actual,
            program,
        },
        &program.main().code,
        &mut a,
    );
    let rb = vm_dispatch::dispatch_loop(
        &mut ReferenceHost(RunBarHost {
            vm: &mut original,
            program,
        }),
        &program.main().code,
        &mut b,
    );
    assert_eq!(ra, rb);
    same_values(&actual.stack, &original.stack);
    assert_eq!(actual.stack.capacity(), original.stack.capacity());
    for (a, b) in actual.user_slots.iter().zip(&original.user_slots) {
        same_values(a, b);
    }
    assert_eq!(actual.written, original.written);
    assert_eq!(actual.heap, original.heap);
    same_values(&actual.call_args, &original.call_args);
    assert_eq!(
        (
            a,
            actual.instructions_remaining,
            actual.string_bytes_allocated
        ),
        (
            b,
            original.instructions_remaining,
            original.string_bytes_allocated
        )
    );
    let pa = actual.profile.as_ref().unwrap();
    let pb = original.profile.as_ref().unwrap();
    assert_eq!(
        (pa.logical_counts(), &pa.units),
        (pb.logical_counts(), &pb.units)
    );
    (pa.combined_dispatches(), pb.combined_dispatches())
}

#[test]
fn constant_pow_preserves_full_frames_every_fuel_cut_and_backing_capacity() {
    for exponent in [
        2.0,
        -2.0,
        0.0,
        -0.0,
        0.5,
        f64::INFINITY,
        f64::from_bits(0x7ff8_0000_0000_0021),
    ] {
        let p = program(vec![
            CsilInstr::PushF64(F64Bits::from(exponent)),
            CsilInstr::CallPure {
                func: PureFunc::MathPow,
                argc: 2,
            },
            CsilInstr::Return,
        ]);
        for first in values() {
            for capacity in [0, 1, 2, 3] {
                for incoming in [0, 1, 2] {
                    for limit in [0, 1, 2, 3] {
                        for bar in [0, 1, 2, 3, u64::MAX] {
                            for run in [0, 1, 2, 3, u64::MAX] {
                                let (actual, reference) =
                                    compare(&p, &first, capacity, bar, run, limit, incoming);
                                assert_eq!(reference, 0);
                                if exponent.to_bits() == 2.0_f64.to_bits()
                                    && capacity > incoming
                                    && incoming > 0
                                    && incoming < limit
                                    && bar >= 2
                                    && run >= 2
                                    && matches!(first, Value::Na | Value::Int(_) | Value::Float(_))
                                {
                                    assert_eq!(actual, 1);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn constant_pow_preserves_standalone_entries_and_malformed_calls() {
    for func in [PureFunc::MathPow, PureFunc::MathSqrt] {
        for argc in [0, 1, 2, 3] {
            for target in [0, 1, 2, 3, 4, u32::MAX] {
                let p = program(vec![
                    CsilInstr::Jump { target },
                    CsilInstr::PushF64(F64Bits::from(2.0)),
                    CsilInstr::CallPure { func, argc },
                    CsilInstr::Return,
                ]);
                for value in values() {
                    for incoming in [0, 1, 2] {
                        for limit in [0, 1, 2, 3] {
                            for budget in [0, 1, 2, 3, 4, 5] {
                                compare(&p, &value, 3, budget, budget, limit, incoming);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn constant_pow_capacity_guard_is_independent_of_intermediate_observation() {
    let p = program(vec![
        CsilInstr::PushF64(F64Bits::from(2.0)),
        CsilInstr::CallPure {
            func: PureFunc::MathPow,
            argc: 2,
        },
        CsilInstr::Return,
    ]);
    let data = data_from_close(&[1.0]);
    let options = RunOptions {
        max_stack_depth: 2,
        ..RunOptions::default()
    };
    let mut vm = Vm::new(&p, &data, &options).unwrap();
    vm.stack = vec![Value::Float(3.0)];
    assert_eq!(vm.stack.capacity(), 1);
    vm.instructions_remaining = 10;
    let mut budget = 10;
    assert_eq!(vm.exec_constant_pow(&p, 2.0, 0, &mut budget), None);
    assert_eq!(
        (vm.stack.capacity(), budget, vm.instructions_remaining),
        (1, 10, 10)
    );
    assert_eq!(vm.stack, vec![Value::Float(3.0)]);
}
