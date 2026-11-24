//! Keeping inference state across a restart must agree with discarding it.
//!
//! The per-instruction tables live in the simulation that
//! [`TypeInference::run_resumed`] rewinds, and are not rebuilt at a restart.
//! These cases hold that retained state to the answer, the error and the
//! number of simulations of [`TypeInference::run_from_scratch`], which starts
//! every simulation on fresh tables.
use super::{
    infer_collection_elems, recover_cfg, InferenceMode, StrategyBroker, TypeInference, TypedOut,
    SIMULATIONS,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram};
use std::collections::BTreeMap;

/// The inference of `program` and the simulations it took, with the state
/// kept across restarts (`reuse`) or rebuilt at each one.
fn infer(program: &CsilProgram, reuse: bool) -> (Result<TypedOut, String>, usize) {
    let cfg = recover_cfg(&program.main().code).unwrap();
    let facts = infer_collection_elems(
        program,
        &cfg.blocks,
        &cfg.term,
        &cfg.rpo_order,
        &BTreeMap::new(),
    );
    let infer = TypeInference::new(
        program,
        &cfg.blocks,
        &cfg.term,
        &cfg.rpo_order,
        &facts,
        None,
        InferenceMode::imported_host(StrategyBroker::Host),
    )
    .unwrap();
    SIMULATIONS.with(|count| count.set(0));
    let typed = if reuse {
        infer.run_resumed()
    } else {
        infer.run_from_scratch()
    };
    (
        typed.map_err(|error| format!("{error:?}")),
        SIMULATIONS.with(std::cell::Cell::get),
    )
}

#[test]
fn reused_tables_match_fresh_tables_across_type_demands_and_loops() {
    let cases = [
        "indicator('Accumulator')\nvar float total = na\ntotal := nz(total) + close\nplot(total)\n",
        "indicator('Int')\nvar int total = na\ntotal := nz(total) + 1\nplot(total)\n",
        "indicator('Branch')\nvar float x = na\nif close > open\n    x := close\nelse\n    x := open\nplot(x)\n",
        "indicator('Loop')\nfloat total = 0\nfor i = 0 to 4\n    total += close[i]\nplot(total)\n",
        "indicator('Arrays')\nvar a = array.new_float()\narray.push(a, close)\nplot(array.get(a, 0))\n",
        "indicator('Strings')\nvar string s = na\ns := str.tostring(close)\nplot(str.length(s))\n",
        "indicator('Bool')\nvar bool b = false\nb := close > open\nplot(b ? 1 : 0)\n",
    ];
    let mut saw_restart = false;
    for source in cases {
        let compiled = compile(source, CompileOptions::default());
        let program = compiled.program.expect("test source compiles");
        let reused = infer(&program, true);
        let fresh = infer(&program, false);
        assert!(reused.0.is_ok(), "{source}: {:?}", reused.0);
        saw_restart |= reused.1 > 1;
        assert_eq!(reused, fresh, "{source}");
    }
    assert!(
        saw_restart,
        "corpus must exercise metadata retention across retries"
    );
}

#[test]
fn reused_tables_preserve_type_conflicts_and_error_order() {
    let compiled = compile(
        "indicator('Conflict')\nvar float x = na\nx := nz(x) + close\nplot(x)\n",
        CompileOptions::default(),
    );
    let mut program = compiled.program.unwrap();
    // Corrupt the final plot consumer after earlier instructions have
    // required inference retries. Both storage policies must reject it at
    // the same instruction with the same diagnostic.
    let code = &mut program.functions[0].code;
    let plot = code
        .iter()
        .position(|op| {
            matches!(
                op,
                CsilInstr::PlotF64 { .. } | CsilInstr::PlotStyledF64 { .. }
            )
        })
        .expect("fixture has a plot consumer");
    code.insert(plot, CsilInstr::BoolNot);
    let reused = infer(&program, true);
    let fresh = infer(&program, false);
    assert!(reused.0.is_err());
    assert!(reused.1 > 1, "conflict must occur after a restart");
    assert_eq!(reused, fresh);
}
