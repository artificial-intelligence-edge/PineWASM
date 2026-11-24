//! Control-flow kernel of [`super::analyze`]: basic blocks, reverse postorder,
//! Cooper-Harvey-Kennedy dominators, the reducibility check, loop headers,
//! merge counts and natural-loop membership.
//!
//! Written closure-free with index loops and `Vec`-indexed state so that
//! Charon/Aeneas can translate it. The CSIL-facing part
//! (which opcode is a jump) is the classifier [`super::ctrl_of`]; everything
//! from there on is this module.

use super::{Block, Term};

/// "No value" marker for `rpo` (unreachable block) and `idom` (not yet
/// computed / unreachable).
pub(crate) const NONE: usize = usize::MAX;

/// Control-transfer class of one instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ctrl {
    /// Not a terminator: execution continues at `pc + 1`.
    Fall,
    /// Unconditional jump to the target pc.
    Jump(usize),
    /// Two-way branch: the target pc or `pc + 1`.
    Branch(usize),
    /// End of the per-bar program.
    Return,
}

/// Why block recovery rejected the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CfgError {
    /// A jump/branch target, or a branch fall-through, is at or past the end.
    TargetOutOfRange(usize),
    /// A block without a jump/return is the last one.
    FallsOffEnd,
}

/// Basic blocks and their successor graph.
pub(crate) struct BlockGraph {
    pub blocks: Vec<Block>,
    pub term: Vec<Term>,
    /// Successor block ids per block, in `Term` order (`taken` before `fall`).
    pub succ: Vec<Vec<usize>>,
    /// The block containing each pc.
    pub block_of_pc: Vec<usize>,
}

/// Split the code into basic blocks. A leader is pc 0, every jump target and
/// every pc after a terminator. Errors are reported in a fixed order: all
/// target checks (by pc, target before fall-through), then the fall-off-the-end
/// check (by block).
pub(crate) fn cfg_blocks(ctrl: &[Ctrl]) -> Result<BlockGraph, CfgError> {
    let len = ctrl.len();
    let mut leader = vec![false; len];
    leader[0] = true;
    // First bad target (the scan stops there), as a flag: no early return
    // from inside the loop.
    let mut bad = false;
    let mut bad_target = 0;
    let mut pc = 0;
    while pc < len && !bad {
        let c = ctrl[pc];
        match c {
            Ctrl::Fall => {}
            Ctrl::Jump(target) => {
                if pc + 1 < len {
                    leader[pc + 1] = true;
                }
                if target >= len {
                    bad = true;
                    bad_target = target;
                } else {
                    leader[target] = true;
                }
            }
            Ctrl::Branch(target) => {
                if pc + 1 < len {
                    leader[pc + 1] = true;
                }
                if target >= len {
                    bad = true;
                    bad_target = target;
                } else {
                    leader[target] = true;
                    if pc + 1 >= len {
                        bad = true;
                        bad_target = pc + 1;
                    }
                }
            }
            Ctrl::Return => {
                if pc + 1 < len {
                    leader[pc + 1] = true;
                }
            }
        }
        pc += 1;
    }
    if bad {
        return Err(CfgError::TargetOutOfRange(bad_target));
    }

    // Blocks in pc order; `block_of_pc[pc]` = index of the last leader <= pc
    // (`cur`; pc 0 is a leader, so it is set before any use).
    let mut blocks: Vec<Block> = Vec::new();
    let mut block_of_pc = vec![0usize; len];
    let mut cur = 0;
    let mut pc = 0;
    while pc < len {
        if leader[pc] {
            if pc > 0 {
                blocks[cur].end = pc;
            }
            cur = blocks.len();
            blocks.push(Block {
                start: pc,
                end: len,
            });
        }
        block_of_pc[pc] = cur;
        pc += 1;
    }

    let n = blocks.len();
    let mut term: Vec<Term> = Vec::new();
    let mut succ: Vec<Vec<usize>> = Vec::new();
    // Only the last block can fall off the end; checked after the loop.
    let mut falls_off = false;
    let mut b = 0;
    while b < n {
        let end = blocks[b].end;
        let c = ctrl[end - 1];
        let t = match c {
            Ctrl::Return => Term::Return,
            Ctrl::Jump(target) => Term::Jump(block_of_pc[target]),
            // `end < len`: the branch's fall-through was range-checked.
            Ctrl::Branch(target) => Term::Branch {
                taken: block_of_pc[target],
                fall: block_of_pc[end],
            },
            Ctrl::Fall => {
                if end >= len {
                    falls_off = true;
                    Term::Return
                } else {
                    Term::Fall(block_of_pc[end])
                }
            }
        };
        let mut out: Vec<usize> = Vec::new();
        match t {
            Term::Fall(s) | Term::Jump(s) => out.push(s),
            Term::Branch { taken, fall } => {
                out.push(taken);
                out.push(fall);
            }
            Term::Return => {}
        }
        succ.push(out);
        term.push(t);
        b += 1;
    }
    if falls_off {
        return Err(CfgError::FallsOffEnd);
    }
    Ok(BlockGraph {
        blocks,
        term,
        succ,
        block_of_pc,
    })
}

/// Reverse postorder from block 0 (iterative DFS with an explicit
/// `(block, next successor index)` stack). Unreachable blocks are absent.
pub(crate) fn cfg_rpo_order(succ: &[Vec<usize>]) -> Vec<usize> {
    let n = succ.len();
    let mut visited = vec![false; n];
    let mut post: Vec<usize> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    stack.push((0, 0));
    visited[0] = true;
    loop {
        match stack.pop() {
            None => break,
            Some((b, i)) => {
                if i < succ[b].len() {
                    stack.push((b, i + 1));
                    let s = succ[b][i];
                    if !visited[s] {
                        visited[s] = true;
                        stack.push((s, 0));
                    }
                } else {
                    post.push(b);
                }
            }
        }
    }
    let mut rpo_order: Vec<usize> = Vec::new();
    let mut k = post.len();
    while k > 0 {
        k -= 1;
        rpo_order.push(post[k]);
    }
    rpo_order
}

/// Position of each block in `rpo_order`; [`NONE`] for unreachable blocks.
pub(crate) fn cfg_rpo_index(n: usize, rpo_order: &[usize]) -> Vec<usize> {
    let mut rpo = vec![NONE; n];
    let mut i = 0;
    while i < rpo_order.len() {
        rpo[rpo_order[i]] = i;
        i += 1;
    }
    rpo
}

/// Predecessor lists over reachable source blocks.
pub(crate) fn cfg_preds(succ: &[Vec<usize>], rpo: &[usize]) -> Vec<Vec<usize>> {
    let n = succ.len();
    let mut preds: Vec<Vec<usize>> = Vec::new();
    let mut b = 0;
    while b < n {
        preds.push(Vec::new());
        b += 1;
    }
    let mut b = 0;
    while b < n {
        if rpo[b] != NONE {
            let mut j = 0;
            while j < succ[b].len() {
                let s = succ[b][j];
                preds[s].push(b);
                j += 1;
            }
        }
        b += 1;
    }
    preds
}

/// CHK `intersect`: walk both fingers up the current dominator tree until
/// they meet.
pub(crate) fn chk_intersect(idom: &[usize], rpo: &[usize], a: usize, b: usize) -> usize {
    let mut a = a;
    let mut b = b;
    while a != b {
        while rpo[a] > rpo[b] {
            a = idom[a];
        }
        while rpo[b] > rpo[a] {
            b = idom[b];
        }
    }
    a
}

/// Cooper-Harvey-Kennedy iterative dominators over `rpo_order`: immediate
/// dominator per block (block 0 maps to itself, unreachable blocks to
/// [`NONE`]).
pub(crate) fn chk_idoms(rpo_order: &[usize], rpo: &[usize], preds: &[Vec<usize>]) -> Vec<usize> {
    let n = rpo.len();
    let mut idom = vec![NONE; n];
    idom[0] = 0;
    let mut changed = true;
    while changed {
        changed = false;
        let mut k = 1;
        while k < rpo_order.len() {
            let b = rpo_order[k];
            let mut new_idom = NONE;
            let mut j = 0;
            while j < preds[b].len() {
                let p = preds[b][j];
                if rpo[p] != NONE && idom[p] != NONE {
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        chk_intersect(&idom, rpo, new_idom, p)
                    };
                }
                j += 1;
            }
            if new_idom != NONE && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
            k += 1;
        }
    }
    idom
}

/// Whether `a` is on the `idom` chain of `b` (`a` dominates `b`).
pub(crate) fn chk_dominates(a: usize, b: usize, idom: &[usize]) -> bool {
    let mut b = b;
    let mut result = false;
    let mut searching = true;
    while searching {
        if a == b {
            result = true;
            searching = false;
        } else if b == 0 || idom[b] == NONE {
            searching = false;
        } else {
            let next = idom[b];
            if next == b {
                searching = false;
            } else {
                b = next;
            }
        }
    }
    result
}

/// Edge classification over reachable blocks: a retreating edge `b -> s`
/// (`rpo[s] <= rpo[b]`) marks `s` a loop header and is reducible iff `s`
/// dominates `b`; every other edge counts as a forward in-edge of `s`.
pub(crate) struct EdgeShape {
    pub loop_header: Vec<bool>,
    pub forward_in: Vec<u32>,
    /// Some retreating edge's target does not dominate its source.
    pub irreducible: bool,
}

pub(crate) fn cfg_classify_edges(succ: &[Vec<usize>], rpo: &[usize], idom: &[usize]) -> EdgeShape {
    let n = succ.len();
    let mut loop_header = vec![false; n];
    let mut forward_in = vec![0u32; n];
    let mut irreducible = false;
    let mut b = 0;
    while b < n {
        if rpo[b] != NONE {
            let mut j = 0;
            while j < succ[b].len() {
                let s = succ[b][j];
                if rpo[s] <= rpo[b] {
                    if !chk_dominates(s, b, idom) {
                        irreducible = true;
                    }
                    loop_header[s] = true;
                } else {
                    forward_in[s] += 1;
                }
                j += 1;
            }
        }
        b += 1;
    }
    EdgeShape {
        loop_header,
        forward_in,
        irreducible,
    }
}

/// Natural loop of the back edge `latch -> header`: the header plus every
/// reachable block that reaches `latch` backwards without passing through
/// the header (standard predecessor worklist).
pub(crate) fn cfg_loop_body(
    preds: &[Vec<usize>],
    rpo: &[usize],
    header: usize,
    latch: usize,
) -> Vec<bool> {
    let n = preds.len();
    let mut seen = vec![false; n];
    seen[header] = true;
    let mut work: Vec<usize> = Vec::new();
    work.push(latch);
    loop {
        match work.pop() {
            None => break,
            Some(x) => {
                if !seen[x] {
                    seen[x] = true;
                    let mut j = 0;
                    while j < preds[x].len() {
                        let p = preds[x][j];
                        if rpo[p] != NONE {
                            work.push(p);
                        }
                        j += 1;
                    }
                }
            }
        }
    }
    seen
}

/// Union of the natural loops of every retreating edge. Each edge gets its
/// own visited set: nested loops share blocks, so the union must not stop an
/// outer walk at inner members.
pub(crate) fn cfg_in_loop(succ: &[Vec<usize>], preds: &[Vec<usize>], rpo: &[usize]) -> Vec<bool> {
    let n = succ.len();
    let mut in_loop = vec![false; n];
    let mut b = 0;
    while b < n {
        if rpo[b] != NONE {
            let mut j = 0;
            while j < succ[b].len() {
                let s = succ[b][j];
                if rpo[s] <= rpo[b] {
                    let seen = cfg_loop_body(preds, rpo, s, b);
                    let mut k = 0;
                    while k < n {
                        in_loop[k] = in_loop[k] || seen[k];
                        k += 1;
                    }
                }
                j += 1;
            }
        }
        b += 1;
    }
    in_loop
}

/// Everything [`super::analyze`] needs from the block graph.
pub(crate) struct CfgShape {
    pub rpo_order: Vec<usize>,
    pub rpo: Vec<usize>,
    pub idom: Vec<usize>,
    pub loop_header: Vec<bool>,
    pub forward_in: Vec<u32>,
    /// Natural-loop membership; empty when `irreducible`.
    pub in_loop: Vec<bool>,
    pub irreducible: bool,
}

pub(crate) fn cfg_shape(succ: &[Vec<usize>]) -> CfgShape {
    let rpo_order = cfg_rpo_order(succ);
    let rpo = cfg_rpo_index(succ.len(), &rpo_order);
    let preds = cfg_preds(succ, &rpo);
    let idom = chk_idoms(&rpo_order, &rpo, &preds);
    let edges = cfg_classify_edges(succ, &rpo, &idom);
    let in_loop = if edges.irreducible {
        Vec::new()
    } else {
        cfg_in_loop(succ, &preds, &rpo)
    };
    CfgShape {
        rpo_order,
        rpo,
        idom,
        loop_header: edges.loop_header,
        forward_in: edges.forward_in,
        in_loop,
        irreducible: edges.irreducible,
    }
}
