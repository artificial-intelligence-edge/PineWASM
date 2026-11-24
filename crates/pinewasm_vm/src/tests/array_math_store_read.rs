//! Compare composition with the existing array-math and store-read fast paths.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CollectionFunc, CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::{ElemType, HandleId, HeapObj, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Existing array fusion, with a test-only lookahead view hiding its newly
/// composable store. Actual dispatch, stores, reads and source attribution
/// continue to use the original program.
struct BaselineHost<'a> {
    inner: RunBarHost<'a>,
    prefix_only: &'a CsilProgram,
}

impl vm_dispatch::DispatchHost for BaselineHost<'_> {
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
        if matches!(
            instr,
            CsilInstr::CallCollection {
                func: CollectionFunc::ArrayGet,
                argc: 2
            }
        ) {
            let vm = &mut *self.inner.vm;
            super::lane_rows::observe(vm, pc);
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            if let Some(flow) = vm.exec_array_math_chain(self.prefix_only, pc, budget) {
                return Ok(flow);
            }
            vm.exec_call_collection(CollectionFunc::ArrayGet, 2, pc, budget)?;
            Ok(vm_dispatch::Flow::Next)
        } else {
            self.inner.exec(instr, pc, budget)
        }
    }
}

#[derive(Clone, Copy)]
enum Damage {
    None,
    EmptyTargetRing,
    MissingWrittenFlag,
}

struct Limits {
    bar: u64,
    run: u64,
    depth: usize,
    spare: usize,
    profile: bool,
    damage: Damage,
}

fn program(store_slot: u32, read_slot: u32, branch: bool) -> CsilProgram {
    let mut program =
        compile("indicator(\"Composed array math\")\na = close\nb = open\nplot(a + b)\n");
    let mut code = vec![
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
        CsilInstr::StoreUserSlotCurrent { slot: store_slot },
        CsilInstr::ReadUserSlot {
            slot: read_slot,
            offset: 0,
        },
    ];
    if branch {
        code.push(CsilInstr::JumpIfFalse { target: 10 });
        code.push(CsilInstr::PushI64(42));
    }
    code.push(CsilInstr::Return);
    let main = &mut program.functions[0];
    main.source_map.truncate(code.len());
    main.code = code;
    main.budget_reference = None;
    main.max_stack = 8;
    program
}

fn same_values(left: &[Value], right: &[Value]) {
    assert_eq!(left.len(), right.len());
    for (left, right) in left.iter().zip(right) {
        match (left, right) {
            (Value::Float(left), Value::Float(right)) => {
                assert_eq!(left.to_bits(), right.to_bits());
            }
            (
                color_left @ Value::Color {
                    input_origin: left_origin,
                    ..
                },
                color_right @ Value::Color {
                    input_origin: right_origin,
                    ..
                },
            ) => {
                assert_eq!(color_left, color_right);
                assert_eq!(left_origin, right_origin);
            }
            _ => assert_eq!(left, right),
        }
    }
}

fn compare(program: &CsilProgram, stack: &[Value], read: &Value, limits: Limits) -> u64 {
    let mut prefix_only = program.clone();
    for instr in &mut prefix_only.functions[0].code {
        if matches!(instr, CsilInstr::StoreUserSlotCurrent { .. }) {
            *instr = CsilInstr::Return;
        }
    }
    let data = data_from_close(&[1.0, 2.0, 3.0]);
    let options = RunOptions {
        profile: limits.profile,
        max_stack_depth: limits.depth,
        ..RunOptions::default()
    };
    let mut composed = Vm::new(program, &data, &options).unwrap();
    let mut baseline = Vm::new(program, &data, &options).unwrap();
    for vm in [&mut composed, &mut baseline] {
        let array = vm
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items: vec![Value::Float(2.0)],
            })
            .unwrap();
        assert_eq!(array, HandleId(0));
        vm.user_slots[0].fill(Value::String("overwritten cell".into()));
        vm.user_slots[1].fill(read.clone());
        vm.bar = 2;
        vm.stack = Vec::with_capacity(stack.len() + limits.spare);
        vm.stack.extend_from_slice(stack);
        vm.instructions_remaining = limits.run;
        vm.call_args = vec![Value::String("retained scratch".into())];
        vm.string_bytes_allocated = 37;
        match limits.damage {
            Damage::None => {}
            Damage::EmptyTargetRing => vm.user_slots[0].clear(),
            Damage::MissingWrittenFlag => vm.written.clear(),
        }
    }
    let (mut a, mut b) = (limits.bar, limits.bar);
    let ra = catch_unwind(AssertUnwindSafe(|| {
        vm_dispatch::dispatch_loop(
            &mut RunBarHost {
                vm: &mut composed,
                program,
            },
            &program.main().code,
            &mut a,
        )
    }));
    let rb = catch_unwind(AssertUnwindSafe(|| {
        vm_dispatch::dispatch_loop(
            &mut BaselineHost {
                inner: RunBarHost {
                    vm: &mut baseline,
                    program,
                },
                prefix_only: &prefix_only,
            },
            &program.main().code,
            &mut b,
        )
    }));
    match (ra, rb) {
        (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
        (Err(_), Err(_)) => {}
        _ => panic!("composition changed whether the malformed frame panics"),
    }
    assert_eq!(a, b);
    same_values(&composed.stack, &baseline.stack);
    assert_eq!(composed.stack.capacity(), baseline.stack.capacity());
    assert_eq!(composed.user_slots.len(), baseline.user_slots.len());
    for (actual, expected) in composed.user_slots.iter().zip(&baseline.user_slots) {
        same_values(actual, expected);
    }
    assert_eq!(composed.written, baseline.written);
    assert_eq!(composed.heap, baseline.heap);
    same_values(&composed.call_args, &baseline.call_args);
    assert_eq!(
        (
            composed.instructions_remaining,
            composed.bytes_reserved,
            composed.string_bytes_allocated
        ),
        (
            baseline.instructions_remaining,
            baseline.bytes_reserved,
            baseline.string_bytes_allocated
        )
    );
    match (&composed.profile, &baseline.profile) {
        (Some(actual), Some(expected)) => {
            assert_eq!(actual.logical_counts(), expected.logical_counts());
            assert_eq!(actual.units, expected.units);
            assert_eq!(
                actual.combined_dispatches(),
                actual.skipped_by_pc().iter().sum::<u64>()
            );
            let mut additional = 0;
            for (pc, (actual_skip, expected_skip)) in actual
                .skipped_by_pc()
                .iter()
                .zip(expected.skipped_by_pc())
                .enumerate()
            {
                assert!(*actual_skip <= actual.logical_counts()[pc]);
                if matches!(
                    program.main().code[pc],
                    CsilInstr::StoreUserSlotCurrent { .. }
                ) {
                    assert_eq!(*expected_skip, 0);
                    additional += *actual_skip;
                } else {
                    assert_eq!(actual_skip, expected_skip);
                }
            }
            assert_eq!(
                actual.combined_dispatches(),
                expected.combined_dispatches() + additional
            );
            additional
        }
        (None, None) => 0,
        _ => panic!("composition changed profiling enablement"),
    }
}

fn operands() -> Vec<Value> {
    vec![
        Value::Float(-0.0),
        Value::Float(1.0),
        Value::Float(-3.0),
        Value::Ref(HandleId(0)),
        Value::Int(0),
    ]
}

#[test]
fn composition_preserves_both_fuel_ceilings_all_frames_and_read_kinds() {
    for read_slot in [0, 1] {
        let program = program(0, read_slot, false);
        for read in [
            Value::Na,
            Value::Bool(false),
            Value::Float(-0.0),
            Value::String("allocating read delegates".into()),
            Value::Ref(HandleId(0)),
        ] {
            for prefix in [false, true] {
                let mut stack = operands();
                if prefix {
                    stack.insert(0, Value::String("untouched prefix".into()));
                }
                for bar in 0..=10 {
                    for run in 0..=10 {
                        for depth in [0, 4, 6] {
                            for profile in [false, true] {
                                compare(
                                    &program,
                                    &stack,
                                    &read,
                                    Limits {
                                        bar,
                                        run,
                                        depth,
                                        spare: if bar % 2 == 0 { 0 } else { 16 },
                                        profile,
                                        damage: Damage::None,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn composition_adds_only_the_store_dispatch_and_keeps_bool_continuations() {
    for read_slot in [0, 1] {
        for branch in [false, true] {
            let program = program(0, read_slot, branch);
            for read in [
                Value::Na,
                Value::Bool(false),
                Value::Bool(true),
                Value::Int(i64::MAX),
                Value::Float(f64::from_bits(0x7ff8_0000_0000_0021)),
                Value::Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    t: 4.0,
                    input_origin: Some(7),
                },
            ] {
                for accumulator in [
                    Value::Na,
                    Value::Float(-0.0),
                    Value::Float(f64::from_bits(1)),
                ] {
                    let mut stack = operands();
                    stack[0] = accumulator;
                    for spare in [0, 16] {
                        assert_eq!(
                            compare(
                                &program,
                                &stack,
                                &read,
                                Limits {
                                    bar: u64::MAX,
                                    run: u64::MAX,
                                    depth: 5,
                                    spare,
                                    profile: true,
                                    damage: Damage::None,
                                }
                            ),
                            1
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn composed_branch_preserves_every_finite_cut_and_retained_capacity() {
    for read_slot in [0, 1] {
        let program = program(0, read_slot, true);
        for read in [Value::Bool(false), Value::Bool(true), Value::Na] {
            for prefix in [false, true] {
                let mut stack = operands();
                if prefix {
                    stack.insert(0, Value::String("branch prefix".into()));
                }
                for bar in 5..=11 {
                    for run in 5..=11 {
                        for spare in [0, 16] {
                            for profile in [false, true] {
                                let additional = compare(
                                    &program,
                                    &stack,
                                    &read,
                                    Limits {
                                        bar,
                                        run,
                                        depth: stack.len(),
                                        spare,
                                        profile,
                                        damage: Damage::None,
                                    },
                                );
                                // Six array/math units precede the pair. The
                                // Store head is skipped only when its own and
                                // the following Read charge both fit; a third
                                // remaining unit is needed for the branch.
                                assert_eq!(additional, u64::from(profile && bar >= 8 && run >= 8));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn composition_refusals_resume_at_original_store_and_read_failures() {
    let template = program(0, 1, false);
    let mut cases = vec![(program(u32::MAX, 1, false), Damage::None)];
    cases.push((template.clone(), Damage::EmptyTargetRing));
    let mut history = template.clone();
    history.functions[0].code[7] = CsilInstr::ReadUserSlot { slot: 1, offset: 1 };
    cases.push((history, Damage::None));
    let mut missing_read = template.clone();
    missing_read.functions[0].code[7] = CsilInstr::ReadUserSlot {
        slot: u32::MAX,
        offset: 0,
    };
    cases.push((missing_read, Damage::None));
    let mut truncated = template.clone();
    truncated.functions[0].code.truncate(7);
    truncated.functions[0].source_map.truncate(7);
    cases.push((truncated, Damage::None));
    let mut neighbor = template;
    neighbor.functions[0].code[7] = CsilInstr::GetField { field_idx: 0 };
    cases.push((neighbor, Damage::None));
    for (program, damage) in cases {
        for read in [
            Value::Na,
            Value::String("deferred clone".into()),
            Value::Ref(HandleId(0)),
        ] {
            for bar in 5..=10 {
                for run in 5..=10 {
                    compare(
                        &program,
                        &operands(),
                        &read,
                        Limits {
                            bar,
                            run,
                            depth: 5,
                            spare: 0,
                            profile: true,
                            damage,
                        },
                    );
                }
            }
        }
    }
    // A malformed frame writes its partial state, then panics on the
    // missing flag. Refusal must preserve that partial state and charge PC.
    compare(
        &program(0, 1, false),
        &operands(),
        &Value::Bool(true),
        Limits {
            bar: 9,
            run: 9,
            depth: 5,
            spare: 0,
            profile: true,
            damage: Damage::MissingWrittenFlag,
        },
    );
}

#[test]
fn composition_keeps_every_tail_independently_enterable() {
    for entry in 1..=8 {
        let mut program = program(0, 1, false);
        program.functions[0]
            .code
            .insert(0, CsilInstr::Jump { target: entry + 1 });
        let mut stack = match entry {
            1 => vec![
                Value::Float(0.0),
                Value::Float(1.0),
                Value::Float(-3.0),
                Value::Float(2.0),
            ],
            2 | 3 => vec![Value::Float(0.0), Value::Float(1.0), Value::Float(2.0)],
            4 | 5 => vec![Value::Float(0.0), Value::Float(2.0)],
            6 => vec![Value::Float(2.0)],
            _ => vec![],
        };
        stack.insert(0, Value::String("retained direct-entry prefix".into()));
        for bar in 0..=10 {
            for run in 0..=10 {
                for spare in [0, 16] {
                    assert_eq!(
                        compare(
                            &program,
                            &stack,
                            &Value::Bool(true),
                            Limits {
                                bar,
                                run,
                                depth: 8,
                                spare,
                                profile: true,
                                damage: Damage::None,
                            }
                        ),
                        0
                    );
                }
            }
        }
    }
}
