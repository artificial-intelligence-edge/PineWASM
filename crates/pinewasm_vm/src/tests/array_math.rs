//! Compare complete VM frames with the ordinary six instruction handlers.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CollectionFunc, CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::{ElemType, HandleId, Heap, HeapObj, Value};

/// Runs `CallCollection` and `CallPure` through the ordinary unfused handlers;
/// other instructions go to `RunBarHost`.
struct ReferenceHost<'a> {
    inner: RunBarHost<'a>,
}

impl vm_dispatch::DispatchHost for ReferenceHost<'_> {
    type Instr = CsilInstr;
    type Error = RuntimeError;
    fn remaining(&mut self) -> &mut u64 {
        &mut self.inner.vm.instructions_remaining
    }
    fn limit_error(&mut self, limit: vm_charge::ChargeLimit, pc: usize) -> RuntimeError {
        self.inner.limit_error(limit, pc)
    }
    fn past_end(&mut self, pc: usize) -> RuntimeError {
        self.inner.past_end(pc)
    }
    fn exec(
        &mut self,
        instr: &CsilInstr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<vm_dispatch::Flow, RuntimeError> {
        match instr {
            CsilInstr::CallCollection { func, argc } => {
                let vm = &mut *self.inner.vm;
                if let Some(profile) = vm.profile.as_mut() {
                    profile.count[pc] += 1;
                    profile.units[pc] += 1;
                }
                vm.exec_call_collection(*func, *argc, pc, budget)?;
                Ok(vm_dispatch::Flow::Next)
            }
            CsilInstr::CallPure { func, argc } => {
                let vm = &mut *self.inner.vm;
                if let Some(profile) = vm.profile.as_mut() {
                    profile.count[pc] += 1;
                    profile.units[pc] += 1;
                }
                vm.exec_call_pure_op(*func, *argc, pc, budget)?;
                Ok(vm_dispatch::Flow::Next)
            }
            _ => self.inner.exec(instr, pc, budget),
        }
    }
}

fn program() -> CsilProgram {
    let mut program = compile("indicator(\"Array math\")\nplot(close)\nplot(open)\nplot(high)\n");
    let main = &mut program.functions[0];
    main.code = vec![
        CsilInstr::CallCollection {
            func: CollectionFunc::ArrayGet,
            argc: 2,
        },
        CsilInstr::F64Sub,
        CsilInstr::CallPure {
            func: PureFunc::MathAbs,
            argc: 1,
        },
        CsilInstr::F64Add,
        CsilInstr::CallPure {
            func: PureFunc::MathLog,
            argc: 1,
        },
        CsilInstr::F64Add,
        CsilInstr::Return,
    ];
    main.source_map.truncate(7);
    main.budget_reference = None;
    main.max_stack = 5;
    program
}

fn compare(
    program: &CsilProgram,
    heap: &Heap,
    stack: &[Value],
    bar: u64,
    run: u64,
    limit: usize,
) -> u64 {
    let data = data_from_close(&[1.0]);
    let options = RunOptions {
        profile: true,
        max_stack_depth: limit,
        ..RunOptions::default()
    };
    let mut actual = Vm::new(program, &data, &options).unwrap();
    let mut original = Vm::new(program, &data, &options).unwrap();
    for vm in [&mut actual, &mut original] {
        vm.stack = stack.to_vec();
        vm.heap = heap.clone();
        vm.instructions_remaining = run;
        vm.call_args = vec![Value::String("retained scratch".into()), Value::Int(42)];
        vm.string_bytes_allocated = 37;
    }
    let (mut actual_budget, mut original_budget) = (bar, bar);
    let actual_result = vm_dispatch::dispatch_loop(
        &mut RunBarHost {
            vm: &mut actual,
            program,
        },
        &program.main().code,
        &mut actual_budget,
    );
    let original_result = vm_dispatch::dispatch_loop(
        &mut ReferenceHost {
            inner: RunBarHost {
                vm: &mut original,
                program,
            },
        },
        &program.main().code,
        &mut original_budget,
    );
    assert_eq!(actual_result, original_result);
    assert_eq!(actual.stack, original.stack);
    for (a, b) in actual.stack.iter().zip(&original.stack) {
        if let (Value::Float(a), Value::Float(b)) = (a, b) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }
    assert_eq!(actual.heap, original.heap);
    assert_eq!(actual.user_slots, original.user_slots);
    assert_eq!(actual.written, original.written);
    assert_eq!(actual.call_args, original.call_args);
    assert_eq!(
        actual.string_bytes_allocated,
        original.string_bytes_allocated
    );
    assert_eq!(
        (actual_budget, actual.instructions_remaining),
        (original_budget, original.instructions_remaining)
    );
    let a = actual.profile.as_ref().unwrap();
    let b = original.profile.as_ref().unwrap();
    assert_eq!(a.logical_counts(), b.logical_counts());
    assert_eq!(a.units, b.units);
    assert_eq!(b.combined_dispatches(), 0);
    let combined = a.combined_dispatches();
    assert!([0, 2, 5].contains(&combined));
    combined
}

fn array(value: Value) -> (Heap, HandleId) {
    let mut heap = Heap::new();
    let handle = heap
        .alloc(HeapObj::Array {
            elem_ty: ElemType::Float,
            items: vec![value],
        })
        .unwrap();
    (heap, handle)
}

#[test]
fn array_math_preserves_numeric_bits_missing_values_errors_and_full_frame() {
    let program = program();
    let mut values = vec![
        Value::Na,
        Value::Int(i64::MIN),
        Value::Int(1),
        Value::Bool(true),
        Value::String("invalid numeric".into()),
        Value::Ref(HandleId(7)),
    ];
    values.extend(
        [
            -f64::MAX,
            -3.0,
            -1.0,
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            2.0_f64.powi(-54),
            0.5,
            1.0,
            3.0,
            f64::MAX,
        ]
        .map(Value::Float),
    );
    for operand in 0..4 {
        for value in &values {
            let mut numeric = [
                Value::Float(-0.0),
                Value::Float(1.0),
                Value::Float(-3.0),
                Value::Float(0.5),
            ];
            numeric[operand] = value.clone();
            let (heap, handle) = array(numeric[3].clone());
            let stack = [
                numeric[0].clone(),
                numeric[1].clone(),
                numeric[2].clone(),
                Value::Ref(handle),
                Value::Int(0),
            ];
            for bar in 0..=8 {
                for run in 0..=8 {
                    for limit in [0, 1, 3, 4, 5, 6, 8] {
                        compare(&program, &heap, &stack, bar, run, limit);
                        let mut prefixed = vec![Value::String("preserved prefix".into())];
                        prefixed.extend_from_slice(&stack);
                        compare(&program, &heap, &prefixed, bar, run, limit);
                    }
                }
            }
        }
    }
    for values in [
        [Value::Na, Value::Na, Value::Na, Value::Na],
        [
            Value::Float(f64::MAX),
            Value::Float(f64::MAX),
            Value::Float(f64::MAX),
            Value::Float(-f64::MAX),
        ],
        [
            Value::Float(-0.0),
            Value::Float(-0.0),
            Value::Float(-0.0),
            Value::Float(0.0),
        ],
    ] {
        let (heap, handle) = array(values[3].clone());
        let stack = [
            values[0].clone(),
            values[1].clone(),
            values[2].clone(),
            Value::Ref(handle),
            Value::Int(0),
        ];
        assert_eq!(compare(&program, &heap, &stack, u64::MAX, u64::MAX, 5), 5);
    }
}

#[test]
fn array_math_delegates_invalid_receivers_full_width_indices_and_short_stacks() {
    let program = program();
    let (mut heap, array) = array(Value::Float(2.0));
    let object = heap
        .alloc(HeapObj::Object {
            table_identity: None,
            type_id: 0,
            fields: vec![],
        })
        .unwrap();
    let ref_array = heap
        .alloc(HeapObj::Array {
            elem_ty: ElemType::Object(0),
            items: vec![Value::Ref(object)],
        })
        .unwrap();
    for receiver in [
        Value::Na,
        Value::Ref(array),
        Value::Ref(object),
        Value::Ref(ref_array),
        Value::Ref(HandleId(999)),
        Value::Int(7),
    ] {
        for index in [
            Value::Na,
            Value::Bool(false),
            Value::Float(0.0),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(1),
            Value::Int(1_i64 << 32),
            Value::Int((1_i64 << 32) + 1),
            Value::Int(i64::MAX),
        ] {
            let stack = [
                Value::Float(0.0),
                Value::Float(1.0),
                Value::Float(-3.0),
                receiver.clone(),
                index,
            ];
            for bar in 0..=7 {
                for run in 0..=7 {
                    for limit in [0, 3, 4, 5, 6] {
                        compare(&program, &heap, &stack, bar, run, limit);
                    }
                }
            }
        }
    }
    for length in 0..5 {
        let stack = vec![Value::Na; length];
        for bar in 0..=7 {
            for run in 0..=7 {
                compare(&program, &heap, &stack, bar, run, 8);
            }
        }
    }
}

#[test]
fn array_math_delegates_malformed_arities_neighbors_and_truncated_tails() {
    let base = program();
    let (heap, handle) = array(Value::Float(2.0));
    let stack = [
        Value::Float(0.0),
        Value::Float(1.0),
        Value::Float(-3.0),
        Value::Ref(handle),
        Value::Int(0),
    ];
    for (pc, instr) in [
        (
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArrayGet,
                argc: 0,
            },
        ),
        (
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArrayGet,
                argc: 1,
            },
        ),
        (
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArrayGet,
                argc: 3,
            },
        ),
        (
            0,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArraySize,
                argc: 1,
            },
        ),
        (1, CsilInstr::F64Add),
        (
            2,
            CsilInstr::CallPure {
                func: PureFunc::MathAbs,
                argc: 0,
            },
        ),
        (
            2,
            CsilInstr::CallPure {
                func: PureFunc::MathSqrt,
                argc: 1,
            },
        ),
        (3, CsilInstr::F64Sub),
        (
            4,
            CsilInstr::CallPure {
                func: PureFunc::MathLog,
                argc: 2,
            },
        ),
        (
            4,
            CsilInstr::CallPure {
                func: PureFunc::MathLog10,
                argc: 1,
            },
        ),
        (5, CsilInstr::F64Sub),
    ] {
        let mut program = base.clone();
        program.functions[0].code[pc] = instr;
        for bar in 0..=8 {
            for run in 0..=8 {
                for limit in [0, 3, 4, 5, 6] {
                    compare(&program, &heap, &stack, bar, run, limit);
                }
            }
        }
    }
    for length in 1..=6 {
        let mut program = base.clone();
        program.functions[0].code.truncate(length);
        compare(&program, &heap, &stack, 20, 20, 5);
    }
}
