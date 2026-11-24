//! Every handoff between the pieces of a split per-bar body
//! (`body/split.rs`), checked on the emitted bytes.
//!
//! A chart module whose per-bar body is large runs it as a tree of
//! functions, its pieces. Between pieces, the per-bar and run counters and
//! the current-bar cells live in globals; inside a piece, both counters
//! (locals 1 and 2) and the cells its own code uses most live in locals. A
//! value a piece keeps in a local must agree with its global wherever control
//! crosses into or out of the piece. In every split module of the test
//! fixtures and of generated scripts:
//!
//! - a piece starts by loading both counters, then each cell it caches, from
//!   their globals (`global.get; local.set`), and every piece of a module
//!   names the same two counter globals;
//! - before each call to another piece, the caller stores both counters, and
//!   every cell it caches that the callee or a piece below it reads or
//!   writes, to their globals (`local.get; global.set`);
//! - after the call, a nonzero result returns at once; then the caller
//!   reloads both counters, and every cell it caches that the callee or a
//!   piece below it writes, from their globals;
//! - a piece that leaves through its exit stores both counters, and every
//!   cell it caches that its own code writes, then returns 0.
//!
//! What a piece reads and writes is read off its bytes too, never off the
//! split plan. It reads or writes every global it names. It writes every
//! global it sets except those of the cells it caches, which it sets only to
//! hand their locals on, and it writes a cell it caches when it stores to the
//! cell's local by any means but loading the cell's global.
//!
//! ```text
//! cargo test -p pinewasm_aot --test split_handoffs -- --nocapture
//! ```

use pinewasm_aot::{
    compile_program_to_wasm, compile_standalone_wasm_program, supports, AotError, ExecutionMode,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use wasmparser::{BlockType, Operator, Parser, Payload, TypeRef};

/// The counter locals of every piece: 1 is the per-bar counter, 2 the run
/// counter.
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

/// What the split modules contributed.
#[derive(Default)]
struct Tally {
    modules: usize,
    pieces: usize,
    calls: usize,
    /// Cached cells a caller stores before a call, because the callee reads
    /// or writes them; those it reloads after one, because the callee writes
    /// them; and those a piece stores when it leaves, because it wrote them.
    flushes: usize,
    reloads: usize,
    exit_stores: usize,
}

/// One piece, as its bytes describe it.
struct Piece {
    /// The globals its entry loads the per-bar and run counters from.
    counters: (u32, u32),
    /// The cells it caches: each one's global, and its local.
    cached: BTreeMap<u32, u32>,
    /// The globals its own code reads or writes, and those it writes.
    accessed: BTreeSet<u32>,
    written: BTreeSet<u32>,
    /// Its calls to other pieces: the index of each `call`, and the callee.
    calls: Vec<(usize, usize)>,
}

/// The globals a piece and every piece below it read or write, and those
/// they write.
#[derive(Clone, Default)]
struct Reach {
    accessed: BTreeSet<u32>,
    written: BTreeSet<u32>,
}

/// The `global.get g; local.set l` pairs from `ops[at]` on, as `(g, l)`, and
/// the index of the first operator after them.
fn loads(ops: &[Operator], mut at: usize) -> (Vec<(u32, u32)>, usize) {
    let mut pairs = Vec::new();
    while let (
        Some(Operator::GlobalGet { global_index }),
        Some(Operator::LocalSet { local_index }),
    ) = (ops.get(at), ops.get(at + 1))
    {
        pairs.push((*global_index, *local_index));
        at += 2;
    }
    (pairs, at)
}

/// The `local.get l; global.set g` pairs that end just before `ops[end]`, as
/// `(g, l)`.
fn stores(ops: &[Operator], mut end: usize) -> BTreeSet<(u32, u32)> {
    let mut pairs = BTreeSet::new();
    while end >= 2 {
        let (Operator::LocalGet { local_index }, Operator::GlobalSet { global_index }) =
            (&ops[end - 2], &ops[end - 1])
        else {
            break;
        };
        pairs.insert((*global_index, *local_index));
        end -= 2;
    }
    pairs
}

/// A split body's pieces, as body indices in the order found, the first piece
/// first: every function that `bar_step` (the last body) or a piece calls
/// with the bar (`local.get 0; call`) and whose result it returns or keeps,
/// between `run` and the three stop functions before `bar_step`. Empty when
/// the body is not split.
fn find_pieces(
    bodies: &[Vec<Operator>],
    imports: usize,
    run: usize,
    bar_step: usize,
) -> Vec<usize> {
    let first_stop = bar_step - 3;
    let mut found = Vec::new();
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
                let Some(body) = (*function_index as usize).checked_sub(imports) else {
                    continue;
                };
                if body > run && body < first_stop && !found.contains(&body) {
                    found.push(body);
                    work.push(body);
                }
            }
        }
    }
    found
}

/// Read one piece's handoff locals and globals, and its calls, off its bytes.
fn read_piece(context: &str, ops: &[Operator], imports: usize, pieces: &[usize]) -> Piece {
    let (entry, after) = loads(ops, 0);
    assert!(
        matches!(
            ops.get(after),
            Some(Operator::Block {
                blockty: BlockType::Empty
            })
        ),
        "{context}: its entry loads are not followed by the block it leaves through"
    );
    let counter = |local: u32| {
        let globals: Vec<u32> = entry
            .iter()
            .filter(|(_, l)| *l == local)
            .map(|(g, _)| *g)
            .collect();
        assert_eq!(
            globals.len(),
            1,
            "{context}: its entry loads counter local {local} {} times",
            globals.len()
        );
        globals[0]
    };
    let counters = (counter(BUDGET), counter(RUN_BUDGET));
    let cached: BTreeMap<u32, u32> = entry
        .iter()
        .copied()
        .filter(|(_, l)| *l != BUDGET && *l != RUN_BUDGET)
        .collect();
    let cell_of: BTreeMap<u32, u32> = cached.iter().map(|(&g, &l)| (l, g)).collect();
    assert_eq!(
        cached.len() + 2,
        entry.len(),
        "{context}: its entry loads one global twice"
    );
    assert_eq!(
        cell_of.len(),
        cached.len(),
        "{context}: two cells share a local"
    );
    let mut accessed = BTreeSet::new();
    let mut written = BTreeSet::new();
    let mut calls = Vec::new();
    for (at, op) in ops.iter().enumerate() {
        match *op {
            Operator::GlobalGet { global_index } => {
                accessed.insert(global_index);
            }
            Operator::GlobalSet { global_index } => {
                accessed.insert(global_index);
                // A cached cell's global is set only to hand its local on.
                if !cached.contains_key(&global_index) {
                    written.insert(global_index);
                }
            }
            Operator::LocalSet { local_index } | Operator::LocalTee { local_index } => {
                if let Some(&global) = cell_of.get(&local_index) {
                    let loaded = match at.checked_sub(1).map(|before| &ops[before]) {
                        Some(Operator::GlobalGet { global_index }) => *global_index == global,
                        _ => false,
                    };
                    if !loaded {
                        written.insert(global);
                    }
                }
            }
            Operator::Call { function_index } => {
                if let Some(callee) = (function_index as usize).checked_sub(imports) {
                    if pieces.contains(&callee) {
                        calls.push((at, callee));
                    }
                }
            }
            _ => {}
        }
    }
    Piece {
        counters,
        cached,
        accessed,
        written,
        calls,
    }
}

/// What `body` and every piece below it read or write, and write.
fn reach(
    body: usize,
    pieces: &BTreeMap<usize, Piece>,
    memo: &mut BTreeMap<usize, Reach>,
    depth: usize,
) -> Reach {
    if let Some(known) = memo.get(&body) {
        return known.clone();
    }
    assert!(depth <= pieces.len(), "piece {body} calls itself");
    let piece = &pieces[&body];
    let mut sets = Reach {
        accessed: piece.accessed.clone(),
        written: piece.written.clone(),
    };
    for &(_, callee) in &piece.calls {
        let below = reach(callee, pieces, memo, depth + 1);
        sets.accessed.extend(below.accessed);
        sets.written.extend(below.written);
    }
    memo.insert(body, sets.clone());
    sets
}

/// Check the call at `ops[at]` from `caller` to a piece that, together with
/// the pieces below it, reads or writes the globals in `callee`.
fn check_call(
    context: &str,
    ops: &[Operator],
    at: usize,
    caller: &Piece,
    callee: &Reach,
    tally: &mut Tally,
) {
    assert!(
        at > 0 && matches!(ops[at - 1], Operator::LocalGet { local_index: 0 }),
        "{context}: the callee is not passed the bar"
    );
    let Some(Operator::LocalTee {
        local_index: status,
    }) = ops.get(at + 1)
    else {
        panic!("{context}: the callee's result is not kept");
    };
    assert!(
        matches!(
            ops.get(at + 2..at + 6),
            Some([
                Operator::If {
                    blockty: BlockType::Empty
                },
                Operator::LocalGet { local_index },
                Operator::Return,
                Operator::End,
            ]) if local_index == status
        ),
        "{context}: a nonzero result does not return at once: {:?}",
        ops.get(at + 1..at + 6)
    );
    let before = stores(ops, at - 1);
    let after: BTreeSet<(u32, u32)> = loads(ops, at + 6).0.into_iter().collect();
    let (budget, run_budget) = caller.counters;
    for (global, local) in [(budget, BUDGET), (run_budget, RUN_BUDGET)] {
        assert!(
            before.contains(&(global, local)),
            "{context}: counter local {local} is not stored to global {global} before the call"
        );
        assert!(
            after.contains(&(global, local)),
            "{context}: counter local {local} is not reloaded from global {global} after the call"
        );
    }
    for (&global, &local) in &caller.cached {
        if callee.accessed.contains(&global) {
            assert!(
                before.contains(&(global, local)),
                "{context}: the callee uses the cell in global {global}, but its caller does not \
                 store local {local} there before the call"
            );
            tally.flushes += 1;
        }
        if callee.written.contains(&global) {
            assert!(
                after.contains(&(global, local)),
                "{context}: the callee writes the cell in global {global}, but its caller does not \
                 reload local {local} from there after the call"
            );
            tally.reloads += 1;
        }
    }
    tally.calls += 1;
}

/// Check what `piece` stores when it leaves through its exit, at the end of
/// its body.
fn check_exit(context: &str, ops: &[Operator], piece: &Piece, tally: &mut Tally) {
    let end = ops.len().saturating_sub(2);
    assert!(
        matches!(
            ops.get(end..),
            Some([Operator::I32Const { value: 0 }, Operator::End])
        ),
        "{context}: it does not end by returning 0"
    );
    let stored = stores(ops, end);
    let (budget, run_budget) = piece.counters;
    for (global, local) in [(budget, BUDGET), (run_budget, RUN_BUDGET)] {
        assert!(
            stored.contains(&(global, local)),
            "{context}: its exit does not store counter local {local} to global {global}"
        );
    }
    for (&global, &local) in &piece.cached {
        if piece.written.contains(&global) {
            assert!(
                stored.contains(&(global, local)),
                "{context}: it writes the cell in global {global}, but its exit does not store \
                 local {local} there"
            );
            tally.exit_stores += 1;
        }
    }
}

/// Check every handoff of `wasm`'s pieces; a module whose body is not split
/// adds nothing.
fn check_module(id: &str, wasm: &[u8], tally: &mut Tally) {
    let mut imports = 0;
    let mut run = None;
    let mut evaluate = None;
    let mut bodies: Vec<Vec<Operator>> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.unwrap() {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    imports += usize::from(matches!(import.unwrap().ty, TypeRef::Func(_)));
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.unwrap();
                    if export.name == "run" {
                        run = Some(export.index as usize);
                    }
                    if export.name == "evaluate_indicator_cabi" {
                        evaluate = Some(export.index as usize);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(
                body.get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(Result::unwrap)
                    .collect(),
            ),
            _ => {}
        }
    }
    // Standalone's entry point follows bar_step and calls the internal
    // run; run is the only function that calls bar_step. Chart modules
    // export run and place bar_step last.
    let bar_step = evaluate.map_or(bodies.len() - 1, |i| i - imports - 1);
    let run = run.map_or_else(
        || {
            bodies
                .iter()
                .position(|body| {
                    body.iter().any(|op| {
                        matches!(op, Operator::Call { function_index } if *function_index as usize == bar_step + imports)
                    })
                })
                .expect("standalone run calls bar_step")
        },
        |i| i - imports,
    );
    let found = find_pieces(&bodies, imports, run, bar_step);
    let Some(&first) = found.first() else {
        return;
    };
    let pieces: BTreeMap<usize, Piece> = found
        .iter()
        .map(|&body| {
            let context = format!("{id}: piece {body}");
            (body, read_piece(&context, &bodies[body], imports, &found))
        })
        .collect();
    let counters = pieces[&first].counters;
    assert_ne!(counters.0, counters.1, "{id}: both counters share a global");
    let mut memo = BTreeMap::new();
    for (&body, piece) in &pieces {
        let context = format!("{id}: piece {body}");
        assert_eq!(
            piece.counters, counters,
            "{context}: its counters live in other globals than piece {first}'s"
        );
        for &(at, callee) in &piece.calls {
            let below = reach(callee, &pieces, &mut memo, 0);
            let context = format!("{context}, call to piece {callee} at operator {at}");
            check_call(&context, &bodies[body], at, piece, &below, tally);
        }
        check_exit(&context, &bodies[body], piece, tally);
    }
    tally.modules += 1;
    tally.pieces += pieces.len();
}

/// Print what one test checked.
fn report(what: &str, tally: &Tally) {
    eprintln!(
        "{what}: {} split modules, {} pieces, {} calls; cached cells: {} stored before a call, \
         {} reloaded after one, {} stored on leaving",
        tally.modules, tally.pieces, tally.calls, tally.flushes, tally.reloads, tally.exit_stores
    );
}

/// A generated indicator long enough to split: a loop, which never moves,
/// then `groups` statement groups, the middle third inside one large `if`
/// block. Every group adds to `total`, so the pieces cache it and write it,
/// and every call hands it on in both directions.
fn split_source(groups: usize) -> String {
    let mut source = String::from(
        "//@version=6\nindicator(\"Split handoffs\")\nfloat total = 0.0\nfloat early = 0.0\n\
         for j = 0 to 4\n    early += close * j\n",
    );
    let (block_start, block_end) = (groups / 3, 2 * groups / 3);
    for i in 0..groups {
        if i == block_start {
            source.push_str("if bar_index % 3 != 1\n");
        }
        let indent = if (block_start..block_end).contains(&i) {
            "    "
        } else {
            ""
        };
        let group = format!(
            "float a{i} = close * {}.0 - open\nif a{i} > high\n    a{i} := a{i} - high\n\
             float b{i} = a{i} > close ? a{i} - close : close - a{i}\n\
             var float acc{i} = 0.0\nacc{i} += b{i} * 0.001\ntotal += acc{i}\n",
            i + 1
        );
        for line in group.lines() {
            source.push_str(indent);
            source.push_str(line);
            source.push('\n');
        }
    }
    source.push_str("plot(total)\nplot(early)\n");
    source
}

/// Every split chart module of the test fixtures.
#[test]
fn every_split_fixture_hands_over_its_counters_and_cells() {
    let root = Path::new(FIXTURE_ROOT);
    let mut tally = Tally::default();
    let mut split = Vec::new();
    for directory in FIXTURE_DIRECTORIES {
        let mut files = Vec::new();
        collect_pine_files(&root.join(directory), &mut files);
        files.sort();
        for path in files {
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let Some(mut program) = compile(&source, CompileOptions::default()).program else {
                continue;
            };
            let mut id = path.display().to_string();
            if id.ends_with("regression/language-indicator.pine") {
                // This fixture uses table deletion/same-position replacement,
                // so its full output is checked on the hosted tier instead of being
                // counted as a native split module.
                assert!(
                    matches!(supports(&program), Err(AotError::UnsupportedOpcode(reason)) if reason == "table.delete alias invalidation requires interpreter")
                );
                let hosted = compile_program_to_wasm(&program).expect("hosted table fixture");
                assert_eq!(
                    hosted.layout.execution_mode,
                    ExecutionMode::InterpreterHosted
                );
            }
            if id.ends_with("regression/language-strategy.pine") {
                // The color inputs are replaced with fixed defaults, so the
                // strategy stays native and keeps its split coverage.
                assert!(
                    matches!(supports(&program), Err(AotError::UnsupportedOpcode(reason)) if reason == "color input provenance")
                );
                let original = compile_program_to_wasm(&program).expect("interpreter-hosted build");
                assert_eq!(
                    original.layout.execution_mode,
                    ExecutionMode::InterpreterHosted
                );
                let mut fixed = source.clone();
                for (input, default) in [
                    ("strategy_request_input_tint = input.color(color.new(color.blue, 90), \"Tint\", group = \"Text\")", "strategy_request_input_tint = color.new(color.blue, 90)"),
                    ("strategy_request_input_gcol = input(color.red, \"Generic Color\", group = \"Generic\")", "strategy_request_input_gcol = color.red"),
                ] {
                    assert_eq!(fixed.matches(input).count(), 1, "exact fixture replacement {input}");
                    fixed = fixed.replacen(input, default, 1);
                }
                program = compile(&fixed, CompileOptions::default())
                    .program
                    .expect("fixed-color fixture compiles");
                supports(&program).expect("fixed-color handoff fixture remains native");
                id.push_str(" (fixed-color handoff fixture)");
            }
            let Ok(module) = compile_program_to_wasm(&program) else {
                continue;
            };
            if module.layout.execution_mode == ExecutionMode::Native {
                let pieces = tally.pieces;
                check_module(&id, &module.wasm, &mut tally);
                if tally.pieces > pieces {
                    split.push((id, tally.pieces - pieces));
                }
            }
        }
    }
    report("fixtures", &tally);
    // The strategy fixture and generated scripts keep native split coverage;
    // language-indicator is explicitly interpreter-hosted for its table semantics.
    let pieces_of = |name: &str| {
        split
            .iter()
            .find(|(id, _)| id.ends_with(name))
            .map_or(0, |(_, pieces)| *pieces)
    };
    assert!(
        pieces_of("regression/language-strategy.pine (fixed-color handoff fixture)") >= 2,
        "language-strategy must split: {split:?}"
    );
}

/// Generated scripts of three sizes, each split, whose calls hand cached
/// cells on and back and whose pieces store the cells they write.
#[test]
fn every_generated_split_hands_over_its_counters_and_cells() {
    let mut tally = Tally::default();
    for groups in [60, 120, 240] {
        let program = compile(&split_source(groups), CompileOptions::default())
            .program
            .expect("the generated script compiles");
        let module = compile_program_to_wasm(&program).expect("the script compiles to Wasm");
        assert_eq!(
            module.layout.execution_mode,
            ExecutionMode::Native,
            "{groups} groups"
        );
        let modules = tally.modules;
        check_module(&format!("generated:{groups}"), &module.wasm, &mut tally);
        assert_eq!(
            tally.modules,
            modules + 1,
            "{groups} groups: the body must split"
        );
    }
    report("generated", &tally);
    assert!(tally.reloads > 0, "no call hands a written cell back");
    assert!(tally.flushes > 0, "no call hands a cached cell on");
    assert!(tally.exit_stores > 0, "no piece stores a cell it wrote");
}

/// Inspect the actual wrapper operators, including the linked broker
/// indices, rather than inferring broker sequencing from the split plan.
/// Only the body result's zero path reaches the finish call.
fn check_strategy_wrapper(wasm: &[u8]) {
    let mut begin = None;
    let mut finish = None;
    for payload in Parser::new(0).parse_all(pinewasm_aot::runtime::MODULE_WASM_BROKER) {
        if let Payload::ExportSection(section) = payload.unwrap() {
            for export in section {
                let export = export.unwrap();
                match export.name {
                    "pinewasm_rt_broker_begin_bar" => begin = Some(export.index),
                    "pinewasm_rt_broker_finish_bar" => finish = Some(export.index),
                    _ => {}
                }
            }
        }
    }
    let (begin, finish) = (begin.unwrap(), finish.unwrap());
    let mut bodies = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CodeSectionEntry(body) = payload.unwrap() {
            bodies.push(body);
        }
    }
    let step = &bodies[bodies.len() - 2];
    assert_eq!(
        step.get_locals_reader()
            .unwrap()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
        vec![(1, wasmparser::ValType::I32)],
        "the wrapper keeps only its body's status in local 1"
    );
    let ops = step
        .get_operators_reader()
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let at = ops
        .iter()
        .position(|op| matches!(op, Operator::Call { function_index } if *function_index == begin))
        .expect("bar_step calls broker begin");
    assert!(
        matches!(
            &ops[at - 1..],
            [
                Operator::LocalGet { local_index: 0 },
                Operator::Call { function_index: b },
                Operator::If { blockty: BlockType::Empty },
                Operator::I32Const { value: pinewasm_aot::status::BROKER_FAILED },
                Operator::Return,
                Operator::End,
                Operator::LocalGet { local_index: 0 },
                Operator::Call { function_index: piece },
                Operator::LocalTee { local_index: 1 },
                Operator::If { blockty: BlockType::Empty },
                Operator::LocalGet { local_index: 1 },
                Operator::Return,
                Operator::End,
                Operator::LocalGet { local_index: 0 },
                Operator::Call { function_index: f },
                Operator::If { blockty: BlockType::Empty },
                Operator::I32Const { value: pinewasm_aot::status::BROKER_FAILED },
                Operator::Return,
                Operator::End,
                Operator::I32Const { value: pinewasm_aot::status::OK },
                Operator::End,
            ] if *b == begin && *f == finish && *piece > finish
        ),
        "broker begin, complete body, exact-status stop, and finish must remain ordered"
    );
}

/// The standalone route uses the same piece handoff convention, including
/// strategies whose broker surrounds the whole piece tree. Read the
/// handoffs from their actual importless emitted bodies as for charts.
#[test]
fn standalone_indicator_and_strategy_pieces_hand_over_counters_and_cells() {
    let mut tally = Tally::default();
    for strategy in [false, true] {
        let mut source = split_source(120);
        if strategy {
            source = source.replacen(
                "indicator(\"Split handoffs\")",
                "strategy(\"Split handoffs\", process_orders_on_close=true)",
                1,
            );
            source.push_str(
                "if close > open\n    strategy.entry('L', strategy.long)\nelse\n    strategy.close('L')\n",
            );
        }
        let program = compile(&source, CompileOptions::default())
            .program
            .expect("generated standalone script compiles");
        let wasm = compile_standalone_wasm_program(
            &program,
            &StandaloneWasmOptions {
                signal_plot: None,
                module_type: if strategy {
                    StandaloneWasmModuleType::Strategy
                } else {
                    StandaloneWasmModuleType::Indicator
                },
            },
        )
        .expect("generated standalone module emits");
        let before = tally.modules;
        check_module(
            &format!("standalone, strategy={strategy}"),
            &wasm,
            &mut tally,
        );
        if strategy {
            check_strategy_wrapper(&wasm);
        }
        assert_eq!(tally.modules, before + 1, "standalone body must split");
    }
    report("standalone", &tally);
    assert!(tally.reloads > 0, "no standalone cell is handed back");
}
