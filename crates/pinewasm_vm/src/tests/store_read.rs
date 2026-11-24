//! Store/current-read pairs compared with the unfused store arm and full frames.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, vm_slots, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
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
        if let CsilInstr::ReadUserSlot { slot, offset } = instr {
            if matches!(
                self.0.program.main().code.get(pc + 1),
                Some(CsilInstr::JumpIfFalse { .. })
            ) {
                // Keep this unfused branch read independent of the read/branch
                // fast path, just as its store is independent.
                let vm = &mut *self.0.vm;
                super::lane_rows::observe(vm, pc);
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
                return Ok(vm_dispatch::Flow::Next);
            }
        }
        if let CsilInstr::StoreUserSlotCurrent { slot } = instr {
            let vm = &mut *self.0.vm;
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            let value = vm.pop(pc)?;
            if !vm_slots::slot_store(
                &mut vm.user_slots,
                &mut vm.written,
                vm.bar,
                vm.slot_mask,
                *slot as usize,
                value,
            ) {
                return Err(vm.fail(
                    crate::RuntimeErrorKind::InvalidProgram,
                    format!("series slot {slot} out of range"),
                    pc,
                ));
            }
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
            input_origin: Some(7),
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
            (
                paint_a @ Value::Color {
                    input_origin: a, ..
                },
                paint_b @ Value::Color {
                    input_origin: b, ..
                },
            ) => {
                assert_eq!(paint_a, paint_b);
                assert_eq!(
                    a, b,
                    "optimized reads must preserve color selection identity"
                );
                assert_eq!(a, &Some(7));
            }
            _ => assert_eq!(a, b),
        }
    }
}

fn program(code: Vec<CsilInstr>) -> CsilProgram {
    let mut program =
        compile("indicator(\"Store read pairs\")\na = close\nb = open\nplot(a + b)\n");
    let main = &mut program.functions[0];
    main.source_map.truncate(code.len());
    main.max_stack = 6;
    main.budget_reference = None;
    main.code = code;
    program
}

fn compare(
    program: &CsilProgram,
    first: &Value,
    second: &Value,
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
        vm.user_slots[1].fill(second.clone());
        vm.bar = 2;
        vm.stack = match incoming {
            0 => vec![],
            1 => vec![first.clone()],
            _ => vec![Value::Float(-0.0), first.clone()],
        };
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
fn store_read_pairs_preserve_full_frames_aliases_and_every_resource_cut() {
    for read_slot in [0, 1] {
        let program = program(vec![
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot {
                slot: read_slot,
                offset: 0,
            },
            CsilInstr::Return,
        ]);
        for first in values() {
            for second in values() {
                for bar in [0, 1, 2, 3, u64::MAX] {
                    for run in [0, 1, 2, 3, u64::MAX] {
                        for limit in [0, 1, 2, 3] {
                            for incoming in [1, 2] {
                                let (actual, original) =
                                    compare(&program, &first, &second, bar, run, limit, incoming);
                                let read = if read_slot == 0 { &first } else { &second };
                                let eligible = bar >= 2
                                    && run >= 2
                                    && incoming <= limit
                                    && !matches!(read, Value::String(_) | Value::Ref(_));
                                assert_eq!(actual, u64::from(eligible));
                                assert_eq!(original, 0);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn invalid_stores_history_references_underflow_and_truncation_keep_original_paths() {
    let store = |slot| CsilInstr::StoreUserSlotCurrent { slot };
    let read = |slot, offset| CsilInstr::ReadUserSlot { slot, offset };
    for code in [
        vec![store(0)],
        vec![store(0), read(1, 0)],
        vec![store(0), read(1, 1), CsilInstr::Return],
        vec![store(u32::MAX), read(1, 0), CsilInstr::Return],
        vec![store(0), read(u32::MAX, 0), CsilInstr::Return],
        vec![store(0), read(1, 3), CsilInstr::Return],
        vec![
            store(0),
            read(1, 0),
            CsilInstr::GetField { field_idx: 0 },
            CsilInstr::Return,
        ],
        vec![
            store(0),
            read(0, 0),
            CsilInstr::GetField { field_idx: 0 },
            CsilInstr::Return,
        ],
    ] {
        let program = program(code);
        for value in values() {
            for bar in 0..=4 {
                for run in 0..=4 {
                    for limit in 0..=3 {
                        for incoming in [0, 1, 2] {
                            compare(&program, &value, &value, bar, run, limit, incoming);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn bool_branch_continuations_preserve_frames_and_every_resource_cut() {
    for read_slot in [0, 1] {
        for target in [3, 4] {
            let program = program(vec![
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot {
                    slot: read_slot,
                    offset: 0,
                },
                CsilInstr::JumpIfFalse { target },
                CsilInstr::Return,
                CsilInstr::Return,
            ]);
            for first in values() {
                for second in values() {
                    for bar in [0, 1, 2, 3, 4, u64::MAX] {
                        for run in [0, 1, 2, 3, 4, u64::MAX] {
                            for limit in [0, 1, 2, 3] {
                                for incoming in [1, 2] {
                                    let (actual, original) = compare(
                                        &program, &first, &second, bar, run, limit, incoming,
                                    );
                                    let read = if read_slot == 0 { &first } else { &second };
                                    let pair = bar >= 2
                                        && run >= 2
                                        && incoming <= limit
                                        && !matches!(read, Value::String(_) | Value::Ref(_));
                                    let branch = pair
                                        && bar >= 3
                                        && run >= 3
                                        && matches!(read, Value::Bool(_));
                                    assert_eq!(actual, u64::from(pair) + u64::from(branch));
                                    assert_eq!(original, 0);
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
fn bool_branch_continuations_keep_alternate_entries_and_invalid_targets() {
    let store = CsilInstr::StoreUserSlotCurrent { slot: 0 };
    let read = CsilInstr::ReadUserSlot { slot: 1, offset: 0 };
    let mut cases = vec![
        vec![
            store.clone(),
            read.clone(),
            CsilInstr::JumpIfFalse { target: u32::MAX },
        ],
        vec![
            store.clone(),
            read.clone(),
            CsilInstr::JumpIfFalse { target: 3 },
        ],
        vec![
            store.clone(),
            read.clone(),
            CsilInstr::JumpIfFalse { target: 0 },
        ],
    ];
    for entry in [2, 3] {
        cases.push(vec![
            CsilInstr::Jump { target: entry },
            store.clone(),
            read.clone(),
            CsilInstr::JumpIfFalse { target: 4 },
            CsilInstr::Return,
        ]);
    }
    for code in cases {
        let program = program(code);
        for first in values() {
            for second in values() {
                for bar in 0..=5 {
                    for run in 0..=5 {
                        for incoming in [0, 1, 2] {
                            compare(&program, &first, &second, bar, run, 3, incoming);
                        }
                    }
                }
            }
        }
    }
}
