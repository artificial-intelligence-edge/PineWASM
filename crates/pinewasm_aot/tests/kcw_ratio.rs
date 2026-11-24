//! Finite KCW ratio expectations, then standalone execution without imports.
//! Regenerate the linked runtime after changing the TA production rule.

use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, ExecutionMode, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::CsilProgram;
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

fn data() -> MarketData {
    let mut data = synthetic_data(4);
    data.open.fill(10.0);
    data.close.fill(10.0);
    data.high.fill(12.0);
    data.low.fill(8.0);
    data
}

fn cases() -> Vec<(&'static str, Vec<Option<f64>>)> {
    vec![
        (
            "plot(ta.kcw(close, 2, 1, false))",
            vec![None, Some(0.8), Some(0.8), Some(0.8)],
        ),
        (
            "plot(ta.kcw(close, 2, 1))",
            vec![None, None, Some(0.8), Some(0.8)],
        ),
        (
            "plot(ta.kcw(close, 2, 1, true))",
            vec![None, None, Some(0.8), Some(0.8)],
        ),
        (
            "plot(ta.kcw(-close, 2, 1, false))",
            vec![None, Some(-0.8), Some(-0.8), Some(-0.8)],
        ),
        ("plot(ta.kcw(0.0, 2, 1, false))", vec![None; 4]),
        (
            "plot(ta.kcw(bar_index == 2 ? na : close, 2, 1, false))",
            vec![None, Some(0.8), None, Some(0.8)],
        ),
        (
            "n = input.int(2)\nm = input.float(1.0)\nplot(ta.kcw(close, n, m, false))",
            vec![None, Some(0.8), Some(0.8), Some(0.8)],
        ),
        (
            "x = 10.0 * (bar_index + 1)\nplot(ta.kcw(x, 3, 2.0, false))",
            vec![None, None, Some(0.8), Some(16.0 / 30.0)],
        ),
        (
            "x = 10.0 * (bar_index + 1)\n[b, u, l] = ta.kc(x, 3, 2.0, false)\nw = ta.kcw(x, 3, 2.0, false)\nplot(w / ((u-l)/b))",
            vec![None, None, Some(1.0), Some(1.0)],
        ),
    ]
}

fn program(body: &str, options: CompileOptions) -> CsilProgram {
    let source = format!("//@version=6\nindicator(\"KCW ratio\")\n{body}\n");
    let result = compile(&source, options);
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "{source}\n{:?}",
        result.diagnostics
    );
    result.program.expect("compiled source")
}

#[test]
fn vm_returns_fixed_ratio_expectations_with_both_compile_options() {
    let data = data();
    for options in [CompileOptions::default(), CompileOptions::all()] {
        for (body, expected) in cases() {
            let result = run(&program(body, options), &data, &RunOptions::default());
            assert!(result.errors.is_empty(), "{body}: {:?}", result.errors);
            assert_eq!(result.plots.len(), 1, "{body}");
            let actual: Vec<_> = result.plots[0]
                .values
                .iter()
                .map(|v| match v {
                    Value::Na => None,
                    Value::Float(v) => Some(*v),
                    other => panic!("unexpected numeric lane: {other:?}"),
                })
                .collect();
            assert_eq!(actual, expected, "{body}");
        }
    }
}

#[test]
fn kcw_sources_have_native_routes_without_fallback() {
    for (body, _) in cases() {
        let program = program(body, CompileOptions::all());
        assert_eq!(
            compile_program(&program)
                .expect("native chart artifact")
                .layout
                .execution_mode,
            ExecutionMode::Native,
            "{body}"
        );
        compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .expect("actual standalone artifact, no interpreter fallback");
    }
}

#[test]
fn standalone_native_kcw_matches_fixed_ratios_without_imports() {
    let data = data();
    for (body, expected) in cases() {
        let program = program(body, CompileOptions::all());
        assert_eq!(
            compile_program(&program)
                .expect("native chart artifact")
                .layout
                .execution_mode,
            ExecutionMode::Native,
            "{body}"
        );
        let wasm = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .expect("actual standalone artifact, no interpreter fallback");
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&standalone_wasm_wire(&data, None))
            .expect("actual native execution");
        let actual = host.plot_lane(0);
        assert_eq!(actual.len(), expected.len(), "{body}");
        for (bar, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            match expected {
                None => assert!(actual.is_nan(), "{body} bar {bar}: {actual}"),
                Some(expected) => {
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{body} bar {bar}");
                }
            }
        }
    }
}
