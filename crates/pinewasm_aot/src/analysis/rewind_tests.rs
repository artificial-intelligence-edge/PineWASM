//! Resuming a simulation after a pin against replaying it.
//!
//! [`super::TypeInference::run_resumed`] returns to the first block step that
//! looked at a newly pinned variable; [`super::TypeInference::run_from_scratch`]
//! is the from-scratch inference, which replays the whole program. With
//! [`SHADOW`] set, every inference on this thread runs both and requires the
//! same answer: the same types, host sites and stack depths, or the same
//! error. The tests here run the analysis of every configuration the
//! compilers use that way, over the fixture corpus, over programs built to
//! pin late, and over a seeded sweep of generated programs. The last tests
//! bound how much of a program one restart simulates again.

use super::{analyze, analyze_with, IntInputBoundary, StrategyBroker, SHADOW, SIMULATIONS, STEPS};
use crate::{
    compile_program_to_wasm, standalone_eligibility, StandaloneWasmBuildConfig,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions, Program};

/// Run `body` with every inference shadowed by the from-scratch replay.
fn shadowed<T>(body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SHADOW.with(|shadow| shadow.set(false));
        }
    }
    SHADOW.with(|shadow| shadow.set(true));
    let _reset = Reset;
    body()
}

/// Every analysis the chart compiler, the standalone compiler and the
/// standalone eligibility check run on `program`: as written, sliced, with
/// its kept quotients carried, with and without the native heap.
fn analyze_every_way(program: &Program) {
    let _ = analyze(program);
    for strategy_heap in [false, true] {
        for broker in [StrategyBroker::Linked, StrategyBroker::Host] {
            let _ = analyze_with(
                program,
                IntInputBoundary::RejectsSentinel,
                strategy_heap,
                broker,
            );
        }
    }
    let _ = compile_program_to_wasm(program);
    for module_type in [
        StandaloneWasmModuleType::Strategy,
        StandaloneWasmModuleType::Indicator,
    ] {
        for signal_slice in [false, true] {
            let options = StandaloneWasmOptions {
                signal_plot: None,
                module_type,
            };
            let config = StandaloneWasmBuildConfig {
                signal_slice,
                ..StandaloneWasmBuildConfig::default()
            };
            let _ = standalone_eligibility(program, &options, &config);
        }
    }
}

fn compiled(source: &str) -> Option<Program> {
    compile(source, CompileOptions::default()).program
}

/// [`compiled`] for a source the front end must accept.
fn must_compile(source: &str) -> Program {
    let result = compile(source, CompileOptions::default());
    match result.program {
        Some(program) => program,
        None => panic!("{:?}\n{source}", result.diagnostics.first()),
    }
}

fn corpus() -> Vec<(std::path::PathBuf, Program)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in [
        "tests/compile-pass",
        "tests/regression",
        "benchmarks/workloads",
    ] {
        for entry in std::fs::read_dir(root.join(dir)).expect("corpus directory") {
            let path = entry.expect("corpus entry").path();
            if path.extension().is_some_and(|ext| ext == "pine") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path).expect("corpus file");
            compiled(&source).map(|program| (path, program))
        })
        .collect()
}

#[test]
fn corpus_programs_type_identically() {
    let programs = corpus();
    assert!(programs.len() >= 40, "{} corpus programs", programs.len());
    shadowed(|| {
        for (_, program) in &programs {
            analyze_every_way(program);
        }
    });
}

/// `count` variables declared `na` at the top and given their type only by a
/// use after all the declarations: each pin has to return to a declaration
/// far behind the instruction that made it.
fn late_typed(count: usize) -> String {
    let mut source = String::from("indicator('Late')\n");
    for i in 0..count {
        let kind = ["float", "int", "color"][i % 3];
        source.push_str(&format!("{kind} v{i} = na\n"));
    }
    for i in 0..count {
        let value = match i % 3 {
            0 => "close * 2.0",
            1 => "bar_index + 1",
            _ => "close > open ? color.red : color.blue",
        };
        source.push_str(&format!(
            "if bar_index % {} == 0\n    v{i} := {value}\n",
            i + 2
        ));
    }
    source.push_str("float total = 0.0\n");
    for i in 0..count {
        match i % 3 {
            0 => source.push_str(&format!("total += nz(v{i})\n")),
            1 => source.push_str(&format!("total += nz(v{i}) * 1.0\n")),
            _ => source.push_str(&format!("plot(close, color = v{i})\n")),
        }
    }
    source.push_str("plot(total)\n");
    source
}

/// Loop-carried and self-referencing state: slots read before their store,
/// history reads, and `na` joined with a typed value at a merge.
fn carried(count: usize) -> String {
    let mut source = String::from("indicator('Carried')\n");
    for i in 0..count {
        source.push_str(&format!(
            "var float a{i} = na\na{i} := nz(a{i}[1], close) + (close > open ? 1.0 : na)\n"
        ));
        source.push_str(&format!(
            "var int n{i} = na\nn{i} := na(n{i}) ? 0 : n{i} + 1\n"
        ));
        source.push_str(&format!(
            "float s{i} = 0.0\nfor k = 0 to 3\n    s{i} += na(a{i}[k]) ? 0.0 : a{i}[k]\n"
        ));
        source.push_str(&format!("x{i} = n{i} > 2 ? s{i} : na\n"));
        source.push_str(&format!("plot(nz(x{i}) + a{i})\n"));
    }
    source
}

/// Collections and objects: the programs the native-heap typing retypes.
fn heaped(count: usize) -> String {
    let mut source =
        String::from("indicator('Heaped')\ntype Pair\n    float left\n    float right\n");
    for i in 0..count {
        source.push_str(&format!(
            "var array<float> xs{i} = array.new_float()\narray.push(xs{i}, close)\n"
        ));
        source.push_str(&format!(
            "if array.size(xs{i}) > {}\n    array.shift(xs{i})\n",
            i + 3
        ));
        source.push_str(&format!(
            "Pair p{i} = Pair.new(array.sum(xs{i}), na)\nfloat q{i} = na\n"
        ));
        source.push_str(&format!(
            "if array.size(xs{i}) > 1\n    q{i} := array.get(xs{i}, 0) + p{i}.left\n"
        ));
        source.push_str(&format!("plot(nz(q{i}) + nz(p{i}.right))\n"));
    }
    source
}

#[test]
fn late_and_carried_pins_type_identically() {
    shadowed(|| {
        for count in [1, 2, 3, 5, 8, 21, 60] {
            for source in [late_typed(count), carried(count), heaped(count)] {
                analyze_every_way(&must_compile(&source));
            }
        }
    });
}

/// SplitMix64: a fixed stream per seed, with no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// One generated variable: its name and the type its declaration states.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Float,
    Int,
    Bool,
    Color,
}

impl Kind {
    fn keyword(self) -> &'static str {
        match self {
            Kind::Float => "float",
            Kind::Int => "int",
            Kind::Bool => "bool",
            Kind::Color => "color",
        }
    }
}

/// An expression of `kind` over the variables declared so far. `na` arms,
/// history reads and `nz` are frequent on purpose: they are what leaves a
/// producer untyped until something later states its type. A bool is never
/// `na`.
fn expr(rng: &mut Rng, kind: Kind, vars: &[(String, Kind)], depth: usize) -> String {
    let same: Vec<&String> = vars
        .iter()
        .filter(|(_, k)| *k == kind)
        .map(|(name, _)| name)
        .collect();
    let leaf = |rng: &mut Rng| -> String {
        if !same.is_empty() && rng.below(3) != 0 {
            let name = same[rng.below(same.len())];
            return if rng.below(4) == 0 {
                format!("{name}[{}]", 1 + rng.below(3))
            } else {
                name.clone()
            };
        }
        match kind {
            Kind::Float => ["close", "open", "1.5", "high - low"][rng.below(4)].to_string(),
            Kind::Int => ["bar_index", "3", "bar_index % 7"][rng.below(3)].to_string(),
            Kind::Bool => ["close > open", "true", "bar_index % 2 == 0"][rng.below(3)].to_string(),
            Kind::Color => ["color.red", "color.blue", "#11223344"][rng.below(3)].to_string(),
        }
    };
    if depth == 0 {
        return leaf(rng);
    }
    let cond = |rng: &mut Rng| expr(rng, Kind::Bool, vars, depth - 1);
    match (kind, rng.below(6)) {
        (Kind::Float | Kind::Int | Kind::Color, 0) => format!(
            "({} ? {} : na)",
            cond(rng),
            expr(rng, kind, vars, depth - 1)
        ),
        (Kind::Float | Kind::Int | Kind::Color, 1) => format!(
            "({} ? na : {})",
            cond(rng),
            expr(rng, kind, vars, depth - 1)
        ),
        (Kind::Bool, 0) => format!("na({})", expr(rng, Kind::Int, vars, depth - 1)),
        (Kind::Bool, 1) => format!(
            "({} > {})",
            expr(rng, Kind::Float, vars, depth - 1),
            expr(rng, Kind::Float, vars, depth - 1)
        ),
        (Kind::Float | Kind::Int, 2) => format!(
            "nz({}, {})",
            expr(rng, kind, vars, depth - 1),
            expr(rng, kind, vars, depth - 1)
        ),
        (Kind::Float, 3) => format!(
            "({} + {} * 0.5)",
            expr(rng, kind, vars, depth - 1),
            expr(rng, Kind::Float, vars, depth - 1)
        ),
        (Kind::Int, 3) => format!(
            "({} + {})",
            expr(rng, kind, vars, depth - 1),
            expr(rng, Kind::Int, vars, depth - 1)
        ),
        (Kind::Bool, 2) => format!("na({})", expr(rng, Kind::Float, vars, depth - 1)),
        (Kind::Bool, 3) => format!(
            "({} and not {})",
            expr(rng, kind, vars, depth - 1),
            cond(rng)
        ),
        _ => leaf(rng),
    }
}

/// A script of declarations (often `na`), conditional and loop
/// reassignments, and uses that state a type late.
fn generated(seed: u64) -> String {
    let mut rng = Rng(seed);
    let mut source = String::from("indicator('Generated')\n");
    let mut vars: Vec<(String, Kind)> = Vec::new();
    let statements = 4 + rng.below(28);
    for index in 0..statements {
        let kind = [Kind::Float, Kind::Int, Kind::Bool, Kind::Color][rng.below(4)];
        match rng.below(7) {
            0..=2 => {
                let name = format!("v{index}");
                let persistent = if rng.below(3) == 0 { "var " } else { "" };
                let init = if kind != Kind::Bool && rng.below(2) == 0 {
                    "na".to_string()
                } else {
                    expr(&mut rng, kind, &vars, 2)
                };
                source.push_str(&format!("{persistent}{} {name} = {init}\n", kind.keyword()));
                vars.push((name, kind));
            }
            3 | 4 if !vars.is_empty() => {
                let (name, kind) = vars[rng.below(vars.len())].clone();
                let cond = expr(&mut rng, Kind::Bool, &vars, 1);
                let then = expr(&mut rng, kind, &vars, 2);
                source.push_str(&format!("if {cond}\n    {name} := {then}\n"));
                if rng.below(2) == 0 {
                    let other = expr(&mut rng, kind, &vars, 1);
                    source.push_str(&format!("else\n    {name} := {other}\n"));
                }
            }
            5 if !vars.is_empty() => {
                let (name, kind) = vars[rng.below(vars.len())].clone();
                let value = expr(&mut rng, kind, &vars, 1);
                source.push_str(&format!(
                    "for k{index} = 0 to {}\n    {name} := {value}\n",
                    1 + rng.below(3)
                ));
            }
            _ => {
                let value = expr(&mut rng, Kind::Float, &vars, 2);
                let color = expr(&mut rng, Kind::Color, &vars, 1);
                source.push_str(&format!("plot({value}, color = {color})\n"));
            }
        }
    }
    for (name, kind) in &vars {
        match kind {
            Kind::Float => source.push_str(&format!("plot({name})\n")),
            Kind::Int => source.push_str(&format!("plot({name} * 1.0)\n")),
            Kind::Bool => source.push_str(&format!("plot({name} ? 1.0 : 0.0)\n")),
            Kind::Color => source.push_str(&format!("plot(close, color = {name})\n")),
        }
    }
    source
}

#[test]
fn generated_programs_type_identically() {
    let mut compiled_count = 0;
    shadowed(|| {
        for seed in 0..600 {
            if let Some(program) = compiled(&generated(seed)) {
                compiled_count += 1;
                analyze_every_way(&program);
            }
        }
    });
    // The sweep is only evidence while the front end accepts what it writes.
    assert!(
        compiled_count >= 300,
        "{compiled_count} generated programs compiled"
    );
}

/// The restarts and the instructions simulated for one native analysis.
fn work_for(source: &str) -> (usize, usize, usize) {
    let program = must_compile(source);
    SIMULATIONS.with(|count| count.set(0));
    STEPS.with(|count| count.set(0));
    analyze(&program).expect("the program lowers natively");
    (
        SIMULATIONS.with(std::cell::Cell::get),
        STEPS.with(std::cell::Cell::get),
        program.main().code.len(),
    )
}

/// `count` uses of an `na`-initialized local, each next to its declaration:
/// every one pins twice (the slot from its use, then the `na` from the
/// store), a few instructions behind the pin.
fn local_pins(count: usize) -> String {
    let mut source = String::from("indicator('Local')\nfloat total = 0.0\n");
    for i in 0..count {
        source.push_str(&format!(
            "float t{i} = na\nif close > open\n    t{i} := close - open\ntotal += nz(t{i})\n"
        ));
    }
    source.push_str("plot(total)\n");
    source
}

#[test]
fn a_restart_simulates_its_neighbourhood_again_not_the_program() {
    let (simulations, steps, code_len) = work_for(&local_pins(300));
    // The pins are still made one restart at a time.
    assert!(simulations > 300, "{simulations} simulations");
    // Replaying from the first block cost about `simulations * code_len / 2`
    // steps; resuming costs a few blocks per restart plus the passes that
    // settle the defaults.
    assert!(
        steps <= 12 * code_len,
        "{steps} steps for {code_len} instructions and {simulations} simulations"
    );
}

#[test]
fn the_work_per_instruction_does_not_grow_with_the_program() {
    let per_instruction = |count| {
        let (_, steps, code_len) = work_for(&local_pins(count));
        steps as f64 / code_len as f64
    };
    let small = per_instruction(40);
    let large = per_instruction(640);
    assert!(
        large <= small * 1.5 + 1.0,
        "{small:.2} steps per instruction at 40 variables, {large:.2} at 640"
    );
}
