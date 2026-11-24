//! Compare the shipped pair path with the original two-instruction handler,
//! including failures between the two instructions and their complete frame.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, vm_slots, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::{ElemType, HandleId, HeapObj, Value};

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
        if let CsilInstr::ReadUserSlot { slot, offset } = instr {
            // The original arm, without the adjacent-field optimization.
            let vm = &mut *self.inner.vm;
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            let value = vm_slots::slot_read(
                &vm.user_slots,
                vm.bar,
                *offset as usize,
                vm.slot_mask,
                *slot as usize,
            )
            .unwrap_or(Value::Na);
            vm.push(value, pc)?;
            Ok(vm_dispatch::Flow::Next)
        } else {
            self.inner.exec(instr, pc, budget)
        }
    }
}

fn program() -> CsilProgram {
    let mut program = compile("indicator(\"Slot field pairs\")\nx = close\nplot(x)\n");
    let main = &mut program.functions[0];
    main.code = vec![
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
        CsilInstr::GetField { field_idx: 0 },
        CsilInstr::Return,
    ];
    main.source_map.truncate(3);
    main.budget_reference = None;
    main.max_stack = 2;
    program
}

#[test]
fn profiling_reserves_its_diagnostic_cell_at_the_memory_limit_boundary() {
    let program = compile("indicator(\"Profile reservation\")\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let minimum = |profile| {
        let mut low = 0;
        let mut high = 4 * 1024 * 1024;
        while low < high {
            let middle = low + (high - low) / 2;
            let options = RunOptions {
                profile,
                max_vm_bytes_per_run: middle,
                ..RunOptions::default()
            };
            match Vm::new(&program, &data, &options) {
                Ok(_) => high = middle,
                Err(error) => {
                    assert_eq!(error.kind, crate::RuntimeErrorKind::VmMemoryLimitExceeded);
                    low = middle + 1;
                }
            }
        }
        low
    };
    // Observe admission through the constructor rather than reusing the
    // reservation calculator. Both logical arrays and the diagnostic cell
    // must fit; the diagnostic must not appear as another logical PC.
    assert_eq!(
        minimum(true) - minimum(false),
        (program.main().code.len() * 3 + 1) * 8
    );
    let options = RunOptions {
        profile: true,
        ..RunOptions::default()
    };
    let vm = Vm::new(&program, &data, &options).unwrap();
    let profile = vm.profile.as_ref().unwrap();
    assert_eq!(profile.logical_counts().len(), program.main().code.len());
    assert_eq!(profile.combined_dispatches(), 0);
    assert_eq!(profile.skipped_by_pc(), vec![0; program.main().code.len()]);
}

#[test]
fn slot_field_pairs_match_original_frames_profiles_and_failure_order() {
    let data = data_from_close(&[1.0, 2.0, 3.0]);
    for initial_bar_budget in [0, 1, 2, 3, 10] {
        for initial_run_budget in [0, 1, 2, 3, 10] {
            for stack_limit in [0, 1, 2, 3] {
                for offset in [0, 1, 2, 3] {
                    for field in [0, 1, 2, 3, 4, 5, 6, 999] {
                        for receiver in 0..5 {
                            let mut program = program();
                            program.functions[0].code[0] =
                                CsilInstr::ReadUserSlot { slot: 0, offset };
                            program.functions[0].code[1] = CsilInstr::GetField { field_idx: field };
                            let options = RunOptions {
                                max_stack_depth: stack_limit,
                                profile: true,
                                ..RunOptions::default()
                            };
                            let mut fused = Vm::new(&program, &data, &options).unwrap();
                            let mut original = Vm::new(&program, &data, &options).unwrap();
                            for vm in [&mut fused, &mut original] {
                                let child = vm
                                    .heap
                                    .alloc(HeapObj::Object {
                                        table_identity: None,
                                        type_id: 0,
                                        fields: vec![Value::Int(27)],
                                    })
                                    .unwrap();
                                let object = vm
                                    .heap
                                    .alloc(HeapObj::Object {
                                        table_identity: None,
                                        type_id: 0,
                                        fields: vec![
                                            Value::Na,
                                            Value::Bool(true),
                                            Value::Int(i64::MIN),
                                            Value::Float(-0.0),
                                            Value::Color {
                                                r: 1,
                                                g: 2,
                                                b: 3,
                                                t: 4.0,
                                                input_origin: None,
                                            },
                                            Value::Ref(child),
                                            Value::String("string field".into()),
                                        ],
                                    })
                                    .unwrap();
                                let array = vm
                                    .heap
                                    .alloc(HeapObj::Array {
                                        elem_ty: ElemType::Float,
                                        items: vec![],
                                    })
                                    .unwrap();
                                let value = match receiver {
                                    0 => Value::Ref(object),
                                    1 => Value::Na,
                                    2 => Value::Bool(true),
                                    3 => Value::Ref(HandleId(999)),
                                    _ => Value::Ref(array),
                                };
                                vm.bar = 2;
                                vm.user_slots[0].fill(value);
                                vm.stack = vec![Value::Float(-0.0)];
                                vm.instructions_remaining = initial_run_budget;
                            }
                            let (mut b_fused, mut b_original) =
                                (initial_bar_budget, initial_bar_budget);
                            let result_fused = vm_dispatch::dispatch_loop(
                                &mut RunBarHost {
                                    vm: &mut fused,
                                    program: &program,
                                },
                                &program.main().code,
                                &mut b_fused,
                            );
                            let result_original = vm_dispatch::dispatch_loop(
                                &mut ReferenceHost {
                                    inner: RunBarHost {
                                        vm: &mut original,
                                        program: &program,
                                    },
                                },
                                &program.main().code,
                                &mut b_original,
                            );
                            assert_eq!(result_fused, result_original);
                            assert_eq!(fused.stack, original.stack);
                            let combined = u64::from(
                                initial_bar_budget >= 2
                                    && initial_run_budget >= 2
                                    && stack_limit > 1
                                    && offset <= 2
                                    && receiver == 0
                                    && field < 6,
                            );
                            assert_eq!(
                                fused.profile.as_ref().unwrap().combined_dispatches(),
                                combined
                            );
                            assert_eq!(original.profile.as_ref().unwrap().combined_dispatches(), 0);
                            for (a, b) in fused.stack.iter().zip(&original.stack) {
                                if let (Value::Float(a), Value::Float(b)) = (a, b) {
                                    assert_eq!(a.to_bits(), b.to_bits());
                                }
                            }
                            assert_eq!(fused.user_slots, original.user_slots);
                            assert_eq!(fused.written, original.written);
                            assert_eq!(fused.heap, original.heap);
                            assert_eq!(
                                (
                                    b_fused,
                                    fused.instructions_remaining,
                                    fused.string_bytes_allocated
                                ),
                                (
                                    b_original,
                                    original.instructions_remaining,
                                    original.string_bytes_allocated
                                )
                            );
                            assert_eq!(
                                fused
                                    .profile
                                    .as_ref()
                                    .map(|p| (p.logical_counts(), &p.units)),
                                original
                                    .profile
                                    .as_ref()
                                    .map(|p| (p.logical_counts(), &p.units))
                            );
                        }
                    }
                }
            }
        }
    }
}
