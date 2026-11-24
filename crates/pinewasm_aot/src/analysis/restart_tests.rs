//! How many whole-program simulations type inference runs.
//!
//! Each restart replays the whole program, so a restart per variable makes
//! inference quadratic in program size. A store that pins a slot nothing has
//! read yet needs no restart; these tests bound the count. The emitter byte
//! gate checks that the types, and so the bytes, are unchanged.

use super::{analyze, SIMULATIONS};
use pinewasm_compiler::{compile, CompileOptions};

fn simulations_for(source: &str) -> usize {
    let program = compile(source, CompileOptions::default())
        .program
        .expect("the source compiles");
    SIMULATIONS.with(|count| count.set(0));
    analyze(&program).expect("the program lowers natively");
    SIMULATIONS.with(std::cell::Cell::get)
}

/// Two reads per variable keep every store (a single-use store/reload pair
/// is coalesced away before analysis).
fn chain(variables: usize) -> String {
    let mut source = String::from("indicator('Chain')\nv0 = close\n");
    for i in 1..variables {
        let prev = i - 1;
        source.push_str(&format!("v{i} = v{prev} * 0.5 + v{prev} * 0.25\n"));
    }
    source.push_str(&format!("plot(v{})\n", variables - 1));
    source
}

#[test]
fn variables_stored_before_any_read_need_no_restart_each() {
    let simulations = simulations_for(&chain(200));
    assert!(
        simulations <= 3,
        "{simulations} simulations for 200 chained variables"
    );
}

#[test]
fn the_count_does_not_grow_with_the_number_of_variables() {
    assert_eq!(simulations_for(&chain(20)), simulations_for(&chain(400)));
}

/// A slot read before its first store (a `var` that reads its previous value
/// first) still restarts: the read already pushed the untyped value.
#[test]
fn a_slot_read_before_its_store_still_types() {
    let source =
        "indicator('Accumulate')\nvar float total = na\ntotal := nz(total) + close\nplot(total)\n";
    let program = compile(source, CompileOptions::default())
        .program
        .expect("the source compiles");
    let analysis = analyze(&program).expect("the program lowers natively");
    assert!(analysis.slot_ty.iter().all(|ty| *ty == super::VTy::F64));
}
