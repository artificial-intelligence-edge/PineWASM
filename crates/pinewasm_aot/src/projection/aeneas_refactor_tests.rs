//! Differential tests that compare the index-only [`super::kernel`] with a
//! direct set-based implementation of `FlowFacts::build`,
//! `downstream_is_chart_only` and the `finish` erasure. That implementation is
//! in the `old` module below; every test demands identical results (uses,
//! canonical comparisons, chart-only sites, host sites and errors).

use super::kernel::{self, InstrClass};
use super::{certify, instr_class, FlowFacts, HeapBudget, StandaloneWasmProjection, StringOptions};
use crate::analysis::{analyze, Analysis, Block, Term, VTy};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram};
use pinewasm_hir::{DrawKind, PureFunc};
use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Chart- and string-heavy programs beside the repository fixtures,
/// including an out-of-range `table.cell` case.
const SYNTHETIC: &[(&str, &str)] = &[
    ("label_table_text", "indicator(\"A\")\nlabel.new(bar_index, close, str.tostring(close))\nvar panel = table.new(position.top_right, 1, 1)\ntable.cell(panel, 0, 0, str.tostring(close) + \" px\")\nplot(close > open ? 1 : -1)\n"),
    ("table_cell_out_of_range", "indicator(\"B\")\nvar t = table.new(position.top_right, 1, 1)\ntable.cell(t, 5, 5, \"x\")\nplot(close)\n"),
    ("var_string_label", "indicator(\"C\")\nvar string txt = \"\"\ntxt := txt + \"a\"\nlabel.new(bar_index, high, txt)\nplot(close)\n"),
    ("string_reaches_plot", "indicator(\"D\")\ns = str.tostring(close)\nplot(s == \"1\" ? 1 : 0)\n"),
    ("input_string_options", "indicator(\"E\")\nmode = input.string(\"fast\", options = [\"fast\", \"slow\"])\nplot(mode == \"fast\" ? ta.sma(close, 3) : ta.sma(close, 9))\n"),
    ("input_string_select", "indicator(\"F\")\nmode = input.string(\"a\", options = [\"a\", \"b\", \"c\"])\npick = close > open ? mode : \"b\"\nplot(pick == \"b\" ? 1 : 0)\n"),
    ("table_clear_merge", "indicator(\"G\")\nvar t = table.new(position.bottom_left, 2, 2)\ntable.cell(t, 0, 0, \"h\")\ntable.merge_cells(t, 0, 0, 1, 0)\nif barstate.islast\n    table.clear(t, 0, 0, 1, 1)\nplot(close)\n"),
    ("label_delete", "indicator(\"H\")\nvar label l = na\nlabel.delete(l)\nl := label.new(bar_index, close, \"x\" + str.tostring(bar_index))\nplot(open)\n"),
    ("line_box", "indicator(\"I\")\nline.new(bar_index - 1, open, bar_index, close)\nbox.new(bar_index - 1, high, bar_index, low)\nplot(close - open)\n"),
    ("string_loop", "indicator(\"J\")\nstring acc = \"\"\nfor i = 0 to 3\n    acc := acc + str.tostring(i)\nlabel.new(bar_index, close, acc)\nplot(close)\n"),
    ("color_input", "indicator(\"K\")\nc = input.color(color.red)\nlabel.new(bar_index, close, \"c\", color = c)\nplot(close)\n"),
    ("strategy_label", "strategy(\"L\")\nif close > open\n    strategy.entry(\"long\", strategy.long)\nlabel.new(bar_index, close, str.tostring(strategy.position_size))\n"),
    ("str_cmp_literal", "indicator(\"M\")\na = close > open ? \"up\" : \"down\"\nplot(a == \"up\" ? 1 : 0)\n"),
    ("label_text_branch", "indicator(\"N\")\nt = close > open ? str.tostring(close) : \"n\"\nif bar_index % 2 == 0\n    label.new(bar_index, close, t)\nplot(close)\n"),
    ("migrated_guarded_plot", "//@version=6\nindicator(\"Migration check\")\nplot(close > open ? close : open)\n"),
];

fn corpus() -> Vec<(String, String)> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let mut out = Vec::new();
    for dir in [
        "tests/compile-pass",
        "tests/regression",
        "benchmarks/workloads",
        "tests/compile-fail",
    ] {
        let mut files: Vec<_> = std::fs::read_dir(format!("{root}/{dir}"))
            .expect("fixture directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "pine"))
            .collect();
        files.sort();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("readable fixture");
            out.push((path.display().to_string(), source));
        }
    }
    for (name, source) in SYNTHETIC {
        out.push(((*name).to_string(), (*source).to_string()));
    }
    out
}

fn uses_as_sets(uses: &[Vec<usize>]) -> Vec<BTreeSet<usize>> {
    uses.iter()
        .map(|consumers| {
            assert!(
                consumers.windows(2).all(|pair| pair[0] < pair[1]),
                "use lists stay strictly increasing: {consumers:?}"
            );
            consumers.iter().copied().collect()
        })
        .collect()
}

type FinishOutcome = (
    Result<(), String>,
    BTreeSet<usize>,
    BTreeSet<usize>,
    Vec<bool>,
);

/// `Analysis` is not `Clone`; callers pass a fresh, identical copy. The new
/// side classifies each instruction with [`instr_class`], so the comparison
/// checks the kernel against that rule; the certified classification is
/// tested separately.
fn run_finish(
    program: &CsilProgram,
    projection: &StandaloneWasmProjection,
    mut analysis: Analysis,
    use_old: bool,
) -> Result<FinishOutcome, ()> {
    let mut projection = projection.clone();
    catch_unwind(AssertUnwindSafe(|| {
        let result = if use_old {
            old::finish(&mut projection, program, &mut analysis)
        } else {
            projection.finish_with(
                program,
                &mut analysis,
                program.main().code.iter().map(instr_class).collect(),
            )
        };
        (
            result.map_err(|error| error.to_string()),
            projection.chart_only.clone(),
            projection.canonical_string_cmp.clone(),
            analysis.host_site.clone(),
        )
    }))
    .map_err(|_| ())
}

type FlowOutcome = Result<(Vec<BTreeSet<usize>>, BTreeSet<usize>), String>;

fn run_flow(
    program: &CsilProgram,
    projection: &StandaloneWasmProjection,
    analysis: &Analysis,
    use_old: bool,
) -> Result<FlowOutcome, ()> {
    catch_unwind(AssertUnwindSafe(|| {
        if use_old {
            old::FlowFacts::build(program, analysis, projection)
                .map(|flow| (flow.uses, flow.canonical_string_cmp))
                .map_err(|error| error.to_string())
        } else {
            let classes: Vec<InstrClass> = program.main().code.iter().map(instr_class).collect();
            FlowFacts::build(program, analysis, projection, &classes)
                .map(|flow| (uses_as_sets(&flow.uses), flow.canonical_string_cmp))
                .map_err(|error| error.to_string())
        }
    }))
    .map_err(|_| ())
}

#[test]
fn corpus_projection_is_identical_old_vs_new() {
    let mut compared = 0usize;
    let mut with_chart_only = 0usize;
    let mut with_canonical_cmp = 0usize;
    for (name, source) in corpus() {
        for options in [CompileOptions::default(), CompileOptions::all()] {
            let Some(program) = compile(&source, options).program else {
                continue;
            };
            let Ok((program, projection)) = StandaloneWasmProjection::prepare(&program) else {
                continue;
            };
            let Ok(analysis) = analyze(&program) else {
                continue;
            };
            let old_flow = run_flow(&program, &projection, &analysis, true);
            let new_flow = run_flow(&program, &projection, &analysis, false);
            assert_eq!(old_flow, new_flow, "{name}: FlowFacts differ");
            let old_finish = run_finish(&program, &projection, analysis, true);
            let again = analyze(&program).expect("analysis is deterministic");
            let new_finish = run_finish(&program, &projection, again, false);
            assert_eq!(old_finish, new_finish, "{name}: finish differs");
            if let Ok((_, chart_only, cmp, _)) = &new_finish {
                with_chart_only += usize::from(!chart_only.is_empty());
                with_canonical_cmp += usize::from(!cmp.is_empty());
            }
            compared += 1;
        }
    }
    // Coverage floor, so a fixture or pipeline change cannot hollow this out.
    assert!(compared >= 100, "only {compared} programs compared");
    assert!(
        with_chart_only >= 10,
        "only {with_chart_only} with chart-only sites"
    );
    assert!(
        with_canonical_cmp >= 4,
        "only {with_canonical_cmp} with canonical cmps"
    );
}

/// Deterministic xorshift, so failures reproduce without a seed file.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn flip(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

fn random_instr(rng: &mut Rng, slots: usize) -> CsilInstr {
    let slot = rng.below(slots) as u32;
    match rng.below(18) {
        0 => CsilInstr::Dup,
        1 => CsilInstr::Pop,
        2 => CsilInstr::ReadUserSlot { slot, offset: 0 },
        3 => CsilInstr::ReadUserSlotDynamic { slot },
        4 => CsilInstr::StoreUserSlotCurrent { slot },
        5 => CsilInstr::PushStr(0),
        6 => CsilInstr::PushNa,
        7 => CsilInstr::CmpEq,
        8 => CsilInstr::CmpNe,
        9 => CsilInstr::Select,
        10 => CsilInstr::StrConcat,
        11 => CsilInstr::CallPure {
            func: PureFunc::StrTostring,
            argc: 1,
        },
        12 => CsilInstr::NewObject {
            type_id: 0,
            field_count: 1,
        },
        13 => CsilInstr::SetCell { attrs: Vec::new() },
        14 => CsilInstr::RegisterDrawing {
            kind: DrawKind::Label,
        },
        15 => match rng.below(3) {
            0 => CsilInstr::ClearCells,
            1 => CsilInstr::MergeCells,
            _ => CsilInstr::DeleteDrawing {
                kind: DrawKind::Label,
            },
        },
        16 => CsilInstr::ReadInput {
            input: rng.below(3) as u32,
        },
        _ => CsilInstr::CmpGt,
    }
}

fn random_ty(rng: &mut Rng) -> VTy {
    if rng.below(3) == 0 {
        VTy::F64
    } else {
        VTy::Str
    }
}

/// The typed stack shape `instr` usually has, so random programs are often
/// stack-consistent and reach the canonical-string and `Select` logic.
fn natural_shape(rng: &mut Rng, instr: &CsilInstr) -> (Vec<VTy>, Vec<VTy>) {
    use VTy::{Str, F64};
    match instr {
        CsilInstr::Pop => (vec![random_ty(rng)], vec![]),
        CsilInstr::PushStr(_) | CsilInstr::PushNa | CsilInstr::ReadInput { .. } => {
            (vec![], vec![Str])
        }
        CsilInstr::CmpEq | CsilInstr::CmpNe => (vec![Str, Str], vec![F64]),
        CsilInstr::Select => {
            let extra = if rng.flip() {
                vec![Str, Str]
            } else {
                vec![Str]
            };
            (vec![F64, Str, Str], extra)
        }
        CsilInstr::StrConcat => (vec![Str, Str], vec![Str]),
        CsilInstr::CallPure { .. } => (vec![F64], vec![Str]),
        CsilInstr::NewObject { .. } | CsilInstr::RegisterDrawing { .. } => (vec![F64], vec![F64]),
        CsilInstr::SetCell { .. } => (vec![F64, Str], vec![]),
        CsilInstr::ClearCells | CsilInstr::MergeCells => (vec![F64; 5], vec![]),
        CsilInstr::DeleteDrawing { .. } => (vec![F64], vec![]),
        _ => (vec![F64, F64], vec![F64]),
    }
}

/// A random but in-range program/analysis pair. It need not be valid CSIL:
/// stack underflow, inconsistent depths and unreachable blocks all occur,
/// which exercises every error path of both implementations.
fn random_case(rng: &mut Rng, base: &CsilProgram) -> (CsilProgram, Analysis) {
    let slots = 1 + rng.below(3);
    let n = 1 + rng.below(14);
    let code: Vec<CsilInstr> = (0..n).map(|_| random_instr(rng, slots)).collect();

    let mut cuts: Vec<usize> = (1..n).filter(|_| rng.below(3) == 0).collect();
    cuts.push(n);
    let mut blocks = Vec::new();
    let mut start = 0;
    for end in cuts {
        blocks.push(Block { start, end });
        start = end;
    }
    let nb = blocks.len();
    let term: Vec<Term> = (0..nb)
        .map(|_| match rng.below(4) {
            0 => Term::Fall(rng.below(nb)),
            1 => Term::Jump(rng.below(nb)),
            2 => Term::Branch {
                taken: rng.below(nb),
                fall: rng.below(nb),
            },
            _ => Term::Return,
        })
        .collect();
    let rpo: Vec<usize> = (0..nb)
        .map(|block| {
            if block != 0 && rng.below(5) == 0 {
                usize::MAX
            } else {
                rng.below(nb + 1)
            }
        })
        .collect();
    let mut operand_tys = Vec::with_capacity(n);
    let mut result_tys = Vec::with_capacity(n);
    for instr in &code {
        let (ops, results) = if rng.below(4) == 0 {
            (
                (0..rng.below(4)).map(|_| random_ty(rng)).collect(),
                (0..rng.below(3)).map(|_| random_ty(rng)).collect(),
            )
        } else {
            natural_shape(rng, instr)
        };
        operand_tys.push(ops);
        result_tys.push(results);
    }

    let mut program = base.clone();
    program.functions[0].code = code;
    let analysis = Analysis {
        blocks,
        term,
        rpo,
        idom: Vec::new(),
        loop_header: Vec::new(),
        merge: Vec::new(),
        depth_at: Vec::new(),
        entry_stack: Vec::new(),
        operand_tys,
        result_tys,
        slot_ty: (0..slots).map(|_| random_ty(rng)).collect(),
        host_site: (0..n).map(|_| rng.flip()).collect(),
        alertcondition_pcs: Vec::new(),
        heap: None,
        heap_ops_present: false,
        heap_loop_sites: 0,
        host_loop_sites: 0,
        int_min_guards: std::collections::BTreeMap::new(),
        strategy_broker: None,
        linked_host: false,
        canonical_strings: false,
    };
    (program, analysis)
}

fn random_projection(rng: &mut Rng) -> StandaloneWasmProjection {
    let mut projection = StandaloneWasmProjection::default();
    if rng.flip() {
        projection.string_inputs.insert(
            0,
            StringOptions {
                handles: vec![0],
                default_index: 0,
            },
        );
    }
    if rng.flip() {
        projection.color_inputs.insert(1);
    }
    projection
}

fn base_program() -> CsilProgram {
    compile(
        "indicator(\"base\")\nplot(close)\n",
        CompileOptions::default(),
    )
    .program
    .expect("base program")
}

#[test]
fn random_flow_and_finish_are_identical_old_vs_new() {
    let base = base_program();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok, mut err, mut chart) = (0usize, 0usize, 0usize);
    let (mut refused, mut refused_old_ok, mut refused_old_err, mut refused_old_panic) =
        (0usize, 0usize, 0usize, 0usize);
    for _ in 0..20_000 {
        let replay = Rng(rng.0);
        let (program, analysis) = random_case(&mut rng, &base);
        let (_, twin) = random_case(&mut Rng(replay.0), &base);
        let projection = random_projection(&mut rng);
        let old_flow = run_flow(&program, &projection, &analysis, true);
        let new_flow = run_flow(&program, &projection, &analysis, false);
        let gate = gate_accepts(&program, &projection, &analysis);
        let old_finish = run_finish(&program, &projection, analysis, true);
        let new_finish = run_finish(&program, &projection, twin, false);
        if !gate {
            // The build gate refuses tables outside the kernel's
            // preconditions: the projection fails closed with its own error
            // and changes nothing. Where the set-based copy accepted such
            // tables, or panicked, count it.
            refused += 1;
            let refusal = format!("program failed validation: {GATE_ERROR}");
            assert_eq!(
                new_flow,
                Ok(Err(refusal.clone())),
                "{:?}",
                program.main().code
            );
            let Ok((Err(message), chart_only, cmp, _)) = &new_finish else {
                panic!("gate refusal must be an error: {new_finish:?}");
            };
            assert_eq!(message, &refusal);
            assert!(chart_only.is_empty() && cmp.is_empty());
            match &old_flow {
                Ok(Ok(_)) => refused_old_ok += 1,
                Ok(Err(_)) => refused_old_err += 1,
                Err(()) => refused_old_panic += 1,
            }
            continue;
        }
        assert_eq!(old_flow, new_flow, "{:?}", program.main().code);
        assert_eq!(old_finish, new_finish, "{:?}", program.main().code);
        match new_finish {
            Ok((Ok(()), chart_only, _, _)) => {
                ok += 1;
                chart += usize::from(!chart_only.is_empty());
            }
            _ => err += 1,
        }
    }
    eprintln!(
        "{ok} ok / {err} err / {chart} chart; gate refused {refused} \
         (old: {refused_old_ok} ok, {refused_old_err} err, {refused_old_panic} panic)"
    );
    // With random result types, `Pop` and chart sinks can have results;
    // the gate refuses exactly those tables (terminal instructions with
    // results), about 18%.
    assert!(
        ok > 500 && err > 1_000 && chart > 250 && refused > 1_000,
        "{ok} ok / {err} err / {chart} chart / {refused} refused"
    );
}

const GATE_ERROR: &str = "standalone WebAssembly projection tables are not well formed";

/// Whether `FlowFacts::build`'s gate accepts the tables of this case.
fn gate_accepts(
    program: &CsilProgram,
    projection: &StandaloneWasmProjection,
    analysis: &Analysis,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let (input, slot_canonical, cmp_candidate) =
            super::flow_tables(program, analysis, projection);
        let code = &program.main().code;
        let classes: Vec<InstrClass> = code.iter().map(instr_class).collect();
        super::wf::kernel_input_ok(
            &input,
            code.len(),
            &slot_canonical,
            &cmp_candidate,
            &classes,
        )
    }))
    .unwrap_or(false)
}

/// Random use graphs (cycles, self-loops, fan-out, empty use lists) through
/// the direct set-based proof and the `Vec<bool>`-visiting kernel.
#[test]
fn random_use_graphs_chart_only_is_identical_old_vs_new() {
    let base = base_program();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut yes, mut no) = (0usize, 0usize);
    for _ in 0..20_000 {
        let n = 1 + rng.below(10);
        let mut program = base.clone();
        program.functions[0].code = (0..n).map(|_| random_instr(&mut rng, 1)).collect();
        let uses: Vec<BTreeSet<usize>> = (0..n)
            .map(|_| (0..rng.below(4)).map(|_| rng.below(n)).collect())
            .collect();
        let uses_vec: Vec<Vec<usize>> = uses.iter().map(|s| s.iter().copied().collect()).collect();
        let classes: Vec<InstrClass> = program.main().code.iter().map(instr_class).collect();
        for pc in 0..n {
            let mut old_visiting = BTreeSet::new();
            let old = old::downstream_is_chart_only(pc, &uses, &program, &mut old_visiting);
            let mut visiting = vec![false; n];
            let new = kernel::downstream_is_chart_only(pc, &uses_vec, &classes, &mut visiting);
            assert_eq!(
                old,
                new,
                "pc {pc} uses {uses:?} code {:?}",
                program.main().code
            );
            let marked: BTreeSet<usize> = (0..n).filter(|&i| visiting[i]).collect();
            assert_eq!(old_visiting, marked, "visiting sets agree after the proof");
            if new {
                yes += 1;
            } else {
                no += 1;
            }
        }
    }
    assert!(yes > 1_000 && no > 1_000, "{yes} yes / {no} no");
}

#[test]
fn instr_class_matches_the_old_matches() {
    // Every class the set-based code distinguished, and one instruction it did
    // not.
    let cases = [
        (CsilInstr::Pop, InstrClass::Pop),
        (
            CsilInstr::SetCell { attrs: Vec::new() },
            InstrClass::ChartSink,
        ),
        (CsilInstr::ClearCells, InstrClass::ChartSink),
        (CsilInstr::MergeCells, InstrClass::ChartSink),
        (
            CsilInstr::DeleteDrawing {
                kind: DrawKind::Table,
            },
            InstrClass::ChartSink,
        ),
        (
            CsilInstr::CallPure {
                func: PureFunc::StrTostring,
                argc: 2,
            },
            InstrClass::ChartProducer,
        ),
        (CsilInstr::StrConcat, InstrClass::ChartProducer),
        (
            CsilInstr::NewObject {
                type_id: 3,
                field_count: 0,
            },
            InstrClass::ChartProducer,
        ),
        (
            CsilInstr::RegisterDrawing {
                kind: DrawKind::Box,
            },
            InstrClass::RegisterDrawing,
        ),
        (
            CsilInstr::CallPure {
                func: PureFunc::StrLength,
                argc: 1,
            },
            InstrClass::Other,
        ),
        (CsilInstr::CmpGt, InstrClass::Other),
    ];
    for (instr, class) in cases {
        assert_eq!(instr_class(&instr), class, "{instr:?}");
    }
}

#[test]
fn sorted_set_helpers_match_btreeset() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for _ in 0..5_000 {
        let a: BTreeSet<usize> = (0..rng.below(8)).map(|_| rng.below(12)).collect();
        let b: BTreeSet<usize> = (0..rng.below(8)).map(|_| rng.below(12)).collect();
        let av: Vec<usize> = a.iter().copied().collect();
        let bv: Vec<usize> = b.iter().copied().collect();
        let union: Vec<usize> = a.union(&b).copied().collect();
        assert_eq!(kernel::sorted_union(&av, &bv), union);
        let mut inserted = av.clone();
        let x = rng.below(12);
        kernel::sorted_insert(&mut inserted, x);
        let mut expected = a.clone();
        expected.insert(x);
        assert_eq!(inserted, expected.into_iter().collect::<Vec<_>>());
    }
}

/// An out-of-range `table.cell` is erased by `finish_with` under the
/// `instr_class` classification, so the standalone run would complete where
/// the VM fails. `finish` keeps it as a host site, so standalone emission
/// fails closed instead.
#[test]
fn out_of_range_table_cell_is_kept_by_the_corrected_rule() {
    let source = SYNTHETIC
        .iter()
        .find(|(name, _)| *name == "table_cell_out_of_range")
        .expect("corpus entry")
        .1;
    let program = compile(source, CompileOptions::default())
        .program
        .expect("compiles");
    let (program, projection) = StandaloneWasmProjection::prepare(&program).expect("prepare");
    let set_cell = program
        .main()
        .code
        .iter()
        .position(|instr| matches!(instr, CsilInstr::SetCell { .. }))
        .expect("a SetCell site");

    let mut pinned = projection.clone();
    let mut analysis = analyze(&program).expect("analyze");
    pinned
        .finish_with(
            &program,
            &mut analysis,
            program.main().code.iter().map(instr_class).collect(),
        )
        .expect("finish");
    assert!(pinned.chart_only.contains(&set_cell));
    assert!(!analysis.host_site[set_cell]);

    let mut corrected = projection;
    let mut analysis = analyze(&program).expect("analyze");
    corrected
        .finish(&program, &mut analysis, HeapBudget::default())
        .expect("finish");
    assert!(!corrected.chart_only.contains(&set_cell));
    assert!(analysis.host_site[set_cell]);
    // The table's constructor is certified, but its `SetCell` use is real, so
    // `NewObject` is not erased and stays a host site: only the certified
    // registry effect after it may be erased, and emission still fails
    // closed on the constructor.
    let code = &program.main().code;
    let table_new = code
        .iter()
        .position(|instr| matches!(instr, CsilInstr::NewObject { .. }))
        .expect("a NewObject site");
    assert!(!corrected.chart_only.contains(&table_new));
    assert!(analysis.host_site[table_new]);
    assert_eq!(corrected.max_bars, None);
    assert!(corrected
        .chart_only
        .iter()
        .all(|&pc| matches!(code[pc], CsilInstr::RegisterDrawing { .. })));
}

/// `finish` erases only certified drawing-lifecycle sites on the fixture
/// programs: `DeleteDrawing`, and constructor pairs (`NewObject` then
/// `RegisterDrawing`) under a request bound. Every string producer, table
/// sink and other chart producer keeps its host site, and no `NewObject` is
/// erased without a bar bound.
#[test]
fn corrected_rule_erases_only_certified_drawing_sites_on_the_corpus() {
    let mut finished = 0usize;
    let mut erased_programs = 0usize;
    for (name, source) in corpus() {
        let Some(program) = compile(&source, CompileOptions::default()).program else {
            continue;
        };
        let Ok((program, mut projection)) = StandaloneWasmProjection::prepare(&program) else {
            continue;
        };
        let Ok(mut analysis) = analyze(&program) else {
            continue;
        };
        if projection
            .finish(&program, &mut analysis, HeapBudget::default())
            .is_ok()
        {
            let code = &program.main().code;
            for &pc in &projection.chart_only {
                match code
                    .get(pc)
                    .expect("erased instruction must belong to the source program")
                {
                    CsilInstr::DeleteDrawing { .. } => {}
                    CsilInstr::NewObject { .. } => {
                        assert!(projection.max_bars.is_some(), "{name}: pc {pc}");
                        assert!(
                            matches!(code[pc + 1], CsilInstr::RegisterDrawing { .. })
                                && projection.chart_only.contains(&(pc + 1)),
                            "{name}: pc {pc}"
                        );
                    }
                    CsilInstr::RegisterDrawing { .. } => {
                        assert!(
                            matches!(code[pc - 1], CsilInstr::NewObject { .. }),
                            "{name}: pc {pc}"
                        );
                    }
                    other => panic!("{name}: erased uncertified {other:?} at pc {pc}"),
                }
            }
            erased_programs += usize::from(!projection.chart_only.is_empty());
            finished += 1;
        }
    }
    assert!(finished >= 50, "only {finished} programs finished");
    assert!(
        erased_programs > 0,
        "no corpus program erased a drawing site"
    );
}

/// Instruction-local certificates: `Pop` keeps its class; string producers
/// and table sinks never have one, whatever the program.
#[test]
fn string_and_table_sites_are_never_certified() {
    let source = SYNTHETIC
        .iter()
        .find(|(name, _)| *name == "label_table_text")
        .expect("corpus entry")
        .1;
    let program = compile(source, CompileOptions::default())
        .program
        .expect("compiles");
    let (program, _) = StandaloneWasmProjection::prepare(&program).expect("prepare");
    let analysis = analyze(&program).expect("analyze");
    let certified = certify(&program, &analysis, HeapBudget::default());
    let mut seen = 0;
    for (pc, instr) in program.main().code.iter().enumerate() {
        let expected = match instr {
            CsilInstr::Pop => Some(InstrClass::Pop),
            CsilInstr::StrConcat
            | CsilInstr::SetCell { .. }
            | CsilInstr::ClearCells
            | CsilInstr::MergeCells
            | CsilInstr::CallPure {
                func: PureFunc::StrTostring,
                ..
            } => Some(InstrClass::Other),
            _ => None,
        };
        if let Some(expected) = expected {
            assert_eq!(certified.classes[pc], expected, "{instr:?} at pc {pc}");
            seen += 1;
        }
    }
    assert!(seen >= 4, "only {seen} string/table/pop sites");
}

/// Negative: a string that reaches a plot is not chart-only.
#[test]
fn string_reaching_a_plot_is_not_erased() {
    let source = SYNTHETIC
        .iter()
        .find(|(name, _)| *name == "string_reaches_plot")
        .expect("corpus entry")
        .1;
    let program = compile(source, CompileOptions::default())
        .program
        .expect("compiles");
    let (program, mut projection) = StandaloneWasmProjection::prepare(&program).expect("prepare");
    let mut analysis = analyze(&program).expect("analyze");
    projection
        .finish(&program, &mut analysis, HeapBudget::default())
        .expect("finish");
    let tostring = program
        .main()
        .code
        .iter()
        .position(|instr| {
            matches!(
                instr,
                CsilInstr::CallPure {
                    func: PureFunc::StrTostring,
                    ..
                }
            )
        })
        .expect("a str.tostring site");
    assert!(!projection.chart_only.contains(&tostring));
}

mod old {
    //! Direct set-based copies of `finish`, `FlowFacts::build` and
    //! `downstream_is_chart_only`, for in-test comparison only.
    #![allow(clippy::all)]

    use crate::analysis::{Analysis, Term, VTy};
    use crate::projection::StandaloneWasmProjection;
    use crate::AotError;
    use pinewasm_csil::{CsilInstr, CsilProgram};
    use pinewasm_hir::PureFunc;
    use std::collections::BTreeSet;

    pub(super) fn finish(
        this: &mut StandaloneWasmProjection,
        program: &CsilProgram,
        analysis: &mut Analysis,
    ) -> Result<(), AotError> {
        let flow = FlowFacts::build(program, analysis, this)?;
        this.canonical_string_cmp = flow.canonical_string_cmp;

        for pc in 0..program.main().code.len() {
            let candidate = matches!(
                program.main().code[pc],
                CsilInstr::CallPure {
                    func: PureFunc::StrTostring,
                    ..
                } | CsilInstr::StrConcat
                    | CsilInstr::NewObject { .. }
            );
            if candidate && downstream_is_chart_only(pc, &flow.uses, program, &mut BTreeSet::new())
            {
                this.chart_only.insert(pc);
            }
        }

        // These operations are chart-output effects.  RegisterDrawing is an
        // identity in value terms; the cell/table operations have no result.
        // Their producers are erased only when the separate use proof above
        // says every downstream use is chart-only.
        for (pc, instr) in program.main().code.iter().enumerate() {
            if matches!(
                instr,
                CsilInstr::RegisterDrawing { .. }
                    | CsilInstr::SetCell { .. }
                    | CsilInstr::ClearCells
                    | CsilInstr::MergeCells
                    | CsilInstr::DeleteDrawing { .. }
            ) {
                this.chart_only.insert(pc);
            }
        }

        for (pc, instr) in program.main().code.iter().enumerate() {
            let projected_input = match instr {
                CsilInstr::ReadInput { input } => {
                    this.string_inputs.contains_key(input) || this.color_inputs.contains(input)
                }
                _ => false,
            };
            if projected_input
                || this.canonical_string_cmp.contains(&pc)
                || this.chart_only.contains(&pc)
            {
                analysis.host_site[pc] = false;
            }
        }
        Ok(())
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct ValueFact {
        producers: BTreeSet<usize>,
        canonical_string: bool,
    }

    impl ValueFact {
        fn producer(pc: usize, canonical_string: bool) -> Self {
            Self {
                producers: BTreeSet::from([pc]),
                canonical_string,
            }
        }

        fn merge(&mut self, other: &Self) -> bool {
            let old_len = self.producers.len();
            self.producers.extend(other.producers.iter().copied());
            let old_canonical = self.canonical_string;
            self.canonical_string &= other.canonical_string;
            self.producers.len() != old_len || self.canonical_string != old_canonical
        }
    }

    pub(super) struct FlowFacts {
        pub(super) uses: Vec<BTreeSet<usize>>,
        pub(super) canonical_string_cmp: BTreeSet<usize>,
    }

    impl FlowFacts {
        /// A small forward data-flow proof over the validated stack machine.
        /// Slot reads/writes and `dup` are transparent, so a producer's recorded
        /// consumers are its real semantic uses rather than storage plumbing.
        pub(super) fn build(
            program: &CsilProgram,
            analysis: &Analysis,
            projection: &StandaloneWasmProjection,
        ) -> Result<Self, AotError> {
            let code = &program.main().code;
            let mut entry: Vec<Option<Vec<ValueFact>>> = vec![None; analysis.blocks.len()];
            entry[0] = Some(Vec::new());
            let mut slots: Vec<ValueFact> = analysis
                .slot_ty
                .iter()
                .map(|ty| ValueFact {
                    producers: BTreeSet::new(),
                    // Every series slot begins as na.  It stays canonical exactly
                    // while every value ever stored into a string slot is too.
                    canonical_string: *ty == VTy::Str,
                })
                .collect();
            let mut uses = vec![BTreeSet::new(); code.len()];
            let mut cmp_ok: Vec<bool> = code
                .iter()
                .enumerate()
                .map(|(pc, instr)| {
                    matches!(instr, CsilInstr::CmpEq | CsilInstr::CmpNe)
                        && analysis.operand_tys[pc].first() == Some(&VTy::Str)
                })
                .collect();
            let mut rpo_order: Vec<usize> = (0..analysis.blocks.len())
                .filter(|&block| analysis.rpo[block] != usize::MAX)
                .collect();
            rpo_order.sort_by_key(|&block| analysis.rpo[block]);

            let cap = analysis.blocks.len() + code.len() + program.series_slots.len() + 4;
            for _ in 0..cap {
                let mut changed = false;
                for &block_id in &rpo_order {
                    let Some(mut stack) = entry[block_id].clone() else {
                        continue;
                    };
                    let block = &analysis.blocks[block_id];
                    for pc in block.start..block.end {
                        let instr = &code[pc];
                        match instr {
                            CsilInstr::Dup => {
                                let value = stack.pop().ok_or_else(|| stack_underflow(pc))?;
                                stack.push(value.clone());
                                stack.push(value);
                                continue;
                            }
                            CsilInstr::ReadUserSlot { slot, .. }
                            | CsilInstr::ReadUserSlotDynamic { slot } => {
                                if matches!(instr, CsilInstr::ReadUserSlotDynamic { .. }) {
                                    let offset = stack.pop().ok_or_else(|| stack_underflow(pc))?;
                                    record_uses(&mut uses, &offset, pc);
                                }
                                stack.push(slots[*slot as usize].clone());
                                continue;
                            }
                            CsilInstr::StoreUserSlotCurrent { slot } => {
                                let value = stack.pop().ok_or_else(|| stack_underflow(pc))?;
                                changed |= slots[*slot as usize].merge(&value);
                                continue;
                            }
                            _ => {}
                        }

                        let pops = analysis.operand_tys[pc].len();
                        if stack.len() < pops {
                            return Err(stack_underflow(pc));
                        }
                        let base = stack.len() - pops;
                        let args = stack.split_off(base);
                        for arg in &args {
                            record_uses(&mut uses, arg, pc);
                        }

                        if cmp_ok[pc] {
                            cmp_ok[pc] &=
                                args.len() == 2 && args.iter().all(|value| value.canonical_string);
                        }

                        for (result_index, ty) in analysis.result_tys[pc].iter().enumerate() {
                            let canonical = if *ty != VTy::Str {
                                false
                            } else {
                                match instr {
                                    CsilInstr::PushStr(_) => true,
                                    CsilInstr::PushNa => true,
                                    CsilInstr::ReadInput { input } => {
                                        projection.string_inputs.contains_key(input)
                                    }
                                    CsilInstr::Select if result_index == 0 => {
                                        args.get(1).is_some_and(|v| v.canonical_string)
                                            && args.get(2).is_some_and(|v| v.canonical_string)
                                    }
                                    _ => false,
                                }
                            };
                            stack.push(ValueFact::producer(pc, canonical));
                        }
                    }

                    let term = analysis.term.get(block_id).copied().ok_or_else(|| {
                        AotError::InvalidProgram(format!(
                            "standalone WebAssembly projection has no terminator for block {block_id}"
                        ))
                    })?;
                    match term {
                        Term::Branch { taken, fall } => {
                            changed |= merge_entry(&mut entry[taken], &stack)?;
                            changed |= merge_entry(&mut entry[fall], &stack)?;
                        }
                        Term::Fall(next) | Term::Jump(next) => {
                            changed |= merge_entry(&mut entry[next], &stack)?;
                        }
                        Term::Return => {}
                    }
                }
                if !changed {
                    return Ok(Self {
                        uses,
                        canonical_string_cmp: cmp_ok
                            .iter()
                            .enumerate()
                            .filter_map(|(pc, ok)| ok.then_some(pc))
                            .collect(),
                    });
                }
            }
            Err(AotError::InvalidProgram(
                "standalone WebAssembly projection data-flow did not converge".to_string(),
            ))
        }
    }

    fn merge_entry(dst: &mut Option<Vec<ValueFact>>, src: &[ValueFact]) -> Result<bool, AotError> {
        let Some(current) = dst else {
            *dst = Some(src.to_vec());
            return Ok(true);
        };
        if current.len() != src.len() {
            return Err(AotError::InvalidProgram(
                "standalone WebAssembly projection saw inconsistent stack depth".to_string(),
            ));
        }
        Ok(current
            .iter_mut()
            .zip(src)
            .fold(false, |changed, (left, right)| left.merge(right) || changed))
    }

    fn record_uses(uses: &mut [BTreeSet<usize>], value: &ValueFact, consumer: usize) {
        for &producer in &value.producers {
            uses[producer].insert(consumer);
        }
    }

    fn stack_underflow(pc: usize) -> AotError {
        AotError::InvalidProgram(format!(
            "standalone WebAssembly projection stack underflow at instruction {pc}"
        ))
    }

    /// True only when every use chain terminates in a discarded chart effect.
    /// Cycles fail closed; they would need a stronger proof than this local slice.
    pub(super) fn downstream_is_chart_only(
        pc: usize,
        uses: &[BTreeSet<usize>],
        program: &CsilProgram,
        visiting: &mut BTreeSet<usize>,
    ) -> bool {
        if !visiting.insert(pc) || uses[pc].is_empty() {
            return false;
        }
        let ok = uses[pc].iter().all(|&consumer| {
            let instr = &program.main().code[consumer];
            if matches!(
                instr,
                CsilInstr::Pop
                    | CsilInstr::SetCell { .. }
                    | CsilInstr::ClearCells
                    | CsilInstr::MergeCells
                    | CsilInstr::DeleteDrawing { .. }
            ) {
                return true;
            }
            if matches!(
                instr,
                CsilInstr::CallPure {
                    func: PureFunc::StrTostring,
                    ..
                } | CsilInstr::StrConcat
                    | CsilInstr::NewObject { .. }
                    | CsilInstr::RegisterDrawing { .. }
            ) {
                return downstream_is_chart_only(consumer, uses, program, visiting);
            }
            false
        });
        visiting.remove(&pc);
        ok
    }
}

/// The adapter's tables satisfy the kernel's preconditions (well-formed
/// indices, at least one block, one comparison flag and one class per pc,
/// and no results for terminal instructions) on every fixture program.
/// `flow_input_wf` and `kernel_input_ok` check exactly these conditions.
#[test]
fn corpus_flow_tables_satisfy_kernel_preconditions() {
    let mut checked = 0usize;
    for (name, source) in corpus() {
        for options in [CompileOptions::default(), CompileOptions::all()] {
            let Some(program) = compile(&source, options).program else {
                continue;
            };
            let Ok((program, projection)) = StandaloneWasmProjection::prepare(&program) else {
                continue;
            };
            let Ok(analysis) = analyze(&program) else {
                continue;
            };
            let (input, slot_canonical, cmp_candidate) =
                super::flow_tables(&program, &analysis, &projection);
            let code = &program.main().code;
            let classes: Vec<InstrClass> = code.iter().map(instr_class).collect();
            assert!(
                super::wf::flow_input_wf(&input, code.len(), slot_canonical.len(), &classes),
                "{name}: kernel preconditions fail"
            );
            assert!(!input.blocks.is_empty(), "{name}: no blocks");
            assert_eq!(cmp_candidate.len(), code.len(), "{name}: comparison flags");
            assert!(
                super::wf::kernel_input_ok(
                    &input,
                    code.len(),
                    &slot_canonical,
                    &cmp_candidate,
                    &classes
                ),
                "{name}: the build gate refuses the tables"
            );
            checked += 1;
        }
    }
    assert!(checked >= 100, "only {checked} programs checked");
}

/// `flow_input_wf` rejects a table that violates the kernel's preconditions.
#[test]
fn flow_input_wf_rejects_bad_tables() {
    let input =
        |end: usize, term: Term, results: Vec<Vec<kernel::ResultCanon>>| kernel::FlowInput {
            ops: vec![kernel::FlowOp::Generic, kernel::FlowOp::Generic],
            pops: vec![0, 1],
            results,
            blocks: vec![Block { start: 0, end }],
            terms: vec![term],
            rpo_order: vec![0],
        };
    let classes = [InstrClass::Other, InstrClass::Pop];
    let good = input(2, Term::Return, vec![vec![kernel::ResultCanon::No], vec![]]);
    assert!(super::wf::flow_input_wf(&good, 2, 0, &classes));
    let late_end = input(3, Term::Return, vec![vec![kernel::ResultCanon::No], vec![]]);
    assert!(!super::wf::flow_input_wf(&late_end, 2, 0, &classes));
    let bad_target = input(
        2,
        Term::Jump(1),
        vec![vec![kernel::ResultCanon::No], vec![]],
    );
    assert!(!super::wf::flow_input_wf(&bad_target, 2, 0, &classes));
    let pop_result = input(2, Term::Return, vec![vec![], vec![kernel::ResultCanon::No]]);
    assert!(!super::wf::flow_input_wf(&pop_result, 2, 0, &classes));
    // The build gate adds: at least one block, one flag and one class per pc.
    let cmp = [false, false];
    assert!(super::wf::kernel_input_ok(&good, 2, &[], &cmp, &classes));
    let mut no_blocks = input(2, Term::Return, vec![vec![kernel::ResultCanon::No], vec![]]);
    no_blocks.blocks.clear();
    no_blocks.terms.clear();
    no_blocks.rpo_order.clear();
    assert!(super::wf::flow_input_wf(&no_blocks, 2, 0, &classes));
    assert!(!super::wf::kernel_input_ok(
        &no_blocks,
        2,
        &[],
        &cmp,
        &classes
    ));
    assert!(!super::wf::kernel_input_ok(
        &good,
        2,
        &[],
        &cmp[..1],
        &classes
    ));
    assert!(!super::wf::kernel_input_ok(
        &good,
        2,
        &[],
        &cmp,
        &classes[..1]
    ));
    assert!(!super::wf::kernel_input_ok(
        &late_end,
        2,
        &[],
        &cmp,
        &classes
    ));
}

/// The graph model reads a program with no registry-reading host site left.
/// `ReadDrawingsAll` is class `Other` (never erased), and a standalone
/// WebAssembly build of a program that still reads the drawing registry
/// refuses to import `host.vm`.
#[test]
fn registry_reads_are_never_erased_and_refused_standalone() {
    for kind in [
        DrawKind::Line,
        DrawKind::Label,
        DrawKind::Box,
        DrawKind::Table,
        DrawKind::Polyline,
        DrawKind::Linefill,
    ] {
        assert_eq!(
            instr_class(&CsilInstr::ReadDrawingsAll { kind }),
            InstrClass::Other
        );
    }
    let source = "indicator(\"R\")\nline.new(bar_index - 1, open, bar_index, close)\nplot(array.size(line.all))\n";
    let program = compile(source, CompileOptions::default())
        .program
        .expect("program");
    assert!(program
        .main()
        .code
        .iter()
        .any(|instr| matches!(instr, CsilInstr::ReadDrawingsAll { .. })));
    let result = crate::compile_standalone_wasm_program(
        &program,
        &crate::StandaloneWasmOptions {
            signal_plot: None,
            module_type: crate::StandaloneWasmModuleType::Strategy,
        },
    );
    assert!(
        matches!(result, Err(crate::AotError::StandaloneWasmNotStandalone(_))),
        "{result:?}"
    );
}
