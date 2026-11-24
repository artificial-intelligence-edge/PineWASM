use pinewasm_aot::{
    compile_program, compile_standalone_wasm_program, ExecutionMode, StandaloneWasmModuleType,
    StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

#[test]
fn once_activation_executes_on_the_native_standalone_route() {
    for (body, expected) in [
        (
            "var int n=0\nonce bar_index>=2\n    n+=1\nplot(n)\n",
            vec![0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
        ),
        (
            "var int n=0\nfor i=0 to 2\n    once\n        n+=1\n        continue\nplot(n)\n",
            vec![1.0; 6],
        ),
    ] {
        let source = format!("//@version=6\nindicator(\"Native once\")\n{body}");
        let compiled = compile(&source, CompileOptions::default());
        assert!(compiled.success, "{:?}", compiled.diagnostics);
        let program = compiled.program.unwrap();
        assert_eq!(
            compile_program(&program).unwrap().layout.execution_mode,
            ExecutionMode::Native
        );
        let bytes = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: StandaloneWasmModuleType::Indicator,
            },
        )
        .expect("once should not need the interpreter host");
        let mut host = StandaloneWasmHost::new(&bytes);
        host.evaluate(&standalone_wasm_wire(&synthetic_data(6), None))
            .unwrap();
        assert_eq!(host.plot_lane(0), expected, "{source}");
    }
}
