//! Attribute skipped tails by counting actual dispatch-host calls, including
//! incomplete fusion and wire compatibility.

use super::{compile, data_from_close};
use crate::{
    vm_charge, vm_dispatch, RunBarHost, RunOptions, RuntimeError, RuntimeErrorKind, Vm, VmProfile,
};
use pinewasm_csil::{CollectionFunc, CsilInstr, CsilProgram, F64Bits, PureFunc};
use pinewasm_mir::{ElemType, HeapObj, Value};
use std::mem::size_of;

struct CountingHost<'a> {
    inner: RunBarHost<'a>,
    physical: Vec<u64>,
}

impl vm_dispatch::DispatchHost for CountingHost<'_> {
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
        instruction: &CsilInstr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<vm_dispatch::Flow, RuntimeError> {
        self.physical[pc] += 1;
        self.inner.exec(instruction, pc, budget)
    }
}

fn program(code: Vec<CsilInstr>) -> CsilProgram {
    let mut program = compile(
        "indicator(\"Dispatch attribution\")\na = close\nb = open\nplot(a)\nplot(b)\nplot(a + b)\nplot(a - b)\nplot(a * b)\n",
    );
    assert!(program.series_slots.len() >= 2);
    let main = &mut program.functions[0];
    assert!(main.source_map.len() >= code.len());
    main.source_map.truncate(code.len());
    main.max_stack = 16;
    main.budget_reference = None;
    main.code = code;
    program
}

fn read(slot: u32) -> CsilInstr {
    CsilInstr::ReadUserSlot { slot, offset: 0 }
}

fn pure(func: PureFunc, argc: u8) -> CsilInstr {
    CsilInstr::CallPure { func, argc }
}

struct DispatchSnapshot {
    logical: Vec<u64>,
    skipped: Vec<u64>,
    result: Result<(), RuntimeError>,
}

fn dispatch(
    program: &CsilProgram,
    stack: Vec<Value>,
    bar_budget: u64,
    run_budget: u64,
    setup: impl FnOnce(&mut Vm),
) -> DispatchSnapshot {
    let data = data_from_close(&[1.0]);
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let mut vm = Vm::new(program, &data, &options).unwrap();
    // The synthetic entry frames intentionally seed already-computed operands,
    // as in the existing fast-path differential tests.
    vm.stack = stack;
    vm.stack.reserve(16);
    vm.instructions_remaining = run_budget;
    setup(&mut vm);
    let mut host = CountingHost {
        inner: RunBarHost {
            vm: &mut vm,
            program,
        },
        physical: vec![0; program.main().code.len()],
    };
    let mut budget = bar_budget;
    let result = vm_dispatch::dispatch_loop(&mut host, &program.main().code, &mut budget);
    let physical = host.physical;
    let counters = vm.profile.as_ref().unwrap();
    let logical = counters.logical_counts().to_vec();
    let skipped = counters.skipped_by_pc().to_vec();
    assert_eq!(skipped.len(), logical.len());
    assert_eq!(skipped.iter().sum::<u64>(), counters.combined_dispatches());
    for pc in 0..logical.len() {
        assert!(
            skipped[pc] <= logical[pc],
            "skipped exceeds logical at {pc}"
        );
        assert_eq!(
            logical[pc],
            physical[pc] + skipped[pc],
            "physical dispatch conservation at {pc}"
        );
        assert!(counters.units[pc] >= logical[pc]);
    }
    DispatchSnapshot {
        logical,
        skipped,
        result,
    }
}

fn assert_successful_tails(snapshot: DispatchSnapshot, expected: &[u64]) {
    assert!(snapshot.result.is_ok(), "{:?}", snapshot.result);
    assert_eq!(snapshot.skipped, expected);
    assert_eq!(snapshot.logical, vec![1; expected.len()]);
}

#[test]
fn every_existing_fast_path_attributes_the_actual_tail_pc() {
    let slots = program(vec![
        read(0),
        CsilInstr::GetField { field_idx: 0 },
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(&slots, vec![], 100, 100, |vm| {
            let object = vm
                .heap
                .alloc(HeapObj::Object {
                    table_identity: None,
                    type_id: 0,
                    fields: vec![Value::Float(3.0)],
                })
                .unwrap();
            vm.user_slots[0].fill(Value::Ref(object));
        }),
        &[0, 1, 0],
    );

    let pow = program(vec![
        CsilInstr::PushF64(F64Bits::from(2.0)),
        pure(PureFunc::MathPow, 2),
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(&pow, vec![Value::Float(3.0)], 100, 100, |_| {}),
        &[0, 1, 0],
    );

    let store = program(vec![
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        read(0),
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(&store, vec![Value::Float(3.0)], 100, 100, |_| {}),
        &[0, 1, 0],
    );

    let math = program(vec![
        pure(PureFunc::MathAbs, 1),
        CsilInstr::F64Add,
        pure(PureFunc::MathLog, 1),
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(
            &math,
            vec![Value::Float(1.0), Value::Float(-3.0)],
            100,
            100,
            |_| {},
        ),
        &[0, 1, 1, 0],
    );

    let array = program(vec![
        CsilInstr::CallCollection {
            func: CollectionFunc::ArrayGet,
            argc: 2,
        },
        CsilInstr::F64Sub,
        pure(PureFunc::MathAbs, 1),
        CsilInstr::F64Add,
        pure(PureFunc::MathLog, 1),
        CsilInstr::F64Add,
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(
            &array,
            vec![
                Value::Float(7.0),
                Value::Float(1.0),
                Value::Float(9.0),
                Value::Na,
                Value::Int(0),
            ],
            100,
            100,
            |vm| {
                let array = vm
                    .heap
                    .alloc(HeapObj::Array {
                        elem_ty: ElemType::Float,
                        items: vec![Value::Float(3.0)],
                    })
                    .unwrap();
                vm.stack[3] = Value::Ref(array);
            },
        ),
        &[0, 1, 1, 1, 1, 1, 0],
    );

    let loop_bound = program(vec![
        CsilInstr::PushI64(0),
        CsilInstr::CmpGe,
        read(0),
        read(1),
        CsilInstr::CmpLe,
        read(0),
        read(1),
        CsilInstr::CmpGe,
        CsilInstr::Select,
        CsilInstr::Return,
    ]);
    assert_successful_tails(
        dispatch(&loop_bound, vec![Value::Int(1)], 100, 100, |vm| {
            vm.user_slots[0].fill(Value::Int(2));
            vm.user_slots[1].fill(Value::Int(7));
        }),
        &[0, 1, 1, 1, 1, 1, 1, 1, 1, 0],
    );
}

#[test]
fn composed_array_store_read_counts_each_skipped_head_and_tail_independently() {
    let program = program(vec![
        CsilInstr::CallCollection {
            func: CollectionFunc::ArrayGet,
            argc: 2,
        },
        CsilInstr::F64Sub,
        pure(PureFunc::MathAbs, 1),
        CsilInstr::F64Add,
        pure(PureFunc::MathLog, 1),
        CsilInstr::F64Add,
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        read(0),
        CsilInstr::Return,
    ]);
    for bar in 5..=9 {
        for run in 5..=9 {
            let snapshot = dispatch(
                &program,
                vec![
                    Value::Float(7.0),
                    Value::Float(1.0),
                    Value::Float(9.0),
                    Value::Na,
                    Value::Int(0),
                ],
                bar,
                run,
                |vm| {
                    let array = vm
                        .heap
                        .alloc(HeapObj::Array {
                            elem_ty: ElemType::Float,
                            items: vec![Value::Float(3.0)],
                        })
                        .unwrap();
                    vm.stack[3] = Value::Ref(array);
                },
            );
            // CountingHost checks logical = actual dispatches
            // + skipped for every PC, including this newly skipped Store head.
            let charged = bar.min(run);
            let complete_pair = u64::from(charged >= 8);
            assert_eq!(&snapshot.skipped[6..8], &[complete_pair; 2]);
            if charged >= 9 {
                assert_successful_tails(snapshot, &[0, 1, 1, 1, 1, 1, 1, 1, 0]);
            } else {
                let error = snapshot.result.unwrap_err();
                assert_eq!(error.instruction_offset, Some(charged as u32));
                assert_eq!(
                    error.kind,
                    if bar <= run {
                        RuntimeErrorKind::InstructionLimitExceeded
                    } else {
                        RuntimeErrorKind::RunInstructionLimitExceeded
                    }
                );
            }
        }
    }
}

#[test]
fn partial_store_read_fusion_never_counts_an_uncharged_boolean_branch() {
    let program = program(vec![
        CsilInstr::StoreUserSlotCurrent { slot: 0 },
        read(0),
        CsilInstr::JumpIfFalse { target: 3 },
        CsilInstr::Return,
    ]);
    for condition in [false, true] {
        for bar in 0..=4 {
            for run in 0..=4 {
                let snapshot = dispatch(&program, vec![Value::Bool(condition)], bar, run, |_| {});
                let charged = bar.min(run);
                assert_eq!(
                    snapshot.skipped,
                    vec![0, u64::from(charged >= 2), u64::from(charged >= 3), 0],
                    "bar={bar}, run={run}, condition={condition}"
                );
                if charged >= 4 {
                    assert!(snapshot.result.is_ok());
                } else {
                    let error = snapshot.result.unwrap_err();
                    assert_eq!(error.instruction_offset, Some(charged as u32));
                    assert!(matches!(
                        error.kind,
                        RuntimeErrorKind::InstructionLimitExceeded
                            | RuntimeErrorKind::RunInstructionLimitExceeded
                    ));
                }
            }
        }
    }
}

#[test]
fn profiling_memory_boundary_includes_every_pc_counter() {
    for source in [
        "indicator(\"Profile memory\")\nplot(close)\n",
        "indicator(\"Profile memory\")\nx = close\ny = open\nplot(x + y)\nplot(x - y)\n",
    ] {
        let program = compile(source);
        let data = data_from_close(&[1.0, 2.0, 3.0]);
        let plain = RunOptions::default();
        let plain_bytes = crate::vm_preallocation_bytes(
            &program,
            &data,
            &plain,
            data.bars(),
            program.history_ring() as usize,
        )
        .unwrap();
        let expected_counters = (3 * program.main().code.len() + 1) * size_of::<u64>();
        let exact = plain_bytes + expected_counters;
        let options = RunOptions {
            profile: true,
            max_vm_bytes_per_run: exact,
            ..plain.clone()
        };
        let allowed = crate::run(&program, &data, &options);
        assert!(allowed.errors.is_empty(), "{:?}", allowed.errors);
        assert_eq!(
            allowed.profile.unwrap().combined_dispatch_by_pc.len(),
            program.main().code.len()
        );
        let refused = crate::run(
            &program,
            &data,
            &RunOptions {
                max_vm_bytes_per_run: exact - 1,
                ..options
            },
        );
        assert_eq!(refused.bars_executed, 0);
        assert_eq!(
            refused.errors[0].kind,
            RuntimeErrorKind::VmMemoryLimitExceeded
        );
        assert_eq!(refused.errors[0].bar_index, None);
        let unprofiled = crate::run(
            &program,
            &data,
            &RunOptions {
                max_vm_bytes_per_run: plain_bytes,
                ..plain
            },
        );
        assert!(unprofiled.errors.is_empty());
        assert!(unprofiled.profile.is_none());
    }
}

#[test]
fn per_pc_wire_counters_round_trip_and_accept_older_profiles() {
    let program = compile("indicator(\"Profile wire\")\nplot(math.pow(close, 2.0))\n");
    let outcome = crate::run(
        &program,
        &data_from_close(&[1.0, 2.0, 3.0]),
        &RunOptions {
            profile: true,
            ..RunOptions::default()
        },
    );
    assert!(outcome.errors.is_empty());
    let profile = outcome.profile.unwrap();
    assert_eq!(profile.combined_dispatch_by_pc.len(), profile.count.len());
    assert!(profile.combined_dispatch_count > 0.0);
    assert_eq!(
        profile.combined_dispatch_by_pc.iter().sum::<f64>(),
        profile.combined_dispatch_count
    );
    for (skipped, count) in profile.combined_dispatch_by_pc.iter().zip(&profile.count) {
        assert!(*skipped >= 0.0 && skipped <= count);
    }
    let wire = serde_json::to_value(&profile).unwrap();
    assert_eq!(
        wire["combinedDispatchByPc"].as_array().unwrap().len(),
        profile.count.len()
    );
    assert_eq!(
        serde_json::from_value::<VmProfile>(wire.clone()).unwrap(),
        profile
    );
    let mut older = wire;
    older
        .as_object_mut()
        .unwrap()
        .remove("combinedDispatchByPc");
    let older: VmProfile = serde_json::from_value(older).unwrap();
    assert!(older.combined_dispatch_by_pc.is_empty());
    assert_eq!(
        older.combined_dispatch_count,
        profile.combined_dispatch_count
    );
    assert_eq!(older.count, profile.count);
    assert_eq!(older.units, profile.units);
}
