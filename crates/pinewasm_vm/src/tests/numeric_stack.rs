//! Successful in-place arithmetic versus the unfused pop/pop/push sequence.
use super::{compile, data_from_close};
use crate::{vm_dispatch, vm_ops, RunOptions, Vm};
use pinewasm_csil::CsilInstr;
use pinewasm_mir::{HandleId, Value};

#[test]
fn numeric_stack_matches_original_values_errors_limits_and_frame() {
    let program = compile("indicator(\"Numeric stack\")\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut values = vec![
        Value::Na,
        Value::Int(i64::MIN),
        Value::Bool(true),
        Value::String("invalid operand".into()),
        Value::Ref(HandleId(7)),
    ];
    values.extend(
        [
            -f64::MAX,
            -1.0,
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            0.5,
            1.0,
            3.0,
            f64::MAX,
        ]
        .map(Value::Float),
    );
    for (instr, op) in [
        (CsilInstr::F64Add, vm_ops::FloatArithOp::Add),
        (CsilInstr::F64Sub, vm_ops::FloatArithOp::Sub),
        (CsilInstr::F64Mul, vm_ops::FloatArithOp::Mul),
        (CsilInstr::F64Div, vm_ops::FloatArithOp::Div),
        (CsilInstr::F64Rem, vm_ops::FloatArithOp::Rem),
    ] {
        for lhs in &values {
            for rhs in &values {
                for stack_limit in [0, 1, 2, 3, 8] {
                    for operands in [vec![], vec![rhs.clone()], vec![lhs.clone(), rhs.clone()]] {
                        for stack_prefix in [vec![], vec![Value::String("prefix untouched".into())]]
                        {
                            let options = RunOptions {
                                max_stack_depth: stack_limit,
                                profile: true,
                                ..RunOptions::default()
                            };
                            let mut actual = Vm::new(&program, &data, &options).unwrap();
                            let mut reference = Vm::new(&program, &data, &options).unwrap();
                            for vm in [&mut actual, &mut reference] {
                                vm.stack = stack_prefix.clone();
                                vm.stack.extend_from_slice(&operands);
                                vm.instructions_remaining = 991;
                            }
                            let (mut budget_actual, budget_reference) = (713, 713);
                            let result = actual.exec_instr(&program, &instr, 0, &mut budget_actual);
                            reference.profile.as_mut().unwrap().count[0] += 1;
                            reference.profile.as_mut().unwrap().units[0] += 1;
                            let expected = (|| {
                                let rhs = reference.pop_float(0, "arithmetic")?;
                                let lhs = reference.pop_float(0, "arithmetic")?;
                                reference.push(vm_ops::float_arith(op, lhs, rhs), 0)?;
                                Ok(vm_dispatch::Flow::Next)
                            })();
                            assert_eq!(result, expected);
                            assert_eq!(actual.stack, reference.stack);
                            for (a, b) in actual.stack.iter().zip(&reference.stack) {
                                if let (Value::Float(a), Value::Float(b)) = (a, b) {
                                    assert_eq!(a.to_bits(), b.to_bits());
                                }
                            }
                            assert_eq!(actual.heap, reference.heap);
                            assert_eq!(actual.user_slots, reference.user_slots);
                            assert_eq!(actual.written, reference.written);
                            assert_eq!(
                                (
                                    budget_actual,
                                    actual.instructions_remaining,
                                    actual.string_bytes_allocated
                                ),
                                (
                                    budget_reference,
                                    reference.instructions_remaining,
                                    reference.string_bytes_allocated
                                )
                            );
                            assert_eq!(
                                actual.profile.as_ref().map(|p| (&p.count, &p.units)),
                                reference.profile.as_ref().map(|p| (&p.count, &p.units))
                            );
                        }
                    }
                }
            }
        }
    }
}
