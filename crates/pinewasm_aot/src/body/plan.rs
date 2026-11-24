//! The structure plan: which wasm `block`/`loop`/`if` labels the body opens,
//! in which order, and which label depth every structural branch uses.
//!
//! This is the dominator-tree walk of Ramsey's "Beyond Relooper" (see the
//! parent module), separated from `wasm_encoder` and from the virtual operand
//! stack. [`plan_structure`] is a pure function of the analysed CFG facts. It
//! returns the steps in emission order; `BodyEmitter::emit` replays them,
//! emitting the block code, the terminator tests and the control
//! instructions. A planning error is returned next to the steps planned
//! before it, so the replay stops at exactly the step where planning failed.
//!
//! The kernel is written in an Aeneas-extractable style: free functions,
//! explicit loops, no closures or iterator adaptors.

use crate::analysis::Term;

/// A wasm label the structured body has open, innermost last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Frame {
    /// The per-bar body wrapper; `RETURN` branches here.
    BarDone,
    /// A wasm `block` whose END is the start of merge node `usize`.
    BlockEnd(usize),
    /// A wasm `loop` headed at node `usize` (back edges branch here).
    LoopHeader(usize),
    /// An `if`/`else` label opened by a conditional terminator.
    Opaque,
}

/// One structural emission step, in emission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// `loop` (empty type), opening `Frame::LoopHeader(x)`.
    Loop(usize),
    /// `block` (empty type), opening `Frame::BlockEnd(y)`.
    Block(usize),
    /// `end` of the innermost open label.
    End,
    /// The instructions of block `x`, except its terminator's branch.
    Code(usize),
    /// The terminator prelude of block `x`: stack flushes and, for a
    /// two-way branch, the i32 test. Its branch steps follow.
    Term(usize),
    /// `if` (empty type), opening `Frame::Opaque`.
    If,
    /// `else` of the innermost `if`.
    Else,
    /// `br depth`; `depth` counts open labels from the innermost (0) and
    /// names the frame recorded next to it.
    Br(usize, Frame),
}

/// A violated relooper invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanError {
    /// Block `usize` was reached twice in the dominator-tree walk.
    ScheduledTwice(usize),
    /// A branch needed a label that is not open.
    NoFrame(Frame),
}

/// The analysed CFG facts the plan depends on, indexed by block.
pub(crate) struct Cfg<'a> {
    pub term: &'a [Term],
    /// Reverse-postorder index per block.
    pub rpo: &'a [usize],
    pub loop_header: &'a [bool],
    pub merge: &'a [bool],
    /// Dominator-tree children per block, sorted by `rpo`.
    pub dom_children: &'a [Vec<usize>],
}

/// Planner state: the open labels (innermost last), the blocks already
/// scheduled, and the steps so far.
pub(crate) struct Planner {
    pub frames: Vec<Frame>,
    pub emitted: Vec<bool>,
    pub steps: Vec<Step>,
}

pub(crate) fn frame_eq(a: Frame, b: Frame) -> bool {
    match a {
        Frame::BarDone => matches!(b, Frame::BarDone),
        Frame::Opaque => matches!(b, Frame::Opaque),
        Frame::BlockEnd(x) => match b {
            Frame::BlockEnd(y) => x == y,
            _ => false,
        },
        Frame::LoopHeader(x) => match b {
            Frame::LoopHeader(y) => x == y,
            _ => false,
        },
    }
}

/// Label depth of the innermost open frame equal to `want` (0 = innermost).
pub(crate) fn depth_of(frames: &[Frame], want: Frame) -> Option<usize> {
    let mut i = frames.len();
    let mut d = 0;
    while i > 0 {
        i -= 1;
        if frame_eq(frames[i], want) {
            return Some(d);
        }
        d += 1;
    }
    None
}

/// `x`'s dominator-tree children that are merge nodes, in `rpo` order.
pub(crate) fn merge_children(cfg: &Cfg<'_>, x: usize) -> Vec<usize> {
    let mut ys = Vec::new();
    push_merge_children(cfg.merge, &cfg.dom_children[x], 0, &mut ys);
    ys
}

/// Append the merge nodes among `children[i..]` to `ys`, in order.
fn push_merge_children(merge: &[bool], children: &[usize], i: usize, ys: &mut Vec<usize>) {
    if i < children.len() {
        let c = children[i];
        if merge[c] {
            ys.push(c);
        }
        push_merge_children(merge, children, i + 1, ys);
    }
}

fn br_frame(p: &mut Planner, want: Frame) -> Result<(), PlanError> {
    match depth_of(&p.frames, want) {
        Some(d) => {
            p.steps.push(Step::Br(d, want));
            Ok(())
        }
        None => Err(PlanError::NoFrame(want)),
    }
}

/// Schedule block `x` and everything it dominates.
fn do_tree(cfg: &Cfg<'_>, p: &mut Planner, x: usize) -> Result<(), PlanError> {
    if p.emitted[x] {
        return Err(PlanError::ScheduledTwice(x));
    }
    p.emitted[x] = true;
    let ys = merge_children(cfg, x);
    if cfg.loop_header[x] {
        p.frames.push(Frame::LoopHeader(x));
        p.steps.push(Step::Loop(x));
        node_within(cfg, p, x, &ys, ys.len())?;
        p.steps.push(Step::End);
        p.frames.pop();
        Ok(())
    } else {
        node_within(cfg, p, x, &ys, ys.len())
    }
}

/// `ys[..n]` are x's merge children sorted by rpo ascending; the OUTERMOST
/// wasm block ends at the LAST of them.
fn node_within(
    cfg: &Cfg<'_>,
    p: &mut Planner,
    x: usize,
    ys: &[usize],
    n: usize,
) -> Result<(), PlanError> {
    if n == 0 {
        p.steps.push(Step::Code(x));
        terminator(cfg, p, x)
    } else {
        let last = ys[n - 1];
        p.frames.push(Frame::BlockEnd(last));
        p.steps.push(Step::Block(last));
        node_within(cfg, p, x, ys, n - 1)?;
        p.steps.push(Step::End);
        p.frames.pop();
        do_tree(cfg, p, last)
    }
}

fn terminator(cfg: &Cfg<'_>, p: &mut Planner, x: usize) -> Result<(), PlanError> {
    p.steps.push(Step::Term(x));
    let term = cfg.term[x];
    match term {
        Term::Return => br_frame(p, Frame::BarDone),
        Term::Jump(t) => branch_to(cfg, p, t, x),
        Term::Fall(t) => branch_to(cfg, p, t, x),
        Term::Branch { taken, fall } => {
            p.frames.push(Frame::Opaque);
            p.steps.push(Step::If);
            branch_to(cfg, p, taken, x)?;
            p.steps.push(Step::Else);
            branch_to(cfg, p, fall, x)?;
            p.steps.push(Step::End);
            p.frames.pop();
            Ok(())
        }
    }
}

fn branch_to(cfg: &Cfg<'_>, p: &mut Planner, target: usize, from: usize) -> Result<(), PlanError> {
    if cfg.rpo[target] <= cfg.rpo[from] {
        br_frame(p, Frame::LoopHeader(target))
    } else if cfg.merge[target] {
        br_frame(p, Frame::BlockEnd(target))
    } else {
        do_tree(cfg, p, target)
    }
}

/// Plan the whole body: block 0 and everything it dominates, inside the
/// caller's `block $bar_done` (`Frame::BarDone`). `blocks` is the number of
/// analysed blocks.
// `vec![Frame::BarDone]` goes through a boxed slice, which Aeneas does not model.
#[allow(clippy::vec_init_then_push)]
pub(crate) fn plan_structure(cfg: &Cfg<'_>, blocks: usize) -> (Vec<Step>, Result<(), PlanError>) {
    let emitted = vec![false; blocks];
    let mut frames = Vec::new();
    frames.push(Frame::BarDone);
    let mut p = Planner {
        frames,
        emitted,
        steps: Vec::new(),
    };
    let r = do_tree(cfg, &mut p, 0);
    (p.steps, r)
}
