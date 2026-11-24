//! The production `PlotF64` state transition, without VM error formatting.
//!
//! Pop precedes both tag and destination validation. In particular, failures
//! after a successful pop consume the operand; they do not roll back the stack.
//! Float payloads are moved unchanged, including directly constructed nonfinite
//! values. Normalization belongs to producers such as `Value::from_f64`.

use pinewasm_mir::Value;

pub(crate) enum PlotStepError {
    Underflow,
    TypeMismatch(Value),
    OutOfRange,
}

/// Called by the actual `Vm` dispatch arm. No allocation, clone, numeric
/// operation, error-message construction, or source-PC translation occurs here.
pub(crate) fn plot_step(
    stack: &mut Vec<Value>,
    plots: &mut [Vec<Value>],
    bar: usize,
    plot_slot: usize,
) -> Result<(), PlotStepError> {
    let value = match stack.pop() {
        Some(value) => value,
        None => return Err(PlotStepError::Underflow),
    };
    match &value {
        Value::Float(_) | Value::Na => {}
        _ => return Err(PlotStepError::TypeMismatch(value)),
    }
    if plot_slot >= plots.len() {
        return Err(PlotStepError::OutOfRange);
    }
    if bar >= plots[plot_slot].len() {
        return Err(PlotStepError::OutOfRange);
    }
    plots[plot_slot][bar] = value;
    Ok(())
}

/// Test-only extraction entry: this calls the same helper as production,
/// rather than copying its body or introducing an alternative value model.
#[cfg(test)]
pub(crate) fn extraction_root(
    stack: &mut Vec<Value>,
    plots: &mut [Vec<Value>],
    bar: usize,
    plot_slot: usize,
) -> Result<(), PlotStepError> {
    plot_step(stack, plots, bar, plot_slot)
}

#[cfg(test)]
mod tests {
    use super::{extraction_root, plot_step, PlotStepError};
    use pinewasm_mir::{HandleId, Value};

    // Baseline copy of the arm semantics: its pop error is represented here
    // without RuntimeError formatting, which remains in the dispatch caller.
    fn original(
        stack: &mut Vec<Value>,
        plots: &mut [Vec<Value>],
        bar: usize,
        plot_slot: usize,
    ) -> Result<(), PlotStepError> {
        let value = match stack.pop() {
            Some(value) => value,
            None => return Err(PlotStepError::Underflow),
        };
        match &value {
            Value::Float(_) | Value::Na => {}
            _ => return Err(PlotStepError::TypeMismatch(value)),
        }
        let cell = plots
            .get_mut(plot_slot)
            .and_then(|series| series.get_mut(bar));
        match cell {
            Some(cell) => *cell = value,
            None => return Err(PlotStepError::OutOfRange),
        }
        Ok(())
    }

    // PartialEq is unsuitable for NaN and does not distinguish signed zero.
    fn value_bits(value: &Value) -> String {
        match value {
            Value::Float(value) => format!("Float:{:016x}", value.to_bits()),
            other => format!("{other:?}"),
        }
    }

    fn state(stack: &[Value], plots: &[Vec<Value>]) -> (Vec<String>, Vec<Vec<String>>) {
        (
            stack.iter().map(value_bits).collect(),
            plots
                .iter()
                .map(|row| row.iter().map(value_bits).collect())
                .collect(),
        )
    }

    fn outcome(result: Result<(), PlotStepError>) -> String {
        match result {
            Ok(()) => "Ok".into(),
            Err(PlotStepError::Underflow) => "Underflow".into(),
            Err(PlotStepError::OutOfRange) => "OutOfRange".into(),
            Err(PlotStepError::TypeMismatch(value)) => {
                format!("TypeMismatch:{}:{}", value.type_name(), value_bits(&value))
            }
        }
    }

    fn dispatch_checks() {
        use crate::{MarketData, RunOptions, RuntimeErrorKind};
        use pinewasm_csil::CsilInstr;
        // The PlotF64 instruction remains part of CSIL v2. Use a checked-in
        // compiled test program, without styling, for its exact dispatch
        // checks. Plot defaults in current compiled programs emit
        // PlotStyledF64, which is covered independently by the rendering
        // contract tests.
        let program = pinewasm_csil::CsilProgram::from_json(include_str!(
            "../../pinewasm_csil/src/fixtures/coalescing_scalar.json"
        ))
        .expect("checked-in scalar test program decodes");
        pinewasm_csil::validate(&program).expect("valid baseline");
        let pc = program.functions[0]
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::PlotF64 { .. }))
            .expect("plot");
        assert_eq!(pc, 1);
        let span = program.functions[0].source_map[pc];
        let data = MarketData {
            open: vec![3.0],
            high: vec![3.0],
            low: vec![3.0],
            close: vec![3.0],
            volume: vec![1.0],
            ..MarketData::default()
        };
        let success = crate::run(&program, &data, &RunOptions::default());
        assert!(success.errors.is_empty());
        assert_eq!(
            value_bits(&success.plots[0].values[0]),
            "Float:4008000000000000"
        );
        // Deliberately corrupted programs use the defensive public run entry,
        // not validated PreparedProgram admission. Both faults must report the
        // tag error first, after consuming the operand.
        for (read, slot, kind, message) in [
            (
                CsilInstr::PushBool(true),
                0,
                RuntimeErrorKind::TypeMismatch,
                "plot requires a float, found bool",
            ),
            (
                CsilInstr::PushBool(true),
                u32::MAX,
                RuntimeErrorKind::TypeMismatch,
                "plot requires a float, found bool",
            ),
            (
                program.functions[0].code[0].clone(),
                u32::MAX,
                RuntimeErrorKind::InvalidProgram,
                "plot slot 4294967295 out of range",
            ),
            (
                CsilInstr::Jump { target: pc as u32 },
                u32::MAX,
                RuntimeErrorKind::StackUnderflow,
                "operand stack underflow (validator should have rejected this program)",
            ),
        ] {
            let mut bad = program.clone();
            bad.functions[0].budget_reference = None;
            bad.functions[0].code[0] = read;
            bad.functions[0].code[pc] = CsilInstr::PlotF64 { plot_slot: slot };
            let result = crate::run(&bad, &data, &RunOptions::default());
            assert_eq!(result.errors.len(), 1);
            let error = &result.errors[0];
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
            assert_eq!(error.instruction_offset, Some(pc as u32));
            assert_eq!(error.bar_index, Some(0));
            assert_eq!(error.span, Some(span));
            // Public outcomes retain the series metadata but expose values
            // only for completed bars. These faults complete zero bars.
            assert_eq!(result.bars_executed, 0);
            assert_eq!(result.plots.len(), 1);
            assert!(result.plots[0].values.is_empty());
            let options = RunOptions::default();
            let mut vm =
                crate::Vm::new(&bad, &data, &options).expect("corrupt program construction");
            let (_, errors) = vm.run_history();
            assert_eq!(errors, result.errors);
            assert!(vm.stack.is_empty());
            assert!(matches!(vm.plots[0][0], Value::Na));
        }
        println!("public VM: one valid run, four corrupted-program errors; exact kind/message/PC/span/bar and unchanged plot");
    }

    #[test]
    fn actual_plot_step_matches_original_and_extraction_root() {
        let mut values = vec![
            Value::Na,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::String("plot\0é".into()),
            Value::Color {
                r: 0,
                g: 255,
                b: 17,
                t: 100.0,
                input_origin: None,
            },
            Value::Ref(HandleId(u32::MAX)),
        ];
        // Both zeros, both minimal subnormals, normal/extreme finite values,
        // infinities, quiet/signaling NaNs, signs and payloads.
        for bits in [
            0,
            0x8000_0000_0000_0000,
            1,
            0x8000_0000_0000_0001,
            0x0010_0000_0000_0000,
            0x8010_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0xbff0_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0xffef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
            0xfff8_0000_0000_0042,
            0x7ff0_0000_0000_0001,
            0xfff0_0000_0000_0001,
        ] {
            values.push(Value::Float(f64::from_bits(bits)));
        }
        let layouts = vec![
            vec![],
            vec![vec![]],
            vec![vec![Value::Na]],
            vec![vec![Value::Int(17), Value::Bool(true)], vec![Value::Na]],
            vec![
                vec![],
                vec![Value::Float(-0.0), Value::String("untouched".into())],
            ],
            vec![
                vec![Value::Na, Value::Ref(HandleId(9))],
                vec![],
                vec![Value::Int(7)],
            ],
        ];
        let mut cases = 0;
        for value in &values {
            for stack in [
                vec![],
                vec![value.clone()],
                vec![Value::Int(91), value.clone()],
            ] {
                for plots in &layouts {
                    for bar in [0, 1, 2, usize::MAX] {
                        for slot in [0, 1, 2, usize::MAX] {
                            let mut expected_stack = stack.clone();
                            let mut expected_plots = plots.clone();
                            let expected = outcome(original(
                                &mut expected_stack,
                                &mut expected_plots,
                                bar,
                                slot,
                            ));
                            for helper in [plot_step, extraction_root] {
                                let mut got_stack = stack.clone();
                                let mut got_plots = plots.clone();
                                let got =
                                    outcome(helper(&mut got_stack, &mut got_plots, bar, slot));
                                assert_eq!(got, expected, "case {cases}, bar {bar}, slot {slot}");
                                assert_eq!(
                                    state(&got_stack, &got_plots),
                                    state(&expected_stack, &expected_plots)
                                );
                                if !stack.is_empty() {
                                    assert_eq!(got_stack.len(), stack.len() - 1);
                                }
                                if expected != "Ok" {
                                    assert_eq!(state(&[], &got_plots), state(&[], plots));
                                }
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 6912);
        // Explicit checks that bounds are not tested before tags, that rejected
        // operands are consumed, and that direct infinity is not normalized.
        let mut stack = vec![Value::Bool(true)];
        assert!(matches!(
            plot_step(&mut stack, &mut [], usize::MAX, usize::MAX),
            Err(PlotStepError::TypeMismatch(_))
        ));
        assert!(stack.is_empty());
        let mut plots = vec![vec![Value::Na]];
        let mut stack = vec![Value::Float(f64::INFINITY)];
        assert_eq!(outcome(plot_step(&mut stack, &mut plots, 0, 0)), "Ok");
        assert_eq!(value_bits(&plots[0][0]), "Float:7ff0000000000000");
        dispatch_checks();
        println!("6912 differential cases; production helper and extraction root; seven Value tags; exact float bits and failure effects");
    }
}
