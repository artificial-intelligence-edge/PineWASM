//! Splitting a module's per-bar code into several functions.
//!
//! V8 optimizes a WebAssembly function as a whole, on a background thread.
//! A large monolithic per-bar body can keep executing baseline code while
//! the optimizing compiler processes it. Splitting eligible regions into a
//! tree of smaller functions lets the engine optimize them independently
//! and distribute compilation work across background threads.
//!
//! A piece is the code of one `do_tree` call of the structure plan
//! ([`super::plan`]): a block and everything it dominates, whose steps are
//! contiguous in the plan. Control enters such a region only at its first
//! block, and it leaves only by branching to a label opened before it. A
//! region moves into its own function when every such branch names the same
//! label, no value is on the operand stack at its first block or at that
//! label, and it is not inside a loop: a loop and its body always stay in one
//! function, so nothing is called per iteration. The caller replaces the
//! region's steps with one call and then branches to that label itself. The
//! steps, instructions, fuel windows, stops and error triples are the ones
//! the unsplit body emits, in the same order; only the function boundaries
//! move.
//!
//! Standalone modules use the same plan and handoffs. For a strategy,
//! `bar_step` keeps the broker's pre-script and post-script phases outside
//! the complete piece tree: a normal body exit reaches the close pass and
//! equity sample once, while a nonzero stop returns before that phase.
//!
//! A piece returns 0 ([`EXIT`]) when it leaves through its label, and
//! otherwise the status a stop inside it returned, which is never 0 and
//! which every caller returns unchanged at once, as the unsplit body
//! returned it. Between functions, the current-bar cells and the two budget
//! counters live in globals (see `crate::emit`); each piece keeps the cells
//! its own code uses most, and both counters, in typed locals.

use super::dom_children;
use super::plan::{self, Cfg, Frame, Step};
use crate::analysis::{Analysis, Term};
use pinewasm_csil::{CsilInstr, CsilProgram};
use std::collections::{BTreeMap, BTreeSet};

/// The per-bar body is split when it has more CSIL instructions than this.
/// It is also each piece's target size, not a cap. While a region's own code
/// (what has not moved out of it) exceeds the target, the planner moves out
/// its largest movable subregions; a basic block is never divided, and code
/// that cannot move stays in its parent: a loop and its body, a region
/// entered or left with a value on the operand stack, and a region that
/// exits to more than one label. Regions that cannot move can therefore
/// exceed the target even after their movable subregions are extracted.
pub(crate) const PIECE_TARGET_INSTRS: usize = 1_000;

/// At most this many of a piece's current-bar cells live in its locals.
const MAX_PIECE_HOT_CELLS: usize = 64;

/// A cell gets a local in a piece only when the piece's code uses it at
/// least this much (each read and store weighs `16^d`, `d` the number of
/// loops around it, as for the unsplit body): loading it on entry and
/// storing it on exit costs as much as a few direct global accesses.
const MIN_PIECE_HOT_WEIGHT: u64 = 4;

/// What a piece returns when it leaves through its exit. Every stop returns
/// a nonzero status (`crate::status`, or a `host.vm` site's nonzero return),
/// so any other result is a stop's.
pub(crate) const EXIT: i32 = crate::status::OK;

/// One function of a split body.
pub(crate) struct Piece {
    /// The piece's steps, `start..end` of [`SplitPlan::steps`], including
    /// the steps of the pieces it calls.
    pub start: usize,
    pub end: usize,
    /// The pieces this one calls, in step order: `(first step, end step,
    /// piece)`. Each replaces its steps with one call.
    pub calls: Vec<(usize, usize, usize)>,
    /// The one label outside the piece that its code, and the code of every
    /// piece it calls, branches to.
    pub exit: Frame,
    /// Operand-stack depths the piece's own code uses.
    pub max_stack: u32,
    /// The slots whose current cells the piece keeps in locals, in local
    /// order, and whether its own code stores to each.
    pub hot: Vec<u32>,
    pub hot_written: Vec<bool>,
    /// Slots the piece or a piece it calls (transitively) reads or stores,
    /// and stores, sorted.
    pub accessed: Vec<u32>,
    pub written: Vec<u32>,
}

/// The split of a body into pieces. `pieces[0]` is the whole body; it calls
/// the pieces outlined from it, which may call others.
pub(crate) struct SplitPlan {
    pub steps: Vec<Step>,
    pub pieces: Vec<Piece>,
}

/// The labels a dominator subtree's branches leave it for: none, one, or
/// more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exits {
    None,
    One(Target),
    Many,
}

/// A label a region can leave for: the end of the body, or a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    BarDone,
    Block(usize),
}

impl Exits {
    fn add(self, target: Target) -> Exits {
        match self {
            Exits::None => Exits::One(target),
            Exits::One(t) if t == target => self,
            _ => Exits::Many,
        }
    }
}

impl SplitPlan {
    /// The plan of `program`'s per-bar body, or `None` when the body stays
    /// one function: it is small, the structure plan failed (the unsplit
    /// emitter then reports the failure where it always has), or no region
    /// can move.
    pub(crate) fn plan(program: &CsilProgram, analysis: &Analysis) -> Option<SplitPlan> {
        let code = &program.main().code;
        if code.len() <= PIECE_TARGET_INSTRS {
            return None;
        }
        let a = analysis;
        let dom_children = dom_children(a);
        let cfg = Cfg {
            term: &a.term,
            rpo: &a.rpo,
            loop_header: &a.loop_header,
            merge: &a.merge,
            dom_children: &dom_children,
        };
        let (steps, planned) = plan::plan_structure(&cfg, a.blocks.len());
        planned.ok()?;
        let regions = regions(&cfg, a.blocks.len())?;
        if regions.steps != steps {
            // The region walk mirrors `plan_structure`; any drift between the
            // two keeps the body whole rather than guessing.
            return None;
        }
        let loop_depth = crate::emit::block_loop_depths(a);
        let exits = subtree_exits(a)?;
        // Instructions before each step, so a range's size is a difference.
        let mut before = Vec::with_capacity(steps.len() + 1);
        before.push(0usize);
        for step in &steps {
            let n = match step {
                Step::Code(x) => a.blocks[*x].end - a.blocks[*x].start,
                _ => 0,
            };
            before.push(before.last().copied().unwrap_or(0) + n);
        }
        let n = regions.list.len();
        let size: Vec<usize> = regions
            .list
            .iter()
            .map(|r| before[r.end] - before[r.start])
            .collect();
        let movable: Vec<bool> = regions
            .list
            .iter()
            .enumerate()
            .map(|(i, r)| {
                i != 0
                    && loop_depth.get(r.x).copied().unwrap_or(1) == 0
                    && a.entry_stack[r.x].is_empty()
                    && match exits[r.x] {
                        Exits::One(Target::BarDone) => true,
                        Exits::One(Target::Block(t)) => a.entry_stack[t].is_empty(),
                        Exits::None | Exits::Many => false,
                    }
            })
            .collect();

        // Bottom-up: when a region's own code (everything not yet moved out of
        // it) exceeds the target, move out its largest movable children.
        let mut outlined = vec![false; n];
        let mut own = size.clone();
        for &id in &regions.post_order {
            let mut residue = size[id];
            for &child in &regions.list[id].children {
                residue = residue - size[child] + own[child];
            }
            if residue > PIECE_TARGET_INSTRS {
                let mut children: Vec<usize> = regions.list[id]
                    .children
                    .iter()
                    .copied()
                    .filter(|&child| movable[child])
                    .collect();
                children.sort_by(|&p, &q| own[q].cmp(&own[p]).then(p.cmp(&q)));
                for child in children {
                    if residue <= PIECE_TARGET_INSTRS {
                        break;
                    }
                    outlined[child] = true;
                    residue -= own[child];
                }
            }
            own[id] = residue;
        }
        outlined[0] = true;
        if outlined.iter().filter(|o| **o).count() < 2 {
            return None;
        }

        // Pieces in step order; the whole body (region 0) first.
        let mut order: Vec<usize> = (0..n).filter(|&i| outlined[i]).collect();
        order.sort_by_key(|&i| {
            (
                regions.list[i].start,
                std::cmp::Reverse(regions.list[i].end),
            )
        });
        let piece_of: BTreeMap<usize, usize> =
            order.iter().enumerate().map(|(p, &r)| (r, p)).collect();
        let mut pieces = Vec::with_capacity(order.len());
        for &r in &order {
            let region = &regions.list[r];
            // The whole body leaves only at its end. A movable region's one
            // exit is a merge block's label: it cannot be a loop header,
            // which would dominate the region and put it inside the loop.
            let exit = match (r, exits[region.x]) {
                (0, _) | (_, Exits::One(Target::BarDone)) => Frame::BarDone,
                (_, Exits::One(Target::Block(t))) => Frame::BlockEnd(t),
                _ => return None,
            };
            pieces.push(Piece {
                start: region.start,
                end: region.end,
                calls: Vec::new(),
                exit,
                max_stack: 0,
                hot: Vec::new(),
                hot_written: Vec::new(),
                accessed: Vec::new(),
                written: Vec::new(),
            });
        }
        // Each piece's direct callees: the outlined regions nearest below it.
        for (p, &r) in order.iter().enumerate() {
            let mut work: Vec<usize> = regions.list[r].children.clone();
            let mut calls = Vec::new();
            while let Some(child) = work.pop() {
                match piece_of.get(&child) {
                    Some(&q) => calls.push((regions.list[child].start, regions.list[child].end, q)),
                    None => work.extend(regions.list[child].children.iter().copied()),
                }
            }
            calls.sort_unstable();
            pieces[p].calls = calls;
        }
        // Every branch out of a piece's own steps, and every callee's exit,
        // either names a label the piece opened or the piece's own exit;
        // otherwise the body stays whole. Each step is read once, by the
        // piece that owns it.
        for piece in &pieces {
            if !leaves_only_through_its_exit(&steps, piece, &pieces) {
                return None;
            }
        }

        // The piece's own code (its steps minus its callees') sets its stack
        // depths and cached cells; callees come later in step order, so going
        // backwards finds every callee's cells first.
        let main_stack = program.main().max_stack;
        for p in (0..pieces.len()).rev() {
            let mut weight: BTreeMap<u32, (u64, bool)> = BTreeMap::new();
            let mut accessed = BTreeSet::new();
            let mut written = BTreeSet::new();
            let mut max_stack = 0u32;
            let mut at = pieces[p].start;
            let mut calls = pieces[p].calls.iter().peekable();
            while at < pieces[p].end {
                if let Some(&&(start, end, q)) = calls.peek() {
                    if start == at {
                        accessed.extend(pieces[q].accessed.iter().copied());
                        written.extend(pieces[q].written.iter().copied());
                        at = end;
                        calls.next();
                        continue;
                    }
                }
                if let Step::Code(x) = steps[at] {
                    let block = &a.blocks[x];
                    let w = 16u64.pow(loop_depth[x].min(4));
                    max_stack = max_stack.max(a.entry_stack[x].len() as u32);
                    for (pc, instr) in code.iter().enumerate().take(block.end).skip(block.start) {
                        let results = a.result_tys.get(pc).map_or(0, Vec::len) as u32;
                        max_stack = max_stack.max(a.depth_at[pc] + results);
                        let (slot, store) = match *instr {
                            CsilInstr::ReadUserSlot { slot, offset: 0 } => (slot, false),
                            CsilInstr::StoreUserSlotCurrent { slot } => (slot, true),
                            _ => continue,
                        };
                        accessed.insert(slot);
                        if store {
                            written.insert(slot);
                        }
                        let cell = weight.entry(slot).or_insert((0, false));
                        cell.0 = cell.0.saturating_add(w);
                        cell.1 |= store;
                    }
                }
                at += 1;
            }
            let mut ranked: Vec<(u32, u64, bool)> = weight
                .into_iter()
                .filter(|(_, (w, _))| *w >= MIN_PIECE_HOT_WEIGHT)
                .map(|(slot, (w, store))| (slot, w, store))
                .collect();
            // Heaviest first; the slot breaks ties, so the plan is a function
            // of the program alone.
            ranked.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
            ranked.truncate(MAX_PIECE_HOT_CELLS);
            ranked.sort_by_key(|cell| cell.0);
            let piece = &mut pieces[p];
            // The stack locals cover every depth the piece's own code uses,
            // and the unsplit body never uses more than the program declares.
            piece.max_stack = max_stack.min(main_stack);
            piece.hot = ranked.iter().map(|cell| cell.0).collect();
            piece.hot_written = ranked.iter().map(|cell| cell.2).collect();
            piece.accessed = accessed.into_iter().collect();
            piece.written = written.into_iter().collect();
        }
        Some(SplitPlan { steps, pieces })
    }
}

/// For every block, the labels its dominator subtree's branches leave it
/// for. A forward edge `b -> t` leaves the subtree of every block on the
/// dominator-tree path from `b` up to, but not including, `idom(t)` (which
/// dominates `b`); a back edge to a loop header leaves every subtree up to
/// the header; a `Return` leaves every subtree that holds it. `None` for a
/// CFG this walk does not recognize.
fn subtree_exits(a: &Analysis) -> Option<Vec<Exits>> {
    let n = a.blocks.len();
    let reachable = |b: usize| a.rpo.get(b).is_some_and(|&r| r != usize::MAX);
    // Sources per target, so each target walks the union of its paths once.
    let mut sources: BTreeMap<(u8, usize), Vec<usize>> = BTreeMap::new();
    for b in (0..n).filter(|&b| reachable(b)) {
        let mut edge = |t: usize| sources.entry((1, t)).or_default().push(b);
        match a.term[b] {
            Term::Return => sources.entry((0, 0)).or_default().push(b),
            Term::Fall(t) | Term::Jump(t) => edge(t),
            Term::Branch { taken, fall } => {
                edge(taken);
                edge(fall);
            }
        }
    }
    let mut exits = vec![Exits::None; n];
    let mut marked = vec![usize::MAX; n];
    for (serial, ((kind, t), from)) in sources.into_iter().enumerate() {
        let (target, stop) = if kind == 0 {
            (Target::BarDone, None)
        } else {
            let t_idom = *a.idom.get(t)?;
            let back = from.iter().all(|&b| a.rpo[t] <= a.rpo[b]);
            let forward = from.iter().all(|&b| a.rpo[t] > a.rpo[b]);
            match (back, forward) {
                (true, false) => (Target::Block(t), Some(t)),
                (false, true) => (Target::Block(t), Some(t_idom)),
                // A target reached by both kinds of edge is a loop header
                // entered from before the loop and from its back edges: its
                // forward edges leave nothing below its idom, its back edges
                // every subtree inside the loop up to the header.
                _ => {
                    for &b in &from {
                        let stop = if a.rpo[t] <= a.rpo[b] { t } else { t_idom };
                        walk_up(
                            a,
                            b,
                            Some(stop),
                            Target::Block(t),
                            serial,
                            &mut marked,
                            &mut exits,
                        )?;
                    }
                    continue;
                }
            }
        };
        for &b in &from {
            walk_up(a, b, stop, target, serial, &mut marked, &mut exits)?;
        }
    }
    Some(exits)
}

/// Record `target` as an exit of every block's subtree from `b` up the
/// dominator tree, stopping before `stop` (or after the root), or at a block
/// this target already marked.
fn walk_up(
    a: &Analysis,
    mut b: usize,
    stop: Option<usize>,
    target: Target,
    serial: usize,
    marked: &mut [usize],
    exits: &mut [Exits],
) -> Option<()> {
    for _ in 0..=a.blocks.len() {
        if Some(b) == stop || marked[b] == serial {
            return Some(());
        }
        marked[b] = serial;
        exits[b] = exits[b].add(target);
        let up = *a.idom.get(b)?;
        if up == b || up == usize::MAX {
            return Some(());
        }
        b = up;
    }
    None
}

/// Whether every branch in `piece`'s own steps names a label the piece
/// opened or its exit, and every callee's exit is one of those too: the
/// checks `emit_piece` and `emit_piece_call` make, made before anything is
/// emitted.
fn leaves_only_through_its_exit(steps: &[Step], piece: &Piece, pieces: &[Piece]) -> bool {
    let mut open: Vec<Frame> = Vec::new();
    let mut at = piece.start;
    let mut calls = piece.calls.iter().peekable();
    while at < piece.end {
        if let Some(&&(start, end, callee)) = calls.peek() {
            if start == at {
                let exit = pieces[callee].exit;
                if plan::depth_of(&open, exit).is_none() && !plan::frame_eq(exit, piece.exit) {
                    return false;
                }
                at = end;
                calls.next();
                continue;
            }
        }
        match steps[at] {
            Step::Loop(x) => open.push(Frame::LoopHeader(x)),
            Step::Block(y) => open.push(Frame::BlockEnd(y)),
            Step::If => open.push(Frame::Opaque),
            Step::End => {
                open.pop();
            }
            Step::Br(depth, frame) if depth >= open.len() && !plan::frame_eq(frame, piece.exit) => {
                return false;
            }
            _ => {}
        }
        at += 1;
    }
    true
}

/// One `do_tree` call of the structure plan.
struct Region {
    /// The dominator-subtree root.
    x: usize,
    start: usize,
    end: usize,
    children: Vec<usize>,
}

struct Regions {
    steps: Vec<Step>,
    list: Vec<Region>,
    post_order: Vec<usize>,
}

/// Replay `plan_structure` and record where each `do_tree` call's steps
/// start and end. `None` on any planning error.
fn regions(cfg: &Cfg<'_>, blocks: usize) -> Option<Regions> {
    let mut walk = Walk {
        cfg,
        frames: vec![Frame::BarDone],
        emitted: vec![false; blocks],
        steps: Vec::new(),
        list: Vec::new(),
        open: Vec::new(),
        post_order: Vec::new(),
    };
    walk.do_tree(0)?;
    Some(Regions {
        steps: walk.steps,
        list: walk.list,
        post_order: walk.post_order,
    })
}

/// The walk of [`plan::plan_structure`], step for step, plus the region
/// bookkeeping. [`SplitPlan::plan`] checks that both produce the same steps.
struct Walk<'c, 'a> {
    cfg: &'c Cfg<'a>,
    frames: Vec<Frame>,
    emitted: Vec<bool>,
    steps: Vec<Step>,
    list: Vec<Region>,
    /// The regions whose walk is in progress, innermost last.
    open: Vec<usize>,
    post_order: Vec<usize>,
}

impl Walk<'_, '_> {
    fn br(&mut self, want: Frame) -> Option<()> {
        let depth = plan::depth_of(&self.frames, want)?;
        self.steps.push(Step::Br(depth, want));
        Some(())
    }

    fn do_tree(&mut self, x: usize) -> Option<()> {
        if *self.emitted.get(x)? {
            return None;
        }
        self.emitted[x] = true;
        let id = self.list.len();
        if let Some(&parent) = self.open.last() {
            self.list[parent].children.push(id);
        }
        self.list.push(Region {
            x,
            start: self.steps.len(),
            end: 0,
            children: Vec::new(),
        });
        self.open.push(id);
        let ys = plan::merge_children(self.cfg, x);
        if self.cfg.loop_header[x] {
            self.frames.push(Frame::LoopHeader(x));
            self.steps.push(Step::Loop(x));
            self.node_within(x, &ys, ys.len())?;
            self.steps.push(Step::End);
            self.frames.pop();
        } else {
            self.node_within(x, &ys, ys.len())?;
        }
        self.open.pop();
        self.list[id].end = self.steps.len();
        self.post_order.push(id);
        Some(())
    }

    fn node_within(&mut self, x: usize, ys: &[usize], n: usize) -> Option<()> {
        if n == 0 {
            self.steps.push(Step::Code(x));
            self.terminator(x)
        } else {
            let last = ys[n - 1];
            self.frames.push(Frame::BlockEnd(last));
            self.steps.push(Step::Block(last));
            self.node_within(x, ys, n - 1)?;
            self.steps.push(Step::End);
            self.frames.pop();
            self.do_tree(last)
        }
    }

    fn terminator(&mut self, x: usize) -> Option<()> {
        self.steps.push(Step::Term(x));
        match self.cfg.term[x] {
            Term::Return => self.br(Frame::BarDone),
            Term::Jump(t) | Term::Fall(t) => self.branch_to(t, x),
            Term::Branch { taken, fall } => {
                self.frames.push(Frame::Opaque);
                self.steps.push(Step::If);
                self.branch_to(taken, x)?;
                self.steps.push(Step::Else);
                self.branch_to(fall, x)?;
                self.steps.push(Step::End);
                self.frames.pop();
                Some(())
            }
        }
    }

    fn branch_to(&mut self, target: usize, from: usize) -> Option<()> {
        if self.cfg.rpo[target] <= self.cfg.rpo[from] {
            self.br(Frame::LoopHeader(target))
        } else if self.cfg.merge[target] {
            self.br(Frame::BlockEnd(target))
        } else {
            self.do_tree(target)
        }
    }
}

/// What a piece's emitter needs beyond the body context: the plan, which
/// piece it is, every piece's function index, the local a callee's result
/// goes to, and the globals the counters cross a boundary in.
pub(crate) struct PieceEnv<'p> {
    pub plan: &'p SplitPlan,
    pub piece: usize,
    /// Function index of each piece.
    pub functions: &'p [u32],
    /// A callee's result.
    pub status_local: u32,
    /// The per-bar and run counters while no function holds them in locals.
    pub budget_global: u32,
    pub run_budget_global: u32,
}

impl super::BodyEmitter<'_, '_> {
    /// Emit one piece's steps; the caller wraps them in the block every
    /// branch out of the piece leaves through, and emits the piece's entry
    /// and exit around it. A branch to a label the piece opened is the
    /// unsplit body's branch; the branch to its exit leaves that block. A
    /// callee's steps are replaced by its call.
    pub(crate) fn emit_piece(mut self, env: &PieceEnv<'_>) -> Result<(), crate::AotError> {
        use wasm_encoder::{BlockType, Instruction as I};
        let piece = &env.plan.pieces[env.piece];
        let steps = &env.plan.steps;
        // The labels this piece has open, innermost last. Every label the
        // body had open outside the piece is reached through its exit.
        let mut open: Vec<Frame> = Vec::new();
        let mut at = piece.start;
        let mut calls = piece.calls.iter().peekable();
        while at < piece.end {
            if let Some(&&(start, end, callee)) = calls.peek() {
                if start == at {
                    self.emit_piece_call(env, callee, &open)?;
                    at = end;
                    calls.next();
                    continue;
                }
            }
            match steps[at] {
                Step::Loop(x) => {
                    open.push(Frame::LoopHeader(x));
                    self.f.instruction(&I::Loop(BlockType::Empty));
                }
                Step::Block(y) => {
                    open.push(Frame::BlockEnd(y));
                    self.f.instruction(&I::Block(BlockType::Empty));
                }
                Step::If => {
                    open.push(Frame::Opaque);
                    self.f.instruction(&I::If(BlockType::Empty));
                }
                Step::Else => {
                    self.f.instruction(&I::Else);
                    // As in the unsplit replay: the stack was flushed before
                    // the test.
                    self.vstack
                        .iter_mut()
                        .for_each(|cell| cell.1 = super::Loc::Local);
                }
                Step::End => {
                    open.pop();
                    self.f.instruction(&I::End);
                }
                Step::Br(depth, frame) => {
                    if depth < open.len() {
                        self.f.instruction(&I::Br(depth as u32));
                    } else if plan::frame_eq(frame, piece.exit) {
                        self.f.instruction(&I::Br(open.len() as u32));
                    } else {
                        return Err(crate::AotError::InvalidProgram(format!(
                            "piece {} branches to {frame:?}, not its exit {:?}",
                            env.piece, piece.exit
                        )));
                    }
                }
                Step::Code(x) => self.emit_block_code(x)?,
                Step::Term(x) => self.emit_terminator(x)?,
            }
            at += 1;
        }
        Ok(())
    }

    /// Call `callee` in place of its steps. The cells this piece keeps in
    /// locals that the callee may touch, and both counters, go to their
    /// globals first and come back after; a stop's status (any result but
    /// [`EXIT`]) returns at once, as it did from inside the callee; then the
    /// piece branches to the callee's exit, or leaves through its own exit,
    /// which is the same label when it is outside this piece too.
    fn emit_piece_call(
        &mut self,
        env: &PieceEnv<'_>,
        callee: usize,
        open: &[Frame],
    ) -> Result<(), crate::AotError> {
        use wasm_encoder::{BlockType, Instruction as I};
        let me = &env.plan.pieces[env.piece];
        let target = &env.plan.pieces[callee];
        let l = self.ctx.locals;
        for &slot in &me.hot {
            if target.accessed.binary_search(&slot).is_ok() {
                self.f
                    .instruction(&l.cur_get(slot))
                    .instruction(&I::GlobalSet(l.home(slot)));
            }
        }
        self.f
            .instruction(&I::LocalGet(l.budget))
            .instruction(&I::GlobalSet(env.budget_global))
            .instruction(&I::LocalGet(l.run_budget))
            .instruction(&I::GlobalSet(env.run_budget_global))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::Call(env.functions[callee]))
            .instruction(&I::LocalTee(env.status_local))
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(env.status_local))
            .instruction(&I::Return)
            .instruction(&I::End)
            .instruction(&I::GlobalGet(env.budget_global))
            .instruction(&I::LocalSet(l.budget))
            .instruction(&I::GlobalGet(env.run_budget_global))
            .instruction(&I::LocalSet(l.run_budget));
        for &slot in &me.hot {
            if target.written.binary_search(&slot).is_ok() {
                self.f
                    .instruction(&I::GlobalGet(l.home(slot)))
                    .instruction(&l.cur_set(slot));
            }
        }
        match plan::depth_of(open, target.exit) {
            Some(depth) => {
                self.f.instruction(&I::Br(depth as u32));
            }
            None if plan::frame_eq(target.exit, me.exit) => {
                self.f.instruction(&I::Br(open.len() as u32));
            }
            None => {
                return Err(crate::AotError::InvalidProgram(format!(
                    "piece {callee} leaves for {:?}, outside piece {} but not its exit {:?}",
                    target.exit, env.piece, me.exit
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SplitPlan, Step};

    /// A script with a loop whose body is larger than a piece, then enough
    /// straight code to split.
    fn source() -> String {
        let mut source = String::from(
            "//@version=6\nindicator(\"Split plan\")\nfloat total = 0.0\nfor i = 0 to 1\n",
        );
        for g in 0..40 {
            source.push_str(&format!(
                "    float l{g} = close * {}.0 - open * i\n    if l{g} > high\n        l{g} := l{g} - high\n    \
                 total += l{g} > close ? l{g} - close : close - l{g}\n",
                g + 1
            ));
        }
        source.push_str("if bar_index % 3 != 1\n");
        for g in 0..40 {
            source.push_str(&format!(
                "    float m{g} = close * {}.0 - open\n    if m{g} > high\n        m{g} := m{g} - high\n    total += m{g}\n",
                g + 1
            ));
        }
        for g in 0..60 {
            source.push_str(&format!(
                "float t{g} = close * {}.0 - open\nif t{g} > high\n    t{g} := t{g} - high\ntotal += t{g}\n",
                g + 1
            ));
        }
        source.push_str("plot(total)\n");
        source
    }

    /// No piece's region starts inside a loop, so a loop and its body stay in
    /// one function and nothing is called per iteration, even when the loop's
    /// body is larger than a piece.
    #[test]
    fn a_split_keeps_every_loop_in_one_function() {
        let result =
            pinewasm_compiler::compile(&source(), pinewasm_compiler::CompileOptions::default());
        let program = result.program.expect("the script compiles");
        let native = crate::plan_native(&program).expect("the script is native");
        let split = SplitPlan::plan(&program, &native.analysis).expect("the body splits");
        assert!(split.pieces.len() > 2, "{} pieces", split.pieces.len());
        let loop_depth = crate::emit::block_loop_depths(&native.analysis);
        let mut loop_blocks = 0;
        for (index, piece) in split.pieces.iter().enumerate() {
            let root = split.steps[piece.start..piece.end]
                .iter()
                .find_map(|step| match step {
                    Step::Code(x) => Some(*x),
                    _ => None,
                })
                .expect("a piece holds code");
            assert_eq!(loop_depth[root], 0, "piece {index} starts inside a loop");
            loop_blocks += split.steps[piece.start..piece.end]
                .iter()
                .filter(|step| matches!(step, Step::Code(x) if loop_depth[*x] > 0))
                .count();
        }
        assert!(loop_blocks > 0, "the fixture's loop must be split around");
    }
}
