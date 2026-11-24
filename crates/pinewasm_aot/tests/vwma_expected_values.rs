//! VWMA and SMA expected values on a discriminating synthetic feed.
//! The native execution requires regenerated current TA runtime bytes.

use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, ExecutionMode, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

const PREFIX: &str = "//@version=6\nindicator(\"VWMA expected values\")\nsrc = close > 3 and close < 7 ? na : close\n";

fn data() -> MarketData {
    let mut data = synthetic_data(5);
    data.open = vec![2.0, 4.0, 6.0, 8.0, 10.0];
    data.close.clone_from(&data.open);
    data.high = data.close.iter().map(|v| v + 1.0).collect();
    data.low = data.close.iter().map(|v| v - 1.0).collect();
    data.volume = vec![1.0, 3.0, 5.0, 2.0, 2.0];
    data
}

fn program(body: &str, options: CompileOptions) -> CsilProgram {
    let source = format!("{PREFIX}{body}\n");
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

fn floats(values: &[Value]) -> Vec<Option<f64>> {
    values
        .iter()
        .map(|value| match value {
            Value::Na => None,
            Value::Float(v) => Some(*v),
            other => panic!("unexpected plot value: {other:?}"),
        })
        .collect()
}

#[test]
fn literal_dynamic_and_composed_vwma_keep_denominator_volume_on_source_na() {
    let body = "n = bar_index == 99999 ? 3 : 2\nplot(ta.vwma(src, 2))\nplot(ta.vwma(src, n))\nplot(ta.sma(src * volume, 2) / ta.sma(volume, 2))";
    let data = data();
    for options in [CompileOptions::default(), CompileOptions::all()] {
        let program = program(body, options);
        assert!(
            program
                .main()
                .code
                .iter()
                .any(|i| matches!(i, CsilInstr::CallTa { .. })),
            "literal engine route is exercised"
        );
        assert!(
            program
                .main()
                .code
                .iter()
                .any(|i| matches!(i, CsilInstr::WindowMeanDynamic { .. })),
            "dynamic lowering route is exercised"
        );
        let result = run(&program, &data, &RunOptions::default());
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.plots.len(), 3);
        for plot in result.plots {
            // Last valid products at bar 3 are 2 and 16; denominator volumes
            // are 5 and 2. A source-masked denominator incorrectly gives 6.
            assert_eq!(
                floats(&plot.values),
                [None, None, None, Some(18.0 / 7.0), Some(9.0)]
            );
        }
    }
}

fn native_bodies() -> [(&'static str, Vec<Option<f64>>); 3] {
    [
        (
            "plot(ta.vwma(src, 2))",
            vec![None, None, None, Some(18.0 / 7.0), Some(9.0)],
        ),
        (
            "n = input.int(2)\nplot(ta.vwma(src, n))",
            vec![None, None, None, Some(18.0 / 7.0), Some(9.0)],
        ),
        (
            "plot(ta.vwma(src, 2) / (ta.sma(src * volume, 2) / ta.sma(volume, 2)))",
            vec![None, None, None, Some(1.0), Some(1.0)],
        ),
    ]
}

fn native(program: &CsilProgram) -> Vec<u8> {
    assert_eq!(
        compile_program(program)
            .expect("native chart artifact")
            .layout
            .execution_mode,
        ExecutionMode::Native
    );
    compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .expect("actual standalone artifact")
}

#[test]
fn literal_and_input_vwma_have_actual_native_routes() {
    for (body, _) in native_bodies() {
        native(&program(body, CompileOptions::all()));
    }
}

#[test]
fn standalone_vwma_matches_expected_values_without_imports() {
    let data = data();
    for (body, expected) in native_bodies() {
        let wasm = native(&program(body, CompileOptions::all()));
        let mut host = StandaloneWasmHost::new(&wasm);
        host.evaluate(&standalone_wasm_wire(&data, None))
            .expect("actual no-import native execution");
        let actual = host.plot_lane(0);
        assert_eq!(actual.len(), expected.len());
        for (bar, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            match expected {
                None => assert!(actual.is_nan(), "{body} bar{bar}: {actual}"),
                Some(expected) => {
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{body} bar{bar}");
                }
            }
        }
    }
}
