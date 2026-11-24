//! Native WebAssembly checks use fixed numerical expectations written in each
//! test.
use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, ExecutionMode, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};

#[allow(dead_code)]
mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

fn assert_native(body: &str, expected: &[Option<f64>]) {
    assert_native_shared_slot(body, expected, false);
}

fn assert_native_shared_slot(body: &str, expected: &[Option<f64>], shared: bool) {
    for options in [CompileOptions::default(), CompileOptions::all()] {
        let result = compile(
            &format!("//@version=6\nindicator(\"Native SMA missing observations\")\n{body}\n"),
            options,
        );
        assert!(result.success, "{body}: {:?}", result.diagnostics);
        let mut program = result.program.unwrap();
        if shared {
            use pinewasm_csil::CsilInstr;
            let slots = program
                .functions
                .iter()
                .flat_map(|f| &f.code)
                .filter_map(|i| {
                    if let CsilInstr::WindowMean { slot, .. } = i {
                        Some(*slot)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for instruction in program.functions.iter_mut().flat_map(|f| &mut f.code) {
                match instruction {
                    CsilInstr::WindowMean { slot, .. } => *slot = slots[0],
                    CsilInstr::StoreUserSlotCurrent { slot } if slots.contains(slot) => {
                        *slot = slots[0];
                    }
                    _ => {}
                }
            }
            for function in &mut program.functions {
                function.budget_reference = None;
            }
            pinewasm_csil::validate(&program).expect("shared numeric SMA slot is valid CSIL");
        }
        let universal = compile_program(&program).expect("literal/input SMA remains native");
        assert_eq!(universal.layout.execution_mode, ExecutionMode::Native);
        let wasm = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .expect("standalone native SMA");
        wasmparser::validate(&wasm).expect("valid emitted WebAssembly");
        let mut host = StandaloneWasmHost::new(&wasm);
        let wire = standalone_wasm_wire(&synthetic_data(expected.len()), None);
        // Reusing a resident module must reset dense observation counts.
        for _ in 0..2 {
            host.evaluate(&wire).expect("native run");
            let lane = host.plot_lane(0);
            for (bar, (actual, expected)) in lane.iter().zip(expected).enumerate() {
                match expected {
                    Some(expected) => {
                        assert_eq!(actual.to_bits(), expected.to_bits(), "{body}: bar {bar}");
                    }
                    None => assert!(actual.is_nan(), "{body}: bar {bar}: {actual}"),
                }
            }
        }
    }
}

#[test]
fn first_party_native_sma_skips_missing_observations_before_and_after_warmup() {
    assert_native(
        "float x=10.0*(bar_index+1)\nfloat g=(bar_index==1 or bar_index==2 or bar_index==5)?na:x\nplot(ta.sma(g,2))",
        &[None,None,None,Some(25.0),Some(45.0),Some(45.0),Some(60.0),Some(75.0),Some(85.0),Some(95.0)],
    );
}

#[test]
fn first_party_native_sma_retains_values_across_long_missing_runs() {
    assert_native(
        "float g=bar_index==0?10.0:bar_index==5?20.0:bar_index==9?30.0:bar_index==10?40.0:na\nplot(ta.sma(g,2))",
        &[None,None,None,None,None,Some(15.0),Some(15.0),Some(15.0),Some(15.0),Some(25.0),Some(35.0),Some(35.0)],
    );
}

#[test]
fn native_shared_slot_sma_matches_the_loop_lifecycle() {
    for final_output in [false, true] {
        let body = format!(
            "float first=ta.sma(float(bar_index*10),2)\nfloat middle=ta.sma(float(bar_index*10+1),2)\nfloat last=ta.sma(float(bar_index*10+2),2)\nplot({})",
            if final_output { "last" } else { "first" },
        );
        let expected = std::iter::once(None)
            .chain(
                (0..11)
                    .map(|bar| Some((if final_output { 7 } else { 6 }) as f64 + 10.0 * bar as f64)),
            )
            .collect::<Vec<_>>();
        assert_native_shared_slot(&body, &expected, true);
    }
}

#[test]
fn native_input_length_follows_the_direct_source_rule() {
    {
        let body = "n=input.int(2)\nplot(ta.sma(g,n))";
        assert_native(
            &format!("float g=bar_index==1?na:10.0*(bar_index+1)\n{body}"),
            &[
                None,
                None,
                Some(20.0),
                Some(35.0),
                Some(45.0),
                Some(55.0),
                Some(65.0),
                Some(75.0),
            ],
        );
    }
}

#[test]
fn native_shared_slot_restores_history_when_the_current_bar_becomes_missing() {
    // Lifecycle check: after a full-ring overwrite, replacing the
    // current observation with na must restore both previous observations.
    assert_native_shared_slot(
        "float first=ta.sma(float(bar_index*10),2)\nfloat middle=ta.sma(float(bar_index*10+1),2)\nfloat last=ta.sma(bar_index==2?na:float(bar_index*10+2),2)\nplot(last)",
        &[None,Some(7.0),Some(7.0),Some(22.0),Some(37.0)],
        true,
    );
}
