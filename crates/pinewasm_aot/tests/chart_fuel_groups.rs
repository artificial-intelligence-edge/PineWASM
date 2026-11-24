//! Every fuel-window fast arm of every chart module and every standalone
//! WebAssembly module the fixtures and the typed matrix produce, checked on
//! the emitted bytes.
//!
//! A window precharges its summed cost before its first instruction runs
//! (`body/chart_fuel.rs`). That is sound only while no grouped instruction
//! can observe either counter or leave the window except by stopping the
//! run. The emitter refuses a module whose grouped instruction emits a
//! counter access; this test checks the same obligations on the bytes,
//! independently of the emitter's own bookkeeping. In each fast arm:
//!
//! - no `local.get`, `local.set` or `local.tee` names a counter local (1 is
//!   the per-bar counter, 2 the run counter, in `run` and `bar_step` alike);
//! - no branch leaves the arm, to the group's own `if` or beyond;
//! - every `return` returns a nonzero status: a nonzero constant, or the
//!   result of the shared `stop` function called with a nonzero status
//!   constant;
//! - no call reaches an import (`host.vm` exchanges the per-bar counter),
//!   and a fast arm calls neither `budget_stop` nor `charge`, and `stop`
//!   only to return its status.
//!
//! It also checks the per-bar split: `run` keeps no window of its own and
//! calls `bar_step` with the bar. A chart module's `bar_step` is its last
//! function; standalone WebAssembly's is the one before the entry point.
//! Both follow their `stop`, `budget_stop` and `charge` functions. A chart
//! module whose body is split (`body/split.rs`) keeps its windows in the
//! pieces `bar_step` reaches, which sit before the stop functions; no other
//! function has one.
//!
//! ```text
//! cargo test -p pinewasm_aot --test chart_fuel_groups -- --nocapture
//! ```

use pinewasm_aot::{
    compile_program_to_wasm, compile_standalone_wasm_program, ExecutionMode,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use std::path::{Path, PathBuf};
use wasmparser::{Operator, Parser, Payload, TypeRef, Validator};

/// The typed matrix's tables and enumeration.
#[path = "typed_matrix/cells.rs"]
mod matrix;

const BUDGET: u32 = 1;
const RUN_BUDGET: u32 = 2;

const FIXTURE_ROOT: &str = "../../tests";
const FIXTURE_DIRECTORIES: &[&str] = &["compile-pass", "regression"];

fn collect_pine_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable fixture directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "generated_failures")
            {
                continue;
            }
            collect_pine_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "pine")
        {
            out.push(path);
        }
    }
}

/// Every fixture and matrix cell, as `(id, source)`.
fn sources() -> Vec<(String, String)> {
    let root = Path::new(FIXTURE_ROOT);
    let mut out = Vec::new();
    for directory in FIXTURE_DIRECTORIES {
        let mut files = Vec::new();
        collect_pine_files(&root.join(directory), &mut files);
        files.sort();
        for path in files {
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            out.push((format!("fixture:{}", path.display()), source));
        }
    }
    for cell in matrix::cells() {
        out.push((format!("matrix:{}", cell.id), cell.program));
    }
    out
}

/// What one module contributed.
#[derive(Default)]
struct Tally {
    modules: usize,
    groups: usize,
    largest: i64,
    /// Pieces of split bodies.
    pieces: usize,
}

/// The cost of the guard/debit prelude that starts `ops`, if one does.
fn prelude_cost(ops: &[Operator]) -> Option<i64> {
    match ops {
        [Operator::LocalGet {
            local_index: BUDGET,
        }, Operator::I64Const { value: bar_cost }, Operator::I64GeU, Operator::LocalGet {
            local_index: RUN_BUDGET,
        }, Operator::I64Const { value: run_cost }, Operator::I64GeU, Operator::I32And, Operator::If {
            blockty: wasmparser::BlockType::Empty,
        }, Operator::LocalGet {
            local_index: BUDGET,
        }, Operator::I64Const {
            value: debit_bar_cost,
        }, Operator::I64Sub, Operator::LocalSet {
            local_index: BUDGET,
        }, Operator::LocalGet {
            local_index: RUN_BUDGET,
        }, Operator::I64Const {
            value: debit_run_cost,
        }, Operator::I64Sub, Operator::LocalSet {
            local_index: RUN_BUDGET,
        }, ..]
            if bar_cost == run_cost && bar_cost == debit_bar_cost && bar_cost == debit_run_cost =>
        {
            Some(*bar_cost)
        }
        _ => None,
    }
}

/// A native module's stop functions (`stop`, `budget_stop`) and its
/// fallback-arm `charge`.
#[derive(Clone, Copy)]
struct Stops {
    stop: u32,
    budget_stop: u32,
    charge: u32,
}

/// Whether the `return` at `ops[at]` returns a nonzero status: a constant,
/// or `stop(code, pc, bar)` with a nonzero constant `code`.
fn returns_a_status(ops: &[Operator], at: usize, stops: Option<Stops>) -> bool {
    if matches!(ops[at - 1], Operator::I32Const { value } if value != 0) {
        return true;
    }
    let Some(stops) = stops else {
        return false;
    };
    at >= 5
        && matches!(ops[at - 1], Operator::Call { function_index } if function_index == stops.stop)
        && matches!(ops[at - 2], Operator::LocalGet { local_index: 0 })
        && matches!(ops[at - 3], Operator::I32Const { .. })
        && matches!(ops[at - 4], Operator::I32Const { value } if value != 0)
}

/// Check the fast arm that starts at `ops[start]` (just after the debits) and
/// return the index of its `else`.
fn check_fast_arm(
    ops: &[Operator],
    start: usize,
    imports: u32,
    stops: Option<Stops>,
    context: &str,
) -> usize {
    // Open labels since the prelude, the group's own `if` included.
    let mut depth = 1_usize;
    let mut at = start;
    loop {
        let op = ops
            .get(at)
            .unwrap_or_else(|| panic!("{context}: fast arm at {start} never reaches its `else`"));
        let escapes = |relative: u32| relative as usize + 1 >= depth;
        match op {
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => depth += 1,
            Operator::Else if depth == 1 => return at,
            Operator::End => {
                depth -= 1;
                assert!(
                    depth > 0,
                    "{context}: fast arm at {start} closes without `else`"
                );
            }
            Operator::LocalGet { local_index }
            | Operator::LocalSet { local_index }
            | Operator::LocalTee { local_index } => assert!(
                *local_index != BUDGET && *local_index != RUN_BUDGET,
                "{context}: op {at} in the fast arm at {start} uses counter local {local_index}"
            ),
            Operator::Br { relative_depth } | Operator::BrIf { relative_depth } => assert!(
                !escapes(*relative_depth),
                "{context}: op {at} branches out of the fast arm at {start}"
            ),
            Operator::BrTable { targets } => {
                assert!(
                    !escapes(targets.default()),
                    "{context}: op {at} branches out of the fast arm at {start}"
                );
                for target in targets.targets() {
                    assert!(
                        !escapes(target.unwrap()),
                        "{context}: op {at} branches out of the fast arm at {start}"
                    );
                }
            }
            Operator::Return => assert!(
                returns_a_status(ops, at, stops),
                "{context}: op {at} in the fast arm at {start} returns without a status: {:?}",
                &ops[at.saturating_sub(8)..=at]
            ),
            Operator::Call { function_index } => {
                assert!(
                    *function_index >= imports,
                    "{context}: op {at} in the fast arm at {start} calls an import"
                );
                if let Some(stops) = stops {
                    assert!(
                        *function_index != stops.budget_stop && *function_index != stops.charge,
                        "{context}: op {at} in the fast arm at {start} charges a budget"
                    );
                    assert!(
                        *function_index != stops.stop
                            || matches!(ops.get(at + 1), Some(Operator::Return)),
                        "{context}: op {at} in the fast arm at {start} stops without returning"
                    );
                }
            }
            Operator::CallIndirect { .. }
            | Operator::ReturnCall { .. }
            | Operator::ReturnCallIndirect { .. }
            | Operator::Unreachable => {
                panic!("{context}: op {at} in the fast arm at {start} is {op:?}")
            }
            _ => {}
        }
        at += 1;
    }
}

fn check_module(id: &str, wasm: &[u8], standalone: bool, tally: &mut Tally) {
    Validator::new()
        .validate_all(wasm)
        .unwrap_or_else(|error| panic!("{id}: invalid module: {error}"));
    let mut imports = 0;
    let mut run = None;
    let mut bodies = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.unwrap() {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    imports += u32::from(matches!(import.unwrap().ty, TypeRef::Func(_)));
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.unwrap();
                    if export.name == "run" {
                        run = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let ops: Vec<Operator> = body
                    .get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(Result::unwrap)
                    .collect();
                bodies.push(ops);
            }
            _ => {}
        }
    }
    let bar_step = if standalone {
        assert_eq!(imports, 0, "{id}: a standalone module imports nothing");
        bodies.len() - 2
    } else {
        bodies.len() - 1
    };
    // Function indices count the imports; body indices do not.
    let first_stop = bar_step as u32 + imports - 3;
    let stops = Some(Stops {
        stop: first_stop,
        budget_stop: first_stop + 1,
        charge: first_stop + 2,
    });
    let calls_bar_step = |ops: &[Operator]| {
        ops.windows(2).any(|pair| {
            matches!(pair, [Operator::LocalGet { local_index: 0 }, Operator::Call { function_index }]
                if *function_index as usize == bar_step + imports as usize)
        })
    };
    // A split body's pieces: called with the bar, their result kept or (the
    // first piece) returned by `bar_step`, between `run` and the stops,
    // reached from `bar_step` or another piece.
    let mut pieces: Vec<usize> = Vec::new();
    if let Some(run) = run {
        let run = (run - imports) as usize;
        let mut work = vec![bar_step];
        while let Some(caller) = work.pop() {
            let ops = &bodies[caller];
            for i in 1..ops.len().saturating_sub(1) {
                if let (
                    Operator::LocalGet { local_index: 0 },
                    Operator::Call { function_index },
                    Operator::LocalTee { .. } | Operator::End,
                ) = (&ops[i - 1], &ops[i], &ops[i + 1])
                {
                    let Some(body) = (*function_index as usize).checked_sub(imports as usize)
                    else {
                        continue;
                    };
                    if body > run
                        && body < (first_stop - imports) as usize
                        && !pieces.contains(&body)
                    {
                        pieces.push(body);
                        work.push(body);
                    }
                }
            }
        }
    }
    match run {
        Some(run) => {
            let run = (run - imports) as usize;
            assert!(bar_step > run, "{id}: bar_step must follow run");
            assert!(
                calls_bar_step(&bodies[run]),
                "{id}: run must hand each bar to bar_step"
            );
        }
        None => {
            assert!(standalone, "{id}: a chart module exports run");
            let callers = bodies.iter().filter(|ops| calls_bar_step(ops)).count();
            assert_eq!(callers, 1, "{id}: run must hand each bar to bar_step");
        }
    }
    for (function, ops) in bodies.iter().enumerate() {
        for at in 0..ops.len() {
            let Some(cost) = prelude_cost(&ops[at..]) else {
                continue;
            };
            assert!(
                function == bar_step || pieces.contains(&function),
                "{id}: a window outside bar_step and its pieces"
            );
            assert!(cost > 0, "{id}: a window that charges nothing");
            check_fast_arm(ops, at + 16, imports, stops, id);
            tally.groups += 1;
            tally.largest = tally.largest.max(cost);
        }
    }
    tally.pieces += pieces.len();
    tally.modules += 1;
}

#[test]
fn every_chart_window_keeps_its_counters_private() {
    let mut tally = Tally::default();
    for (id, source) in sources() {
        let Some(program) = compile(&source, CompileOptions::default()).program else {
            continue;
        };
        let Ok(module) = compile_program_to_wasm(&program) else {
            continue;
        };
        if !matches!(module.layout.execution_mode, ExecutionMode::Native) {
            // The emitter's own refusal falls back to the hosted tier; it
            // must never be the reason a module is not native.
            let blocker = module.layout.native_blocker.unwrap_or_default();
            assert!(!blocker.contains("fuel group"), "{id}: {blocker}");
            continue;
        }
        check_module(&id, &module.wasm, false, &mut tally);
    }
    eprintln!(
        "{} chart modules, {} windows, largest precharge {}",
        tally.modules, tally.groups, tally.largest
    );
    assert!(tally.modules > 100, "{} chart modules", tally.modules);
    assert!(tally.groups > 1_000, "{} windows", tally.groups);
    // Chart windows outgrow the standalone kernel, which never precharges
    // more than eight.
    assert!(tally.largest > 8, "largest precharge {}", tally.largest);
}

/// The same obligations on a split body's pieces (`body/split.rs`): a
/// generated script long enough to split keeps every window in a piece, and
/// each piece's windows never access either counter.
#[test]
fn every_split_piece_keeps_its_counters_private() {
    let mut source = String::from("//@version=6\nindicator(\"Split pieces\")\nfloat total = 0.0\n");
    for i in 0..60 {
        source.push_str(&format!(
            "float a{i} = close * {}.0 - open\nif a{i} > high\n    a{i} := a{i} - high\n\
             float b{i} = a{i} > close ? a{i} - close : close - a{i}\n\
             var float acc{i} = 0.0\nacc{i} += b{i} * 0.001\ntotal += acc{i}\n",
            i + 1
        ));
    }
    source.push_str("plot(total)\n");
    let program = compile(&source, CompileOptions::default())
        .program
        .expect("the generated script compiles");
    let module = compile_program_to_wasm(&program).expect("the script compiles to Wasm");
    assert!(matches!(
        module.layout.execution_mode,
        ExecutionMode::Native
    ));
    let mut tally = Tally::default();
    check_module("split", &module.wasm, false, &mut tally);
    assert!(tally.pieces > 2, "{} pieces", tally.pieces);
    assert!(tally.groups > 100, "{} windows", tally.groups);
}

/// The same obligations on standalone WebAssembly, which shares the chart
/// window kernel and stops through its two shared stop functions.
#[test]
fn every_standalone_window_keeps_its_counters_private() {
    let mut tally = Tally::default();
    for (id, source) in sources() {
        let Some(program) = compile(&source, CompileOptions::default()).program else {
            continue;
        };
        // A `strategy()` source designates no plot; any other source is
        // built as a strategy-type module on its first plot.
        let options = StandaloneWasmOptions {
            signal_plot: program.metadata.strategy.is_none().then_some(0),
            module_type: StandaloneWasmModuleType::Strategy,
        };
        let wasm = match compile_standalone_wasm_program(&program, &options) {
            Ok(wasm) => wasm,
            Err(error) => {
                // A refusal is the ordinary outcome for most programs; a
                // window must never be the reason.
                let error = error.to_string();
                assert!(!error.contains("fuel group"), "{id}: {error}");
                continue;
            }
        };
        check_module(&id, &wasm, true, &mut tally);
    }
    eprintln!(
        "{} standalone modules, {} windows, largest precharge {}",
        tally.modules, tally.groups, tally.largest
    );
    assert!(tally.modules > 50, "{} standalone modules", tally.modules);
    assert!(tally.groups > 500, "{} windows", tally.groups);
    assert!(tally.largest > 8, "largest precharge {}", tally.largest);
}
