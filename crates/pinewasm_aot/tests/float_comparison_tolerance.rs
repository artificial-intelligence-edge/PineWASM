//! Finite comparison regression vectors plus derived routing checks.

use pinewasm_aot::{
    compile_standalone_wasm_program, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_mir::Value;
use pinewasm_vm::{run, RunOptions};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

fn check(source: &str, expected: &[Vec<f64>]) {
    let compiled = compile(source, CompileOptions::all());
    assert!(compiled.success, "{source}: {:?}", compiled.diagnostics);
    let program = compiled.program.expect("compiled program");
    let bars = expected.first().expect("expected plots").len();
    let data = synthetic_data(bars);
    let reference = run(&program, &data, &RunOptions::default());
    assert!(
        reference.errors.is_empty(),
        "{source}: {:?}",
        reference.errors
    );
    assert_eq!(reference.plots.len(), expected.len());
    let wasm = compile_standalone_wasm_program(
        &program,
        &StandaloneWasmOptions {
            signal_plot: None,
            module_type: StandaloneWasmModuleType::Indicator,
        },
    )
    .unwrap_or_else(|error| panic!("{source}: {error}"));
    let mut native = StandaloneWasmHost::new(&wasm);
    native
        .evaluate(&standalone_wasm_wire(&data, None))
        .expect("native evaluation");
    for (plot, wanted) in expected.iter().enumerate() {
        let lane = native.plot_lane(plot);
        assert_eq!(lane.len(), bars);
        for (bar, expected) in wanted.iter().enumerate() {
            assert_eq!(
                reference.plots[plot].values[bar],
                Value::Float(*expected),
                "VM plot={plot} bar={bar}\n{source}"
            );
            assert_eq!(
                lane[bar].to_bits(),
                expected.to_bits(),
                "native plot={plot} bar={bar}\n{source}"
            );
        }
    }
}

fn bits(lhs: &str, rhs: &str) -> String {
    ["==", "!=", "<", "<=", ">", ">="]
        .iter()
        .enumerate()
        .map(|(index, op)| format!("({lhs}{op}{rhs}?{}:0)", 1 << (5 - index)))
        .collect::<Vec<_>>()
        .join("+")
}

#[test]
fn all_comparison_lattice_vectors_match_vm_and_importless_native() {
    // Each entry gives the six comparison results for one operand pair.
    // From most to least significant bit: ==, !=, <, <=, >, >=.
    // Both the bytecode interpreter and standalone Wasm must match all 48 entries.
    let lattice_expectations = [
        [28., 37., 37., 37., 37., 19.],
        [28., 28., 37., 37., 19., 19.],
        [28., 28., 37., 37., 19., 19.],
        [28., 28., 37., 37., 19., 19.],
        [28., 28., 37., 37., 19., 19.],
        [28., 28., 37., 37., 19., 19.],
        [28., 37., 37., 37., 37., 19.],
        [28., 28., 37., 37., 19., 19.],
    ];
    let mut source =
        "//@version=6\nindicator(\"Comparison lattice\")\nscale=input.float(1.0)\n".to_string();
    let mut expected = Vec::new();
    for (group, base) in [0.0, 1.0, -1.0, 0.3].iter().enumerate() {
        source.push_str(&format!("float b{group}={base:?}+float(bar_index%2)\n"));
        for (index, delta) in [-1.5e-10, -1e-10, -5e-11, 5e-11, 1e-10, 1.5e-10]
            .iter()
            .enumerate()
        {
            let lhs = format!("a{group}_{index}");
            let rhs = format!("b{group}");
            source.push_str(&format!(
                "float {lhs}={rhs}+({delta:?})*scale\nplot({})\n",
                bits(&lhs, &rhs)
            ));
            expected.push(vec![
                lattice_expectations[group][index],
                lattice_expectations[group + 4][index],
            ]);
        }
    }
    check(&source, &expected);
}

#[test]
fn slots_history_mixed_pairs_missing_and_exact_large_ints_keep_their_boundaries() {
    // Derived routing and preservation checks. Missing operands remain false
    // for all six ops.
    let source = format!(
        "//@version=6\nindicator(\"Comparison routes\")\neps=input.float(5e-11)\na=eps+float(bar_index%2)\nb=float(bar_index%2)\nplot({})\nplot({})\nplot({})\nplot({})\nn=input.int(9007199254740993)\nm=input.int(9007199254740992)\nplot({})\nfloat missing=na\nplot({})\nplot({})\n",
        bits("a","b"),bits("a[1]","b[1]"),bits("eps","0"),bits("0","eps"),bits("n","m"),bits("missing","eps"),bits("eps","missing")
    );
    check(
        &source,
        &[
            vec![37.; 2],
            vec![0., 37.],
            vec![37.; 2],
            vec![37.; 2],
            vec![19.; 2],
            vec![0.; 2],
            vec![0.; 2],
        ],
    );
}

#[test]
fn fused_uneven_quotient_uses_float_tolerance_and_exact_quotient_stays_integer() {
    // Uneven quotient lies within1e-10 of the integer1. Reverse-side and
    // stored/carry routes must agree; exact quotient above2^53 stays exact.
    let mut source = "//@version=6\nindicator(\"Fused quotient comparisons\")\nn=input.int(20000000001)\nd=input.int(20000000000)\n".to_string();
    for (lhs, rhs) in [("(n/d)", "1"), ("1", "(n/d)")] {
        source.push_str(&format!("plot({})\n", bits(lhs, rhs)));
    }
    check(&source, &[vec![37.; 2], vec![37.; 2]]);

    // A stored quotient invokes fraction carry. Keep this script separate:
    // the existing guard refuses an inexactly representable large integer
    // anywhere in a carried quotient flow rather than silently rounding it.
    let stored = format!("//@version=6\nindicator(\"Carried quotient comparisons\")\nn=input.int(20000000001)\nd=input.int(20000000000)\nint q=n/d\nplot({})\nplot({})\n", bits("q", "1"), bits("1", "q"));
    check(&stored, &[vec![37.; 2], vec![37.; 2]]);

    let exact = format!("//@version=6\nindicator(\"Exact quotient comparisons\")\nlarge=input.int(9007199254740993)\none=input.int(1)\nplot({})\nplot({})\n", bits("(large/one)", "9007199254740992"), bits("9007199254740992", "(large/one)"));
    check(&exact, &[vec![19.; 2], vec![28.; 2]]);
}
