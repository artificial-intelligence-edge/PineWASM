//! Full-frame comparison against ordinary instruction dispatch, including
//! limits between the three operations and malformed defensive inputs.
use super::{compile, data_from_close};
use crate::{vm_charge, vm_dispatch, RunBarHost, RunOptions, RuntimeError, Vm};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::PureFunc;
use pinewasm_mir::{HandleId, Value};

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
        if let CsilInstr::CallPure { func, argc } = instr {
            let vm = &mut *self.inner.vm;
            if let Some(profile) = vm.profile.as_mut() {
                profile.count[pc] += 1;
                profile.units[pc] += 1;
            }
            vm.exec_call_pure_op(*func, *argc, pc, budget)?;
            Ok(vm_dispatch::Flow::Next)
        } else {
            self.inner.exec(instr, pc, budget)
        }
    }
}

fn program() -> CsilProgram {
    let mut program =
        compile("indicator(\"Math sequence\")\nplot(math.log(1 + math.abs(close)))\n");
    let main = &mut program.functions[0];
    main.code = vec![
        CsilInstr::CallPure {
            func: PureFunc::MathAbs,
            argc: 1,
        },
        CsilInstr::F64Add,
        CsilInstr::CallPure {
            func: PureFunc::MathLog,
            argc: 1,
        },
        CsilInstr::Return,
    ];
    main.source_map.truncate(4);
    main.budget_reference = None;
    main.max_stack = 3;
    program
}

fn compare(program: &CsilProgram, stack: &[Value], bar: u64, run: u64, limit: usize) {
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
        vm.instructions_remaining = run;
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
    let eligible = stack.len() >= 2
        && stack.len() <= limit
        && bar >= 3
        && run >= 3
        && stack[stack.len() - 2..]
            .iter()
            .all(|v| matches!(v, Value::Na | Value::Float(_)))
        && matches!(
            program.main().code.first(),
            Some(CsilInstr::CallPure {
                func: PureFunc::MathAbs,
                argc: 1
            })
        )
        && matches!(program.main().code.get(1), Some(CsilInstr::F64Add))
        && matches!(
            program.main().code.get(2),
            Some(CsilInstr::CallPure {
                func: PureFunc::MathLog,
                argc: 1
            })
        );
    assert_eq!(a.combined_dispatches(), if eligible { 2 } else { 0 });
    assert_eq!(b.combined_dispatches(), 0);
}

#[test]
fn math_chain_preserves_values_limits_source_errors_and_complete_frame() {
    let program = program();
    let mut values = vec![
        Value::Na,
        Value::Int(i64::MIN),
        Value::Int(3),
        Value::Bool(true),
        Value::String("wrong operand".into()),
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
    for lhs in &values {
        for rhs in &values {
            for bar in [0, 1, 2, 3, 4, 10] {
                for run in [0, 1, 2, 3, 4, 10] {
                    for limit in [0, 1, 2, 3, 8] {
                        compare(&program, &[lhs.clone(), rhs.clone()], bar, run, limit);
                        compare(
                            &program,
                            &[Value::String("prefix".into()), lhs.clone(), rhs.clone()],
                            bar,
                            run,
                            limit,
                        );
                    }
                }
            }
        }
    }
    for stack in [
        vec![],
        vec![Value::Na],
        vec![Value::Float(-0.0)],
        vec![Value::String("short".into())],
    ] {
        for bar in 0..=4 {
            for run in 0..=4 {
                for limit in 0..=3 {
                    compare(&program, &stack, bar, run, limit);
                }
            }
        }
    }
}

#[test]
fn math_chain_delegates_wrong_arities_neighbors_and_missing_tails() {
    let original = program();
    for (pc, instr) in [
        (
            0,
            CsilInstr::CallPure {
                func: PureFunc::MathAbs,
                argc: 0,
            },
        ),
        (
            0,
            CsilInstr::CallPure {
                func: PureFunc::MathAbs,
                argc: 2,
            },
        ),
        (
            0,
            CsilInstr::CallPure {
                func: PureFunc::MathSqrt,
                argc: 1,
            },
        ),
        (1, CsilInstr::F64Sub),
        (
            2,
            CsilInstr::CallPure {
                func: PureFunc::MathLog,
                argc: 0,
            },
        ),
        (
            2,
            CsilInstr::CallPure {
                func: PureFunc::MathLog,
                argc: 2,
            },
        ),
        (
            2,
            CsilInstr::CallPure {
                func: PureFunc::MathLog10,
                argc: 1,
            },
        ),
    ] {
        let mut program = original.clone();
        program.functions[0].code[pc] = instr;
        for bar in 0..=5 {
            for run in 0..=5 {
                for limit in 0..=3 {
                    compare(
                        &program,
                        &[Value::Float(1.0), Value::Float(-3.0)],
                        bar,
                        run,
                        limit,
                    );
                }
            }
        }
    }
    for length in 1..=3 {
        let mut program = original.clone();
        program.functions[0].code.truncate(length);
        compare(
            &program,
            &[Value::Float(1.0), Value::Float(-3.0)],
            10,
            10,
            3,
        );
    }
}
