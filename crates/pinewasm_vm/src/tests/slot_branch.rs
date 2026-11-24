//! Equivalence checks against the unfused read arm, including stops between
//! the pair, malformed operands/targets, and kept stack capacity.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, vm_slots, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::{HandleId, Value};

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
        if let CsilInstr::ReadUserSlot { slot, offset } = instr {
            let vm = &mut *self.0.vm;
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
            self.0.exec(instr, pc, budget)
        }
    }
}

fn program() -> CsilProgram {
    let mut program = compile("indicator(\"Read branch\")\nx = close\nplot(x)\n");
    let main = &mut program.functions[0];
    main.code = vec![
        CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
        CsilInstr::JumpIfFalse { target: 3 },
        CsilInstr::PushI64(42),
        CsilInstr::Return,
    ];
    main.source_map.truncate(4);
    main.budget_reference = None;
    main.max_stack = 2;
    program
}

fn compare(fused: &Vm, original: &Vm) {
    assert_eq!(fused.stack, original.stack);
    assert_eq!(fused.stack.capacity(), original.stack.capacity());
    assert_eq!(fused.user_slots, original.user_slots);
    assert_eq!(fused.written, original.written);
    assert_eq!(fused.heap, original.heap);
    assert_eq!(
        fused.instructions_remaining,
        original.instructions_remaining
    );
    assert_eq!(fused.bytes_reserved, original.bytes_reserved);
    assert_eq!(
        fused.string_bytes_allocated,
        original.string_bytes_allocated
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
    if let Some(profile) = &fused.profile {
        assert_eq!(
            profile.combined_dispatches(),
            profile.skipped_by_pc().iter().sum::<u64>()
        );
        for (logical, skipped) in profile.logical_counts().iter().zip(profile.skipped_by_pc()) {
            assert!(skipped <= logical);
        }
    }
}

#[test]
fn read_branch_matches_original_fuel_errors_frames_and_stack_allocation() {
    let data = data_from_close(&[1.0, 2.0, 3.0]);
    let template = program();
    for profile in [false, true] {
        for bar_budget in [0, 1, 2, 3, 10, u64::MAX] {
            for run_budget in [0, 1, 2, 3, 10, u64::MAX] {
                for stack_limit in [0, 1, 2, 3] {
                    for offset in [0, 2, 3] {
                        for value in [
                            Value::Na,
                            Value::Bool(false),
                            Value::Bool(true),
                            Value::Int(0),
                            Value::Float(-0.0),
                            Value::String("type error after pop".into()),
                            Value::Ref(HandleId(999)),
                        ] {
                            for capacity in [0, 1, 4] {
                                let mut program = template.clone();
                                program.functions[0].code[0] =
                                    CsilInstr::ReadUserSlot { slot: 0, offset };
                                let options = RunOptions {
                                    profile,
                                    max_stack_depth: stack_limit,
                                    ..RunOptions::default()
                                };
                                let mut fused = Vm::new(&program, &data, &options).unwrap();
                                let mut original = Vm::new(&program, &data, &options).unwrap();
                                for vm in [&mut fused, &mut original] {
                                    vm.bar = 2;
                                    vm.user_slots[0].fill(value.clone());
                                    vm.stack = Vec::with_capacity(capacity);
                                    // Include an untouched prefix and the full-capacity push case.
                                    if capacity == 1 {
                                        vm.stack.push(Value::Int(-7));
                                    }
                                    vm.instructions_remaining = run_budget;
                                }
                                let (mut bf, mut bo) = (bar_budget, bar_budget);
                                let rf = vm_dispatch::dispatch_loop(
                                    &mut RunBarHost {
                                        vm: &mut fused,
                                        program: &program,
                                    },
                                    &program.main().code,
                                    &mut bf,
                                );
                                let ro = vm_dispatch::dispatch_loop(
                                    &mut ReferenceHost(RunBarHost {
                                        vm: &mut original,
                                        program: &program,
                                    }),
                                    &program.main().code,
                                    &mut bo,
                                );
                                assert_eq!(rf, ro);
                                assert_eq!(bf, bo);
                                compare(&fused, &original);
                                if let Some(p) = &fused.profile {
                                    let eligible = bar_budget >= 2
                                        && run_budget >= 2
                                        && stack_limit > usize::from(capacity == 1)
                                        && capacity == 4
                                        && (offset > 2
                                            || matches!(value, Value::Na | Value::Bool(_)));
                                    assert_eq!(p.skipped_by_pc(), &[0, u64::from(eligible), 0, 0]);
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
fn read_branch_preserves_target_errors_backward_loops_and_direct_tail_entry() {
    let data = data_from_close(&[1.0]);
    for target in [0, 1, 2, 3, 4, u32::MAX] {
        for direct in [false, true] {
            for condition in [Value::Na, Value::Bool(false), Value::Bool(true)] {
                let mut program = program();
                program.functions[0].code[1] = CsilInstr::JumpIfFalse { target };
                if direct {
                    program.functions[0].code[0] = CsilInstr::Jump { target: 1 };
                }
                let options = RunOptions {
                    profile: true,
                    ..RunOptions::default()
                };
                let mut fused = Vm::new(&program, &data, &options).unwrap();
                let mut original = Vm::new(&program, &data, &options).unwrap();
                for vm in [&mut fused, &mut original] {
                    vm.user_slots[0].fill(condition.clone());
                    vm.stack = vec![condition.clone()];
                    vm.instructions_remaining = 11;
                }
                let (mut bf, mut bo) = (11, 11);
                let rf = vm_dispatch::dispatch_loop(
                    &mut RunBarHost {
                        vm: &mut fused,
                        program: &program,
                    },
                    &program.main().code,
                    &mut bf,
                );
                let ro = vm_dispatch::dispatch_loop(
                    &mut ReferenceHost(RunBarHost {
                        vm: &mut original,
                        program: &program,
                    }),
                    &program.main().code,
                    &mut bo,
                );
                assert_eq!(rf, ro);
                assert_eq!(bf, bo);
                compare(&fused, &original);
                if direct {
                    assert_eq!(fused.profile.as_ref().unwrap().combined_dispatches(), 0);
                }
            }
        }
    }
}
