//! Differential tests of the aot-body structure plan: the label structure
//! comes from the pure [`super::plan`] kernel and is checked against a fused
//! walk in the `old` module below, with `wasm_encoder` emission replaced by
//! recording the same control events. Every test demands identical events,
//! depths and errors. The fixture tests also check, on the repository's own
//! fixture and workload programs, two structural facts: every branch depth
//! resolves, and the plan is well nested.

use super::dom_children;
use super::plan::{self, Frame, PlanError, Step};
use crate::analysis::{analyze, Analysis, Term};
use pinewasm_compiler::{compile, CompileOptions};

/// A fused walk that records the control events it would emit.
mod old {
    use super::{Frame, Term};

    /// The control instructions the fused emitter wrote, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Ev {
        Loop,
        Block,
        End,
        Code(usize),
        Term(usize),
        If,
        Else,
        Br(u32),
    }

    pub(super) struct Walk<'a> {
        pub term: &'a [Term],
        pub rpo: &'a [usize],
        pub loop_header: &'a [bool],
        pub merge: &'a [bool],
        pub dom_children: &'a [Vec<usize>],
        pub frames: Vec<Frame>,
        pub emitted: Vec<bool>,
        pub out: Vec<Ev>,
    }

    impl Walk<'_> {
        fn merge_children(&self, x: usize) -> Vec<usize> {
            self.dom_children[x]
                .iter()
                .copied()
                .filter(|&c| self.merge[c])
                .collect()
        }

        pub(super) fn do_tree(&mut self, x: usize) -> Result<(), String> {
            if self.emitted[x] {
                return Err(format!("block {x} scheduled twice (relooper invariant)"));
            }
            self.emitted[x] = true;
            let ys = self.merge_children(x);
            if self.loop_header[x] {
                self.frames.push(Frame::LoopHeader(x));
                self.out.push(Ev::Loop);
                self.node_within(x, &ys)?;
                self.out.push(Ev::End);
                self.frames.pop();
            } else {
                self.node_within(x, &ys)?;
            }
            Ok(())
        }

        fn node_within(&mut self, x: usize, ys: &[usize]) -> Result<(), String> {
            if let Some((&last, rest)) = ys.split_last() {
                self.frames.push(Frame::BlockEnd(last));
                self.out.push(Ev::Block);
                self.node_within(x, rest)?;
                self.out.push(Ev::End);
                self.frames.pop();
                self.do_tree(last)
            } else {
                self.out.push(Ev::Code(x));
                self.emit_terminator(x)
            }
        }

        fn depth_of(&self, want: Frame) -> Result<u32, String> {
            self.frames
                .iter()
                .rev()
                .position(|f| *f == want)
                .map(|d| d as u32)
                .ok_or_else(|| format!("no frame for {want:?}"))
        }

        fn branch_to(&mut self, target: usize, from: usize) -> Result<(), String> {
            if self.rpo[target] <= self.rpo[from] {
                let d = self.depth_of(Frame::LoopHeader(target))?;
                self.out.push(Ev::Br(d));
                Ok(())
            } else if self.merge[target] {
                let d = self.depth_of(Frame::BlockEnd(target))?;
                self.out.push(Ev::Br(d));
                Ok(())
            } else {
                self.do_tree(target)
            }
        }

        /// The terminator's control part (the instruction/term match and the
        /// flush/test prelude are `Ev::Term`).
        fn emit_terminator(&mut self, x: usize) -> Result<(), String> {
            self.out.push(Ev::Term(x));
            match self.term[x] {
                Term::Return => {
                    let d = self.depth_of(Frame::BarDone)?;
                    self.out.push(Ev::Br(d));
                    Ok(())
                }
                Term::Jump(t) | Term::Fall(t) => self.branch_to(t, x),
                Term::Branch { taken, fall } => self.emit_if_branch(taken, fall, x),
            }
        }

        fn emit_if_branch(&mut self, taken: usize, fall: usize, from: usize) -> Result<(), String> {
            self.frames.push(Frame::Opaque);
            self.out.push(Ev::If);
            self.branch_to(taken, from)?;
            self.out.push(Ev::Else);
            self.branch_to(fall, from)?;
            self.out.push(Ev::End);
            self.frames.pop();
            Ok(())
        }
    }
}

use old::Ev;

fn new_events(steps: &[Step]) -> Vec<Ev> {
    steps
        .iter()
        .map(|step| match *step {
            Step::Loop(_) => Ev::Loop,
            Step::Block(_) => Ev::Block,
            Step::End => Ev::End,
            Step::Code(x) => Ev::Code(x),
            Step::Term(x) => Ev::Term(x),
            Step::If => Ev::If,
            Step::Else => Ev::Else,
            Step::Br(d, _) => Ev::Br(d as u32),
        })
        .collect()
}

fn plan_error_message(error: PlanError) -> String {
    match error {
        PlanError::ScheduledTwice(x) => format!("block {x} scheduled twice (relooper invariant)"),
        PlanError::NoFrame(want) => format!("no frame for {want:?}"),
    }
}

type Outcome = (Vec<Ev>, Result<(), String>);

fn run_old(cfg: &plan::Cfg<'_>, blocks: usize) -> Outcome {
    let mut walk = old::Walk {
        term: cfg.term,
        rpo: cfg.rpo,
        loop_header: cfg.loop_header,
        merge: cfg.merge,
        dom_children: cfg.dom_children,
        frames: vec![Frame::BarDone],
        emitted: vec![false; blocks],
        out: Vec::new(),
    };
    let result = walk.do_tree(0);
    (walk.out, result)
}

fn run_new(cfg: &plan::Cfg<'_>, blocks: usize) -> (Vec<Step>, Outcome) {
    let (steps, result) = plan::plan_structure(cfg, blocks);
    let events = new_events(&steps);
    (steps, (events, result.map_err(plan_error_message)))
}

/// Replay the label effect of `steps` from `[BarDone]`, checking that every
/// `Br` names the frame at its depth, `Else` sits directly in an `if`, `End`
/// never closes `BarDone`, and the walk ends back at `[BarDone]`. These are
/// the well-nestedness and branch-depth properties of a plan, checked on
/// concrete plans.
fn check_well_nested(steps: &[Step]) -> Result<(), String> {
    let mut frames = vec![Frame::BarDone];
    for (i, step) in steps.iter().enumerate() {
        match *step {
            Step::Loop(x) => frames.push(Frame::LoopHeader(x)),
            Step::Block(y) => frames.push(Frame::BlockEnd(y)),
            Step::If => frames.push(Frame::Opaque),
            Step::Else => {
                if frames.last() != Some(&Frame::Opaque) {
                    return Err(format!("step {i}: else outside an if"));
                }
            }
            Step::End => {
                if frames.len() <= 1 {
                    return Err(format!("step {i}: end closes the bar block"));
                }
                frames.pop();
            }
            Step::Br(d, target) => {
                let at = frames.len().checked_sub(d + 1).map(|k| frames[k]);
                if at != Some(target) {
                    return Err(format!("step {i}: br {d} reaches {at:?}, not {target:?}"));
                }
            }
            Step::Code(_) | Step::Term(_) => {}
        }
    }
    if frames == [Frame::BarDone] {
        Ok(())
    } else {
        Err(format!("unclosed frames {frames:?}"))
    }
}

/// Deterministic xorshift, so failures reproduce without a seed file.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

struct RandomCfg {
    term: Vec<Term>,
    rpo: Vec<usize>,
    loop_header: Vec<bool>,
    merge: Vec<bool>,
    dom_children: Vec<Vec<usize>>,
}

fn random_cfg(rng: &mut Rng) -> RandomCfg {
    let n = 1 + rng.below(12);
    let term = (0..n)
        .map(|_| match rng.below(4) {
            0 => Term::Return,
            1 => Term::Jump(rng.below(n)),
            2 => Term::Fall(rng.below(n)),
            _ => Term::Branch {
                taken: rng.below(n),
                fall: rng.below(n),
            },
        })
        .collect();
    let rpo: Vec<usize> = (0..n)
        .map(|b| if rng.chance(80) { b } else { rng.below(n) })
        .collect();
    let loop_header = (0..n).map(|_| rng.chance(25)).collect();
    let merge = (0..n).map(|_| rng.chance(35)).collect();
    let mut dom_children = vec![Vec::new(); n];
    for b in 1..n {
        if rng.chance(85) {
            let parent = rng.below(b);
            dom_children[parent].push(b);
        }
    }
    for children in &mut dom_children {
        children.sort_by_key(|&c| rpo[c]);
    }
    RandomCfg {
        term,
        rpo,
        loop_header,
        merge,
        dom_children,
    }
}

#[test]
fn random_cfgs_plan_is_identical_old_vs_new() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut ok = 0usize;
    let mut errors = 0usize;
    for case in 0..20_000 {
        let g = random_cfg(&mut rng);
        let cfg = plan::Cfg {
            term: &g.term,
            rpo: &g.rpo,
            loop_header: &g.loop_header,
            merge: &g.merge,
            dom_children: &g.dom_children,
        };
        let blocks = g.term.len();
        let old = run_old(&cfg, blocks);
        let (steps, new) = run_new(&cfg, blocks);
        assert_eq!(old, new, "case {case}: plans differ");
        if new.1.is_ok() {
            check_well_nested(&steps).unwrap_or_else(|e| panic!("case {case}: {e}"));
            ok += 1;
        } else {
            errors += 1;
        }
    }
    // Both outcomes must be exercised, so the comparison covers the error
    // precedence as well as successful plans.
    assert!(ok > 1_000 && errors > 1_000, "ok {ok}, errors {errors}");
}

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
    out
}

fn cfg_of<'a>(a: &'a Analysis, dom: &'a [Vec<usize>]) -> plan::Cfg<'a> {
    plan::Cfg {
        term: &a.term,
        rpo: &a.rpo,
        loop_header: &a.loop_header,
        merge: &a.merge,
        dom_children: dom,
    }
}

#[test]
fn corpus_plan_is_identical_old_vs_new_and_well_nested() {
    let mut planned = 0usize;
    let mut loops = 0usize;
    let mut branches = 0usize;
    for (name, source) in corpus() {
        for options in [CompileOptions::default(), CompileOptions::all()] {
            let Some(program) = compile(&source, options).program else {
                continue;
            };
            let Ok(analysis) = analyze(&program) else {
                continue;
            };
            let dom = dom_children(&analysis);
            let cfg = cfg_of(&analysis, &dom);
            let blocks = analysis.blocks.len();
            let old = run_old(&cfg, blocks);
            let (steps, new) = run_new(&cfg, blocks);
            assert_eq!(old, new, "{name}: plans differ");
            assert!(new.1.is_ok(), "{name}: analysed program fails to plan");
            check_well_nested(&steps).unwrap_or_else(|e| panic!("{name}: {e}"));
            loops += steps.iter().filter(|s| matches!(s, Step::Loop(_))).count();
            branches += steps.iter().filter(|s| matches!(s, Step::If)).count();
            planned += 1;
        }
    }
    // Coverage floor, so a fixture set or pipeline change cannot hollow this out.
    // (The analysed fixture set has no loop headers today; loops are covered by
    // the random CFGs above.)
    assert!(
        planned >= 70 && branches > 50,
        "{planned} {loops} {branches}"
    );
}
