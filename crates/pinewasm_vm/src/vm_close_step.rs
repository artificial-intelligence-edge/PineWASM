//! Close read and push, without VM error formatting or dispatch charge.
//! All four OHLC loads precede normalization and the stack-limit check. A short
//! market array can therefore panic before a stack-overflow error. No rollback,
//! allocator, other-builtin, or numeric arithmetic behavior is added here.

use pinewasm_mir::Value;

pub(crate) struct Ohlc<'a> {
    pub(crate) open: &'a [f64],
    pub(crate) high: &'a [f64],
    pub(crate) low: &'a [f64],
    pub(crate) close: &'a [f64],
}

pub(crate) enum CloseStepError {
    StackOverflow,
}

/// Value-producing path used by both the Close opcode and builtin reads.
/// Keeps the unconditional open/high/low/close indexed-read order.
pub(crate) fn close_value(bar: usize, offset: u32, data: &Ohlc<'_>) -> Value {
    let Some(index) = bar.checked_sub(offset as usize) else {
        return Value::Na;
    };
    let (_o, _h, _l, c) = (
        data.open[index],
        data.high[index],
        data.low[index],
        data.close[index],
    );
    Value::from_f64(c)
}

/// Read before checking depth, exactly as `read_builtin` followed by `Vm::push`.
pub(crate) fn close_step(
    stack: &mut Vec<Value>,
    max_stack_depth: usize,
    bar: usize,
    offset: u32,
    data: &Ohlc<'_>,
) -> Result<(), CloseStepError> {
    let value = close_value(bar, offset, data);
    if stack.len() >= max_stack_depth {
        return Err(CloseStepError::StackOverflow);
    }
    stack.push(value);
    Ok(())
}

/// Test-only wrapper calls the same implementation; `Value::from_f64` stays visible.
#[cfg(test)]
pub(crate) fn extraction_root(
    stack: &mut Vec<Value>,
    max_stack_depth: usize,
    bar: usize,
    offset: u32,
    data: &Ohlc<'_>,
) -> Result<(), CloseStepError> {
    close_step(stack, max_stack_depth, bar, offset, data)
}

#[cfg(test)]
mod tests {
    use super::{close_step, close_value, extraction_root, CloseStepError, Ohlc};
    use pinewasm_mir::Value;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn bits(value: &Value) -> String {
        match value {
            Value::Float(v) => format!("Float:{:016x}", v.to_bits()),
            other => format!("{other:?}"),
        }
    }

    fn original(
        stack: &mut Vec<Value>,
        max_stack_depth: usize,
        bar: usize,
        offset: u32,
        data: &Ohlc<'_>,
    ) -> Result<(), CloseStepError> {
        // Direct form of the inline Close path, used as the test expectation: Close is not a strategy builtin.
        let value = match bar.checked_sub(offset as usize) {
            None => Value::Na,
            Some(index) => {
                let (_o, _h, _l, c) = (
                    data.open[index],
                    data.high[index],
                    data.low[index],
                    data.close[index],
                );
                Value::from_f64(c)
            }
        };
        if stack.len() >= max_stack_depth {
            return Err(CloseStepError::StackOverflow);
        }
        stack.push(value);
        Ok(())
    }

    fn outcome(result: Result<(), CloseStepError>) -> &'static str {
        match result {
            Ok(()) => "Ok",
            Err(CloseStepError::StackOverflow) => "StackOverflow",
        }
    }

    fn float_patterns() -> [u64; 16] {
        [
            0,
            0x8000_0000_0000_0000,
            1,
            0x8000_0000_0000_0001,
            0x0010_0000_0000_0000,
            0x8010_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0xbff4_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0xffef_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_1234,
            0xfff8_0000_0000_4321,
            0x7ff0_0000_0000_0001,
            0xfff0_0000_0000_0001,
        ]
    }

    fn dispatch_checks() {
        use crate::{MarketData, RunOptions, RuntimeErrorKind};
        use pinewasm_csil::CsilInstr;
        use pinewasm_hir::BuiltinSeries;
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"Close step\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        let mir = pinewasm_mir::lower(&analysis.hir);
        let program = pinewasm_csil::lower_mir(&mir).expect("lower");
        pinewasm_csil::validate(&program).expect("valid");
        let read_pc = program.functions[0]
            .code
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CsilInstr::ReadBuiltin {
                        source: BuiltinSeries::Close,
                        offset: 0
                    }
                )
            })
            .expect("close");
        let span = program.functions[0].source_map[read_pc];
        for (source, expected) in [
            (BuiltinSeries::Close, 4.0),
            (BuiltinSeries::Open, 1.0),
            (BuiltinSeries::High, 2.0),
            (BuiltinSeries::Low, 3.0),
            (BuiltinSeries::Volume, 5.0),
        ] {
            let data = MarketData {
                open: vec![1.0],
                high: vec![2.0],
                low: vec![3.0],
                close: vec![4.0],
                volume: vec![5.0],
                ..MarketData::default()
            };
            let mut candidate = program.clone();
            candidate.functions[0].budget_reference = None;
            candidate.functions[0].code[read_pc] = CsilInstr::ReadBuiltin { source, offset: 0 };
            let result = crate::run(&candidate, &data, &RunOptions::default());
            assert!(result.errors.is_empty());
            assert_eq!(result.bars_executed, 1);
            assert_eq!(
                bits(&result.plots[0].values[0]),
                bits(&Value::Float(expected))
            );
            let options = RunOptions::default();
            let vm = crate::Vm::new(&candidate, &data, &options).expect("vm");
            assert_eq!(
                bits(&vm.read_builtin(source, 0)),
                bits(&Value::Float(expected))
            );
            assert!(matches!(vm.read_builtin(source, 1), Value::Na));
        }
        let data = MarketData {
            open: vec![1.0],
            high: vec![2.0],
            low: vec![3.0],
            close: vec![4.0],
            volume: vec![5.0],
            ..MarketData::default()
        };
        for pattern in float_patterns() {
            let mut numeric_data = data.clone();
            numeric_data.close[0] = f64::from_bits(pattern);
            let result = crate::run(&program, &numeric_data, &RunOptions::default());
            assert!(result.errors.is_empty());
            assert_eq!(result.bars_executed, 1);
            assert_eq!(
                bits(&result.plots[0].values[0]),
                bits(&Value::from_f64(numeric_data.close[0]))
            );
            let options = RunOptions::default();
            let vm = crate::Vm::new(&program, &numeric_data, &options).expect("numeric vm");
            assert_eq!(
                bits(&vm.read_builtin(BuiltinSeries::Close, 0)),
                bits(&Value::from_f64(numeric_data.close[0]))
            );
        }
        let options = RunOptions {
            max_stack_depth: 0,
            ..RunOptions::default()
        };
        let result = crate::run(&program, &data, &options);
        assert_eq!(result.errors.len(), 1);
        let error = &result.errors[0];
        assert_eq!(error.kind, RuntimeErrorKind::StackOverflow);
        assert_eq!(error.message, "operand stack exceeded 0 values");
        assert_eq!(error.instruction_offset, Some(read_pc as u32));
        assert_eq!(error.bar_index, Some(0));
        assert_eq!(error.span, Some(span));
        assert_eq!(result.bars_executed, 0);
        let mut vm = crate::Vm::new(&program, &data, &options).expect("zero-depth vm");
        assert!(vm.run_bar().is_err());
        assert!(vm.stack.is_empty());
        // Malformed-data state: the constructor's alignment check
        // is bypassed only after successful construction, to expose load order.
        let ragged = MarketData {
            open: vec![],
            ..data.clone()
        };
        vm.data = ragged;
        assert!(catch_unwind(AssertUnwindSafe(|| vm.run_bar())).is_err());
        assert!(vm.stack.is_empty());
        println!("Close checks: 16 IEEE values; five builtin values and history guards; Close overflow kind/message/PC/span/bar; ragged-data panic before overflow");
    }

    #[test]
    fn actual_close_step_matches_original_and_extraction_root() {
        let patterns = float_patterns();
        let mut cases = 0;
        for pattern in patterns {
            let samples = [f64::from_bits(pattern); 3];
            let data = Ohlc {
                open: &samples,
                high: &samples,
                low: &samples,
                close: &samples,
            };
            assert_eq!(
                bits(&close_value(0, 0, &data)),
                if samples[0].is_finite() {
                    format!("Float:{pattern:016x}")
                } else {
                    "Na".into()
                }
            );
            for bar in [0, 1, 2] {
                for offset in [0, 1, 2, u32::MAX] {
                    for limit in [0, 1, 2, usize::MAX] {
                        for initial in [
                            vec![],
                            vec![Value::Na],
                            vec![Value::Bool(true), Value::Float(-0.0)],
                        ] {
                            let mut expected = initial.clone();
                            let want = outcome(original(&mut expected, limit, bar, offset, &data));
                            for run in [close_step, extraction_root] {
                                let mut stack = initial.clone();
                                assert_eq!(
                                    outcome(run(&mut stack, limit, bar, offset, &data)),
                                    want
                                );
                                assert_eq!(
                                    stack.iter().map(bits).collect::<Vec<_>>(),
                                    expected.iter().map(bits).collect::<Vec<_>>()
                                );
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2304);
        // Each unconditional array load can panic before the zero stack limit.
        let full = [1.0];
        for short in 0..4 {
            let slices: Vec<&[f64]> = (0..4)
                .map(|i| if i == short { &[][..] } else { &full[..] })
                .collect();
            let data = Ohlc {
                open: slices[0],
                high: slices[1],
                low: slices[2],
                close: slices[3],
            };
            for run in [original, close_step, extraction_root] {
                let mut stack = vec![Value::Bool(true)];
                assert!(
                    catch_unwind(AssertUnwindSafe(|| run(&mut stack, 0, 0, 0, &data))).is_err()
                );
                assert_eq!(
                    stack.iter().map(bits).collect::<Vec<_>>(),
                    vec!["Bool(true)"]
                );
                let mut stack = Vec::new();
                assert_eq!(outcome(run(&mut stack, 1, 0, 1, &data)), "Ok");
                assert!(matches!(stack.as_slice(), [Value::Na]));
                let mut full_stack = vec![Value::Na];
                assert_eq!(
                    outcome(run(&mut full_stack, 0, 0, 1, &data)),
                    "StackOverflow"
                );
                assert!(matches!(full_stack.as_slice(), [Value::Na]));
            }
        }
        dispatch_checks();
        println!("2304 differential cases; 16 IEEE patterns; four ragged OHLC panic/offset cases; close_step and the test-only wrapper");
    }
}
