//! Pin the native-AOT boundary for simple integer counting loops.
//!
//! The `i64::MIN` ambiguity analysis does not refine facts across branches
//! and loop joins, so a loop counter's addition has an unknown range. The
//! standalone tier used to decline such a loop. It now lowers it natively and
//! guards the addition: a present `i64::MIN` result stops the run with
//! `ABI_MISMATCH` instead of reading back as `na`. These loops never reach
//! the minimum, so the module and the VM must agree exactly.

use pinewasm_aot::analysis::{analyze, IntMinGuard};
use pinewasm_aot::{compile_standalone_program, header, status, AotModule};
use pinewasm_compiler::{
    compile, run_program, CompileOptions, MarketData, Program, RunOptions, Value,
};
use pinewasm_csil::CsilInstr;

fn one_bar() -> MarketData {
    MarketData {
        open: vec![1.0],
        high: vec![1.0],
        low: vec![1.0],
        close: vec![1.0],
        volume: vec![1.0],
        ..MarketData::default()
    }
}

/// Run a zero-import module over one bar and return its status and plot 0.
/// These programs read no market data, so the lanes stay zero.
fn run_one_bar(module: &AotModule) -> (i32, f64) {
    let engine = wasmi::Engine::default();
    let wasm = wasmi::Module::new(&engine, &module.wasm[..]).expect("valid module");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = wasmi::Linker::<()>::new(&engine)
        .instantiate_and_start(&mut store, &wasm)
        .expect("a standalone module has no imports");
    let prepare = instance
        .get_typed_func::<i32, i32>(&store, "prepare")
        .expect("prepare export");
    assert_eq!(prepare.call(&mut store, 1).unwrap(), status::OK);
    let run = instance
        .get_typed_func::<(), i32>(&store, "run")
        .expect("run export");
    let code = run.call(&mut store, ()).unwrap();
    let memory = instance.get_memory(&store, "memory").expect("memory");
    let mem = memory.data(&store);
    let word = |off: u32| {
        u32::from_le_bytes(mem[off as usize..off as usize + 4].try_into().unwrap()) as usize
    };
    let plots = word(header::OFF_PLOTS_BASE);
    (
        code,
        f64::from_le_bytes(mem[plots..plots + 8].try_into().unwrap()),
    )
}

/// The loop's addition is the only site the static facts cannot clear.
fn assert_only_the_addition_is_guarded(program: &Program, add_pc: usize, label: &str) {
    let guards = analyze(program).expect("native analysis").int_min_guards;
    assert_eq!(
        guards.into_iter().collect::<Vec<_>>(),
        vec![(add_pc, IntMinGuard::Arith)],
        "{label}: the guard sits on IntAdd"
    );
}

#[test]
fn integer_counting_loop_runs_natively_and_preserves_zero_one_many_iterations() {
    for (label, start, limit, expected) in [
        ("zero", 5_i64, 5_i64, 5.0_f64),
        ("one", 5_i64, 6_i64, 6.0_f64),
        ("many", 2_i64, 5_i64, 5.0_f64),
    ] {
        let source = format!(
            "indicator(\"counting loop\")\nvar w = {start}\nw := {start}\nwhile w < {limit}\n    w := w + 1\nplot(w, \"counter\")\n"
        );
        let compiled = compile(&source, CompileOptions::all());
        assert!(compiled.success, "{label}: {:?}", compiled.diagnostics);
        let program: Program = compiled.program.expect("successful compile has CSIL");
        let code = &program.main().code;
        let add_pc = code
            .iter()
            .position(|op| matches!(op, CsilInstr::IntAdd))
            .expect("compiled loop contains integer addition");
        assert!(code.iter().any(|op| matches!(op, CsilInstr::CmpLt)));
        assert!(code
            .iter()
            .any(|op| matches!(op, CsilInstr::JumpIfFalse { .. })));

        let module = compile_standalone_program(&program)
            .unwrap_or_else(|error| panic!("{label}: the guarded loop lowers natively: {error}"));
        assert_only_the_addition_is_guarded(&program, add_pc, label);
        assert_eq!(
            run_one_bar(&module),
            (status::OK, expected),
            "{label}: the module matches the VM"
        );

        let result = run_program(&program, &one_bar(), &RunOptions::default());
        assert!(
            result.runtime_errors.is_empty(),
            "{label}: {:#?}",
            result.runtime_errors
        );
        assert!(
            result.diagnostics.is_empty(),
            "{label}: {:#?}",
            result.diagnostics
        );
        assert_eq!(result.bars_executed, 1, "{label}");
        assert_eq!(result.plots.len(), 1, "{label}");
        assert_eq!(
            result.plots[0].values,
            vec![Value::Float(expected)],
            "{label}"
        );
    }
}

#[test]
fn signed_counting_loop_boundaries_preserve_exact_integer_exit() {
    for (label, start, limit) in [
        ("negative", -3_i64, 0_i64),
        ("minimum-adjacent", i64::MIN + 1, i64::MIN + 2),
        ("maximum-adjacent", i64::MAX - 1, i64::MAX),
        ("maximum-zero", i64::MAX, i64::MAX),
    ] {
        let source = format!(
            "indicator(\"signed boundary\")\nvar w = {start}\nw := {start}\nwhile w < {limit}\n    w := w + 1\nplot(w == {limit} ? 1 : 0, \"exact exit\")\n"
        );
        let compiled = compile(&source, CompileOptions::all());
        assert!(compiled.success, "{label}: {:?}", compiled.diagnostics);
        let program = compiled.program.expect("compiled signed loop");
        let add_pc = program
            .main()
            .code
            .iter()
            .position(|op| matches!(op, CsilInstr::IntAdd))
            .expect("signed loop retains IntAdd");
        let module = compile_standalone_program(&program)
            .unwrap_or_else(|error| panic!("{label}: the guarded loop lowers natively: {error}"));
        assert_only_the_addition_is_guarded(&program, add_pc, label);
        assert_eq!(
            run_one_bar(&module),
            (status::OK, 1.0),
            "{label}: no iteration reaches the minimum, so the guard stays silent"
        );
        let result = run_program(&program, &one_bar(), &RunOptions::default());
        assert!(
            result.runtime_errors.is_empty(),
            "{label}: {:?}",
            result.runtime_errors
        );
        assert!(
            result.diagnostics.is_empty(),
            "{label}: {:?}",
            result.diagnostics
        );
        assert_eq!(result.bars_executed, 1, "{label}");
        assert_eq!(result.plots.len(), 1, "{label}");
        assert_eq!(
            result.plots[0].values,
            vec![Value::Float(1.0)],
            "{label}: compare the integer counter before float conversion"
        );
    }
}
