//! Differential tests for the CFG kernel and the generic fixpoint driver.
//! The CFG part of [`analyze`](super::analyze) is the closure-free kernel
//! [`super::cfg`] (plus the classifier [`super::ctrl_of`]), and the round
//! driver of [`super::infer_collection_elems`] is the generic
//! [`super::fixpoint::run_fixpoint`]. A direct, unoptimized baseline
//! implementation of the same analysis is kept in this file for comparison.
//!
//! The baseline's transfer function also applies the map element typing rule,
//! marked "map element types" below: maps carry their key and value types
//! (`Elem::Map`), so `map.keys`/`map.values`/`map.copy` results and map
//! element results are typed. That rule changes the facts on purpose (a
//! `map.keys` array's `array.get` site is `Known`), so the baseline applies it
//! as well. The baseline checks that the CFG kernel and the generic fixpoint
//! driver compute the same facts as the hand-written round loop. The mirrored
//! lines call the same helpers (`map_new_elem`, `map_result_elem`,
//! `site_map_facts`) as the production pass, so the baseline does not check
//! those helpers; the differential map tests in `tests/differential.rs` do.

#![allow(clippy::too_many_lines)]

use super::{
    constructor_elem, gate_fail, infer_collection_elems, map_new_elem, map_result_elem,
    native_coll_op, native_lane, pushes_literal_int, recover_cfg, site_map_facts, Block, Elem,
    ElemFacts, GcCells, HeapGate, NativeCollOp, Term, VTy,
};
use pinewasm_csil::{CsilInstr, CsilProgram};
use std::collections::{BTreeMap, BTreeSet};

// ---- baseline implementation: CFG recovery (analysis.rs `analyze`) ----

fn successors(instr: &CsilInstr, pc: usize) -> Vec<usize> {
    match instr {
        CsilInstr::Jump { target } => vec![*target as usize],
        CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
        | CsilInstr::JumpIfSlotWritten { target, .. } => vec![*target as usize, pc + 1],
        CsilInstr::Return => vec![],
        _ => vec![pc + 1],
    }
}

fn is_terminator(instr: &CsilInstr) -> bool {
    matches!(
        instr,
        CsilInstr::Jump { .. }
            | CsilInstr::JumpIfFalse { .. }
            | CsilInstr::JumpIfExact { .. }
            | CsilInstr::JumpIfSlotWritten { .. }
            | CsilInstr::Return
    )
}

struct OldCfg {
    blocks: Vec<Block>,
    term: Vec<Term>,
    rpo_order: Vec<usize>,
    rpo: Vec<usize>,
    idom: Vec<usize>,
    loop_header: Vec<bool>,
    merge: Vec<bool>,
    in_loop: Vec<bool>,
    block_of: Vec<usize>,
}

fn old_cfg(code: &[CsilInstr]) -> Result<OldCfg, String> {
    // ---- basic blocks ------------------------------------------------------
    let mut leaders: BTreeSet<usize> = BTreeSet::new();
    leaders.insert(0);
    for (pc, instr) in code.iter().enumerate() {
        if is_terminator(instr) {
            if pc + 1 < code.len() {
                leaders.insert(pc + 1);
            }
            for s in successors(instr, pc) {
                if s >= code.len() {
                    return Err(format!("jump target {s} out of range"));
                }
                leaders.insert(s);
            }
        }
    }
    let leader_list: Vec<usize> = leaders.iter().copied().collect();
    let block_of_pc = |pc: usize| -> usize {
        match leader_list.binary_search(&pc) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    };
    let mut blocks = Vec::with_capacity(leader_list.len());
    for (i, &start) in leader_list.iter().enumerate() {
        let end = leader_list.get(i + 1).copied().unwrap_or(code.len());
        blocks.push(Block { start, end });
    }

    // Terminators + successor block ids.
    let mut term = Vec::with_capacity(blocks.len());
    let mut succ: Vec<Vec<usize>> = Vec::with_capacity(blocks.len());
    for b in &blocks {
        let last_pc = b.end - 1;
        let t = match &code[last_pc] {
            CsilInstr::Return => Term::Return,
            CsilInstr::Jump { target } => Term::Jump(block_of_pc(*target as usize)),
            CsilInstr::JumpIfFalse { target }
            | CsilInstr::JumpIfExact { target, .. }
            | CsilInstr::JumpIfSlotWritten { target, .. } => Term::Branch {
                taken: block_of_pc(*target as usize),
                fall: block_of_pc(b.end),
            },
            _ => Term::Fall(block_of_pc(b.end)),
        };
        // A fall-through off the end of the function is invalid (validator
        // rejects it; defense in depth).
        if matches!(t, Term::Fall(_) | Term::Branch { .. }) && b.end >= code.len() {
            return Err("control flow runs past the end of the function".to_string());
        }
        succ.push(match t {
            Term::Fall(n) => vec![n],
            Term::Jump(t) => vec![t],
            Term::Branch { taken, fall } => vec![taken, fall],
            Term::Return => vec![],
        });
        term.push(t);
    }
    let n = blocks.len();

    // ---- reverse postorder (from block 0) ----------------------------------
    let mut rpo_order: Vec<usize> = Vec::with_capacity(n); // rpo_order[i] = block id
    {
        let mut visited = vec![false; n];
        let mut post: Vec<usize> = Vec::with_capacity(n);
        // Iterative DFS with an explicit phase marker.
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        visited[0] = true;
        while let Some((b, i)) = stack.pop() {
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
        rpo_order.extend(post.iter().rev());
    }
    let mut rpo = vec![usize::MAX; n];
    for (i, &b) in rpo_order.iter().enumerate() {
        rpo[b] = i;
    }
    // Unreachable blocks (rpo MAX) are legal CSIL; skip them everywhere below.
    let reachable = |b: usize| rpo[b] != usize::MAX;

    // ---- dominators (Cooper-Harvey-Kennedy over rpo) -----------------------
    let mut idom = vec![usize::MAX; n];
    idom[0] = 0;
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, succs) in succ.iter().enumerate() {
        if !reachable(b) {
            continue;
        }
        for &s in succs {
            preds[s].push(b);
        }
    }
    let intersect = |idom: &[usize], rpo: &[usize], mut a: usize, mut b: usize| -> usize {
        while a != b {
            while rpo[a] > rpo[b] {
                a = idom[a];
            }
            while rpo[b] > rpo[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo_order.iter().skip(1) {
            let mut new_idom = usize::MAX;
            for &p in &preds[b] {
                if !reachable(p) || idom[p] == usize::MAX {
                    continue;
                }
                new_idom = if new_idom == usize::MAX {
                    p
                } else {
                    intersect(&idom, &rpo, new_idom, p)
                };
            }
            if new_idom != usize::MAX && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }

    let dominates = |a: usize, mut b: usize, idom: &[usize]| -> bool {
        loop {
            if a == b {
                return true;
            }
            if b == 0 || idom[b] == usize::MAX {
                return false;
            }
            let next = idom[b];
            if next == b {
                return false;
            }
            b = next;
        }
    };

    // ---- back edges, loop headers, merges, reducibility --------------------
    let mut loop_header = vec![false; n];
    let mut forward_in = vec![0u32; n];
    for b in 0..n {
        if !reachable(b) {
            continue;
        }
        for &s in &succ[b] {
            if rpo[s] <= rpo[b] {
                // Retreating edge: reducible iff the target dominates the
                // source (a natural-loop back edge).
                if !dominates(s, b, &idom) {
                    return Err("irreducible".to_string());
                }
                loop_header[s] = true;
            } else {
                forward_in[s] += 1;
            }
        }
    }
    let merge: Vec<bool> = forward_in.iter().map(|&c| c >= 2).collect();

    // Natural-loop membership (standard preds worklist per back edge): the
    // run-path policy wants to know whether the native heap is LOOP-bound —
    // the compute-heavy shape where native execution amortizes its per-run
    // setup and residual cold host hops (docs/aot.md §13).
    let mut in_loop = vec![false; n];
    for b in 0..n {
        if !reachable(b) {
            continue;
        }
        for &s in &succ[b] {
            if rpo[s] <= rpo[b] {
                // Per-edge visited set: nested loops share blocks, so the
                // union must not stop an outer walk at inner members.
                let mut seen = vec![false; n];
                seen[s] = true;
                let mut work = vec![b];
                while let Some(x) = work.pop() {
                    if seen[x] {
                        continue;
                    }
                    seen[x] = true;
                    work.extend(preds[x].iter().copied().filter(|&p| reachable(p)));
                }
                for (flag, member) in in_loop.iter_mut().zip(&seen) {
                    *flag |= member;
                }
            }
        }
    }

    let block_of: Vec<usize> = (0..code.len()).map(block_of_pc).collect();
    Ok(OldCfg {
        blocks,
        term,
        rpo_order,
        rpo,
        idom,
        loop_header,
        merge,
        in_loop,
        block_of,
    })
}

// ---- baseline implementation: collection-element fixpoint ----

fn old_infer_collection_elems(
    program: &CsilProgram,
    blocks: &[Block],
    term: &[Term],
    rpo_order: &[usize],
) -> ElemFacts {
    use pinewasm_hir::CollectionAbstract;
    let code = &program.main().code;
    let slots = program.series_slots.len();
    let n = blocks.len();
    let mut entry: Vec<Option<Vec<Elem>>> = vec![None; n];
    entry[0] = Some(Vec::new());
    let mut slot_elems = vec![Elem::Unset; slots];
    // Collection handles stored in UDT fields (`type Features: array<float>
    // f1 …`) keep their element type through GetField/SetField/NewObject.
    // Object cells carry their declared type (`Obj(type_id)`), so fields
    // key by (type_id, field index) and unrelated types never collide.
    let mut field_elems: BTreeMap<(u32, u32), Elem> = BTreeMap::new();
    let mut site_elem: BTreeMap<usize, Elem> = BTreeMap::new();
    let mut ref_getfields: BTreeSet<usize> = BTreeSet::new();
    let mut site_obj: BTreeMap<usize, u32> = BTreeMap::new();
    let mut site_any: BTreeSet<usize> = BTreeSet::new();
    let mut host_types: BTreeSet<u32> = BTreeSet::new();

    let any_heap = code.iter().any(|i| {
        matches!(
            i,
            CsilInstr::CallCollection { .. }
                | CsilInstr::NewObject { .. }
                | CsilInstr::CopyObject { .. }
                | CsilInstr::GetField { .. }
                | CsilInstr::SetField { .. }
        )
    });
    // Evaluated on the post-convergence checking round.
    let mut flow_ok = any_heap;

    // Resolve every opcode through CSIL's shared contract. An invalid
    // descriptor fails the native heap gate closed instead of indexing the
    // TA table independently here.
    let effect = |instr: &CsilInstr| {
        instr
            .resolve_stack_effect(&program.ta_states)
            .map(|effect| (effect.pops as usize, effect.pushes as usize))
    };

    // Fixpoint over the reducible CFG (validated depths make every pop
    // safe), then ONE more `checking` round at the stable state that
    // evaluates the phase D gate and records per-site facts.
    let max_rounds = n + code.len() + 4;
    let mut round = 0;
    let mut checking = false;
    loop {
        let mut changed = false;
        for &b in rpo_order {
            let Some(stack0) = entry[b].clone() else {
                continue;
            };
            let mut stack = stack0;
            // `pc` is the function-wide program counter, not a slice index —
            // the range loop is the semantic form.
            #[allow(clippy::needless_range_loop)]
            for pc in blocks[b].start..blocks[b].end {
                let instr = &code[pc];
                match instr {
                    CsilInstr::Dup => {
                        let top = stack.pop().unwrap_or(Elem::Mixed);
                        stack.push(top);
                        stack.push(top);
                    }
                    CsilInstr::Select => {
                        let rhs = stack.pop().unwrap_or(Elem::Mixed);
                        let lhs = stack.pop().unwrap_or(Elem::Mixed);
                        stack.pop();
                        stack.push(lhs.join(rhs));
                    }
                    // The polymorphic bottom: `na` takes any origin, so a
                    // `cond ? na : myArray` merge keeps the array's facts.
                    CsilInstr::PushNa => {
                        stack.push(Elem::Unset);
                    }
                    CsilInstr::ReadUserSlot { slot, .. } => {
                        stack.push(slot_elems[*slot as usize]);
                    }
                    CsilInstr::ReadUserSlotDynamic { slot } => {
                        stack.pop();
                        stack.push(slot_elems[*slot as usize]);
                    }
                    CsilInstr::StoreUserSlotCurrent { slot } => {
                        let value = stack.pop().unwrap_or(Elem::Mixed);
                        let joined = slot_elems[*slot as usize].join(value);
                        if joined != slot_elems[*slot as usize] {
                            slot_elems[*slot as usize] = joined;
                            changed = true;
                        }
                    }
                    CsilInstr::NewObject {
                        type_id,
                        field_count,
                    } => {
                        let fc = *field_count as usize;
                        for i in 0..fc {
                            let value = stack.pop().unwrap_or(Elem::Mixed);
                            let idx = (fc - 1 - i) as u32; // top = last field
                            let cell = field_elems.entry((*type_id, idx)).or_insert(Elem::Unset);
                            let joined = cell.join(value);
                            if joined != *cell {
                                *cell = joined;
                                changed = true;
                            }
                        }
                        stack.push(if host_types.contains(type_id) {
                            Elem::Opaque
                        } else {
                            Elem::Obj(*type_id)
                        });
                    }
                    CsilInstr::CopyObject { type_id } => {
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        if checking && !host_types.contains(type_id) {
                            match receiver {
                                Elem::Obj(tid) if tid == *type_id => {
                                    site_obj.insert(pc, tid);
                                }
                                _ => flow_ok = gate_fail(pc, "CopyObject receiver unproven"),
                            }
                        }
                        stack.push(if host_types.contains(type_id) {
                            Elem::Opaque
                        } else {
                            Elem::Obj(*type_id)
                        });
                    }
                    CsilInstr::SetField { field_idx } => {
                        let value = stack.pop().unwrap_or(Elem::Mixed);
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        if let Elem::Obj(tid) = receiver {
                            let cell = field_elems.entry((tid, *field_idx)).or_insert(Elem::Unset);
                            let joined = cell.join(value);
                            if joined != *cell {
                                *cell = joined;
                                changed = true;
                            }
                        }
                        if checking {
                            match receiver {
                                Elem::Obj(tid) => {
                                    site_obj.insert(pc, tid);
                                }
                                // Registered drawing handles are host objects.
                                Elem::Opaque => {}
                                _ => flow_ok = gate_fail(pc, "SetField receiver unproven"),
                            }
                        }
                    }
                    CsilInstr::GetField { field_idx } => {
                        let receiver = stack.pop().unwrap_or(Elem::Mixed);
                        let field = match receiver {
                            Elem::Obj(tid) => field_elems
                                .get(&(tid, *field_idx))
                                .copied()
                                .unwrap_or(Elem::Mixed),
                            _ => Elem::Mixed,
                        };
                        // A field state of Known(elem)/Obj(t) means the field
                        // VALUE is itself a heap handle.
                        // map element types: a field holding a map is a handle.
                        if matches!(
                            field,
                            Elem::Known(_) | Elem::Obj(_) | Elem::ObjectArray(_) | Elem::Map(..)
                        ) && ref_getfields.insert(pc)
                        {
                            changed = true;
                        }
                        if checking {
                            match receiver {
                                Elem::Obj(tid) => {
                                    site_obj.insert(pc, tid);
                                }
                                // Registered drawing handles are host objects.
                                Elem::Opaque => {}
                                _ => flow_ok = gate_fail(pc, "GetField receiver unproven"),
                            }
                        }
                        stack.push(field);
                    }
                    CsilInstr::CallCollection { func, argc } => {
                        let pops = *argc as usize;
                        let mut popped = Vec::with_capacity(pops);
                        for _ in 0..pops {
                            popped.push(stack.pop().unwrap_or(Elem::Mixed));
                        }
                        // The receiver, when the function takes one, is the
                        // DEEPEST operand (= last popped).
                        let takes_receiver = func
                            .operand_reqs(*argc)
                            .last()
                            .is_some_and(|req| matches!(req, CollectionAbstract::Ref));
                        let receiver = if takes_receiver {
                            popped.last().copied().unwrap_or(Elem::Mixed)
                        } else {
                            Elem::Mixed
                        };
                        if takes_receiver {
                            let prev = site_elem.get(&pc).copied().unwrap_or(Elem::Unset);
                            let joined = prev.join(receiver);
                            if joined != prev {
                                site_elem.insert(pc, joined);
                                changed = true;
                            }
                        }
                        if checking {
                            // Gate: the (func, argc) shape must be native and
                            // the lane type provable (constructors carry it;
                            // receiver ops need a proven receiver; the float
                            // aggregates need a numeric lane).
                            match native_coll_op(*func, *argc, pushes_literal_int(code, pc, 0)) {
                                None => {
                                    flow_ok = gate_fail(pc, &format!("non-native {func:?}/{argc}"));
                                }
                                Some(NativeCollOp::NewArray | NativeCollOp::FromArgs) => {}
                                Some(op) => {
                                    match site_elem.get(&pc).copied().unwrap_or(Elem::Unset) {
                                        Elem::Known(e) if native_lane(e) => {
                                            let aggregate = matches!(
                                                op,
                                                NativeCollOp::Sum
                                                    | NativeCollOp::Avg
                                                    | NativeCollOp::Min
                                                    | NativeCollOp::Max
                                            );
                                            if aggregate && e == VTy::Bool3 {
                                                flow_ok = gate_fail(pc, "bool aggregate");
                                            }
                                        }
                                        // Lane-unknown native arrays accept
                                        // the lane-independent receivers.
                                        Elem::AnyLane
                                            if matches!(
                                                op,
                                                NativeCollOp::Size | NativeCollOp::Clear
                                            ) =>
                                        {
                                            site_any.insert(pc);
                                        }
                                        _ => {
                                            flow_ok = gate_fail(pc, "receiver lane unproven");
                                        }
                                    }
                                }
                            }
                        }
                        // Result: constructors know their element type;
                        // same-elem copies inherit the receiver's. An
                        // ELEMENT result from a scalar lane is a plain value
                        // (never a heap handle); from a Ref lane it is a
                        // handle of untracked class.
                        // map element types: map projections first.
                        let map_result = if matches!(func, pinewasm_csil::CollectionFunc::MapNew) {
                            Some(map_new_elem(program, pc))
                        } else {
                            map_result_elem(*func, receiver)
                        };
                        let result = if let Some(result) = map_result {
                            result
                        } else {
                            match func.result() {
                                CollectionAbstract::Elem => match receiver {
                                    Elem::ObjectArray(tid) => Elem::Obj(tid),
                                    Elem::Known(VTy::Ref) => Elem::Mixed,
                                    Elem::Known(_) => Elem::Opaque,
                                    Elem::Unset => Elem::Unset,
                                    other => other,
                                },
                                CollectionAbstract::Ref => {
                                    let fresh = if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayNewObject
                                    ) {
                                        match code.get(pc.wrapping_sub(1)) {
                                            Some(CsilInstr::PushI64(tag)) if *tag >= 1000 => {
                                                u32::try_from(*tag - 1000)
                                                    .ok()
                                                    .filter(|tid| {
                                                        (*tid as usize) < program.object_types.len()
                                                    })
                                                    .map_or(
                                                        Elem::Known(VTy::Ref),
                                                        Elem::ObjectArray,
                                                    )
                                            }
                                            _ => Elem::Known(VTy::Ref),
                                        }
                                    } else {
                                        constructor_elem(*func)
                                    };
                                    if fresh != Elem::Mixed {
                                        fresh
                                    } else if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayFrom
                                    ) {
                                        // Native array, lane resolved by the
                                        // typed pass (docs/aot.md §13).
                                        Elem::AnyLane
                                    } else if matches!(
                                        func,
                                        pinewasm_csil::CollectionFunc::ArrayCopy
                                            | pinewasm_csil::CollectionFunc::ArraySlice
                                    ) {
                                        receiver
                                    } else {
                                        Elem::Mixed
                                    }
                                }
                                // Scalar results (size, sum, includes, …) can
                                // never be heap handles.
                                _ => Elem::Opaque,
                            }
                        };
                        stack.push(result);
                    }
                    // The registration site pins the constructed object's
                    // type as host-backing: the drawing registry, GC, and
                    // serializers read it on the interpreter VM heap. The
                    // fixpoint re-runs NewObject with the pin, so the
                    // checking round sees these operands as Opaque.
                    CsilInstr::RegisterDrawing { .. } => {
                        let obj = stack.pop().unwrap_or(Elem::Mixed);
                        if let Elem::Obj(tid) = obj {
                            if host_types.insert(tid) {
                                changed = true;
                            }
                        } else if checking && obj.maybe_native() {
                            flow_ok = gate_fail(pc, "RegisterDrawing operand unproven");
                        }
                        stack.push(Elem::Opaque);
                    }
                    other => {
                        let (pops, pushes) = match effect(other) {
                            Ok(effect) => effect,
                            Err(error) => {
                                flow_ok = gate_fail(pc, &format!("invalid stack effect: {error}"));
                                break;
                            }
                        };
                        // Residual host sites (and every other non-heap
                        // opcode) must never consume a possible native-heap
                        // handle. Dead pops and ref identity are native and
                        // handle-safe.
                        let handle_safe =
                            matches!(other, CsilInstr::Pop | CsilInstr::CmpEq | CsilInstr::CmpNe);
                        for _ in 0..pops {
                            let cell = stack.pop().unwrap_or(Elem::Mixed);
                            if checking && !handle_safe && cell.maybe_native() {
                                flow_ok =
                                    gate_fail(pc, &format!("native handle at non-heap {other:?}"));
                            }
                        }
                        for _ in 0..pushes {
                            // Non-heap opcodes never produce native handles
                            // (host refs — drawings, footprints — are opaque
                            // to the native heap).
                            stack.push(Elem::Opaque);
                        }
                    }
                }
            }
            let succs: Vec<usize> = match term[b] {
                Term::Fall(s) | Term::Jump(s) => vec![s],
                Term::Branch { taken, fall } => vec![taken, fall],
                Term::Return => vec![],
            };
            for s in succs {
                match &mut entry[s] {
                    slot @ None => {
                        *slot = Some(stack.clone());
                        changed = true;
                    }
                    Some(prev) => {
                        if prev.len() != stack.len() {
                            // Malformed input; the typed pass rejects it.
                            return ElemFacts {
                                field_vty: BTreeMap::new(),
                                site_elem: BTreeMap::new(),
                                site_map: BTreeMap::new(),
                                ref_getfields: BTreeSet::new(),
                                site_obj: BTreeMap::new(),
                                site_any: BTreeSet::new(),
                                host_types: BTreeSet::new(),
                                gate: HeapGate {
                                    any_heap,
                                    flow_ok: false,
                                },
                                gc_cells: GcCells::default(),
                            };
                        }
                        for (p, c) in prev.iter_mut().zip(&stack) {
                            let joined = p.join(*c);
                            if joined != *p {
                                *p = joined;
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if checking {
            break;
        }
        round += 1;
        if !changed {
            checking = true;
        } else if round >= max_rounds {
            // Not converged (should not happen on a finite lattice): keep
            // the phase C behavior rather than judge the gate on a moving
            // state.
            flow_ok = false;
            checking = true;
        }
    }

    ElemFacts {
        field_vty: BTreeMap::new(),
        // map element types
        site_map: site_map_facts(&site_elem),
        site_elem: site_elem
            .into_iter()
            .filter_map(|(pc, elem)| match elem {
                Elem::Known(t) => Some((pc, t)),
                Elem::ObjectArray(_) => Some((pc, VTy::Ref)),
                _ => None,
            })
            .collect(),
        ref_getfields,
        site_obj,
        site_any,
        host_types,
        gate: HeapGate { any_heap, flow_ok },
        gc_cells: GcCells::default(),
    }
}

// ---- generators ----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn any_target(rng: &mut Rng, len: usize) -> u32 {
    // Mostly in range; sometimes exactly `len` or past it.
    if rng.below(20) == 0 {
        (len + rng.below(3) as usize) as u32
    } else {
        rng.below(len as u64) as u32
    }
}

fn any_code(rng: &mut Rng) -> Vec<CsilInstr> {
    let len = 1 + rng.below(28) as usize;
    let mut code = Vec::with_capacity(len);
    for _ in 0..len {
        let instr = match rng.below(10) {
            0 | 1 => CsilInstr::Jump {
                target: any_target(rng, len),
            },
            2 | 3 => CsilInstr::JumpIfFalse {
                target: any_target(rng, len),
            },
            4 => CsilInstr::JumpIfExact {
                on: rng.below(2) == 0,
                target: any_target(rng, len),
            },
            5 => CsilInstr::JumpIfSlotWritten {
                slot: 0,
                target: any_target(rng, len),
            },
            6 => CsilInstr::Return,
            7 => CsilInstr::PushNa,
            _ => CsilInstr::Pop,
        };
        code.push(instr);
    }
    // Usually end in Return so most programs pass block recovery.
    if rng.below(8) != 0 {
        *code.last_mut().unwrap() = CsilInstr::Return;
    }
    code
}

fn corpus() -> Vec<CsilProgram> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in [
        "tests/compile-pass",
        "tests/regression",
        "benchmarks/workloads",
    ] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "pine"))
            .collect();
        paths.sort();
        for path in paths {
            let source = std::fs::read_to_string(&path).expect("read corpus file");
            let result =
                pinewasm_compiler::compile(&source, pinewasm_compiler::CompileOptions::default());
            if let Some(program) = result.program {
                out.push(program);
            }
        }
    }
    out
}

fn facts_eq(a: &ElemFacts, b: &ElemFacts) -> bool {
    a.site_elem == b.site_elem
        && a.site_map == b.site_map
        && a.ref_getfields == b.ref_getfields
        && a.site_obj == b.site_obj
        && a.site_any == b.site_any
        && a.host_types == b.host_types
        && a.gate == b.gate
}

fn check_cfg_agrees(code: &[CsilInstr]) -> &'static str {
    let old = old_cfg(code);
    let new = recover_cfg(code);
    match (old, new) {
        (Err(o), Err(n)) => {
            let n = match n {
                crate::AotError::InvalidProgram(m) => m,
                crate::AotError::IrreducibleControlFlow => "irreducible".to_string(),
                other => panic!("unexpected error {other:?}"),
            };
            assert_eq!(o, n, "{code:?}");
            if o == "irreducible" {
                "irreducible"
            } else {
                "invalid"
            }
        }
        (Ok(o), Ok(n)) => {
            let spans = |bs: &[Block]| bs.iter().map(|b| (b.start, b.end)).collect::<Vec<_>>();
            assert_eq!(spans(&o.blocks), spans(&n.blocks), "{code:?}");
            assert_eq!(o.term, n.term, "{code:?}");
            assert_eq!(o.rpo_order, n.rpo_order, "{code:?}");
            assert_eq!(o.rpo, n.rpo, "{code:?}");
            assert_eq!(o.idom, n.idom, "{code:?}");
            assert_eq!(o.loop_header, n.loop_header, "{code:?}");
            assert_eq!(o.merge, n.merge, "{code:?}");
            assert_eq!(o.in_loop, n.in_loop, "{code:?}");
            assert_eq!(o.block_of, n.block_of_pc, "{code:?}");
            if n.loop_header.iter().any(|&h| h) {
                "loop"
            } else {
                "acyclic"
            }
        }
        (o, n) => panic!(
            "old {:?} vs new {:?} on {code:?}",
            o.map(|_| ()),
            n.map(|_| ())
        ),
    }
}

// ---- tests ----

/// Random jump-heavy bodies: the kernel recovers exactly the same
/// blocks, terminators, rpo, dominators, loop headers, merges and loop
/// membership, and fails with the same error.
#[test]
fn cfg_kernel_matches_baseline() {
    let mut rng = Rng(0x243f_6a88_85a3_08d3);
    let mut seen: BTreeMap<&'static str, usize> = BTreeMap::new();
    for _ in 0..60_000 {
        let code = any_code(&mut rng);
        *seen.entry(check_cfg_agrees(&code)).or_default() += 1;
    }
    for kind in ["irreducible", "invalid", "loop", "acyclic"] {
        assert!(
            seen.get(kind).copied().unwrap_or(0) > 1_000,
            "outcome {kind} under-exercised: {seen:?}"
        );
    }
}

/// Every compiled corpus program: same CFG facts and, over them, the same
/// collection-element facts from the hand-written round loop and the generic
/// driver.
#[test]
fn corpus_cfg_and_elem_facts_match_baseline() {
    let programs = corpus();
    assert!(programs.len() >= 40, "corpus too small: {}", programs.len());
    let mut heap = 0;
    for program in &programs {
        let code = &program.main().code;
        check_cfg_agrees(code);
        let parts = recover_cfg(code).expect("compiled programs are reducible");
        let old = old_infer_collection_elems(program, &parts.blocks, &parts.term, &parts.rpo_order);
        let new = infer_collection_elems(
            program,
            &parts.blocks,
            &parts.term,
            &parts.rpo_order,
            &BTreeMap::new(),
        );
        assert!(facts_eq(&old, &new), "elem facts differ");
        if new.gate.any_heap {
            heap += 1;
        }
    }
    assert!(heap >= 5, "too few heap programs: {heap}");
}

/// Mutated corpus programs (retargeted jumps, swapped and dropped
/// instructions): the baseline and the production code agree on malformed
/// states and on panics.
#[test]
fn mutated_corpus_matches_baseline() {
    let programs = corpus();
    let mut rng = Rng(0x1319_8a2e_0370_7344);
    let mut compared = 0;
    let mut malformed = 0;
    for program in &programs {
        for _ in 0..40 {
            let mut p = program.clone();
            let code = &mut p.functions[0].code;
            let len = code.len();
            if len < 2 {
                continue;
            }
            for _ in 0..=rng.below(3) {
                let i = rng.below(len as u64) as usize;
                let j = rng.below(len as u64) as usize;
                match rng.below(3) {
                    0 => code.swap(i, j),
                    1 => {
                        if let CsilInstr::Jump { target }
                        | CsilInstr::JumpIfFalse { target }
                        | CsilInstr::JumpIfExact { target, .. }
                        | CsilInstr::JumpIfSlotWritten { target, .. } = &mut code[i]
                        {
                            *target = j as u32;
                        }
                    }
                    _ => code[i] = CsilInstr::Pop,
                }
            }
            let code = &p.main().code;
            let Ok(parts) = recover_cfg(code) else {
                check_cfg_agrees(code);
                continue;
            };
            check_cfg_agrees(code);
            let old = std::panic::catch_unwind(|| {
                old_infer_collection_elems(&p, &parts.blocks, &parts.term, &parts.rpo_order)
            });
            let new = std::panic::catch_unwind(|| {
                infer_collection_elems(
                    &p,
                    &parts.blocks,
                    &parts.term,
                    &parts.rpo_order,
                    &BTreeMap::new(),
                )
            });
            match (old, new) {
                (Ok(o), Ok(n)) => {
                    assert!(facts_eq(&o, &n), "elem facts differ on a mutant");
                    if !n.gate.flow_ok && n.site_obj.is_empty() && n.host_types.is_empty() {
                        malformed += 1;
                    }
                    compared += 1;
                }
                (Err(_), Err(_)) => {}
                (o, n) => panic!("old ok={} new ok={}", o.is_ok(), n.is_ok()),
            }
        }
    }
    assert!(compared > 500, "too few mutants compared: {compared}");
    assert!(malformed > 0);
}

/// The generic driver on a scripted body: stops after exactly one checking
/// round, counts ordinary rounds, and reports non-convergence at the cap.
#[test]
fn run_fixpoint_protocol() {
    use super::fixpoint::{run_fixpoint, FixpointRound, RoundEnd};
    struct Script {
        ends: Vec<RoundEnd>,
        calls: Vec<bool>,
    }
    impl FixpointRound for Script {
        fn round(&mut self, checking: bool) -> RoundEnd {
            self.calls.push(checking);
            if self.ends.is_empty() {
                RoundEnd::Stable
            } else {
                self.ends.remove(0)
            }
        }
    }
    let mut s = Script {
        ends: vec![RoundEnd::Changed, RoundEnd::Changed, RoundEnd::Stable],
        calls: vec![],
    };
    let o = run_fixpoint(&mut s, 10);
    assert!(o.converged && !o.malformed && o.rounds == 3);
    assert_eq!(s.calls, vec![false, false, false, true]);
    let mut s = Script {
        ends: vec![RoundEnd::Changed; 20],
        calls: vec![],
    };
    let o = run_fixpoint(&mut s, 4);
    assert!(!o.converged && !o.malformed && o.rounds == 4);
    assert_eq!(s.calls, vec![false, false, false, false, true]);
    let mut s = Script {
        ends: vec![RoundEnd::Changed, RoundEnd::Malformed],
        calls: vec![],
    };
    let o = run_fixpoint(&mut s, 4);
    assert!(o.malformed && o.rounds == 1);
}

/// The production round body does not guarantee that a `Stable` round leaves
/// the state unchanged. An ordinary round can report `Stable` and still change
/// the state: `NewObject`/`SetField` create a `field_elems` key with
/// `or_insert(Unset)` and do not set `changed` when the joined value stays
/// `Unset` (while `GetField` reads a missing key as `Mixed`). This test pins
/// that behavior so it cannot silently drift; the baseline implementation
/// above has the same arms.
#[test]
fn elem_round_stable_round_can_change_state() {
    use super::fixpoint::{FixpointRound, RoundEnd};
    use super::ElemRound;
    let mut program = base_fixture();
    program.functions[0].code = vec![
        CsilInstr::PushNa,
        CsilInstr::NewObject {
            type_id: 0,
            field_count: 1,
        },
        CsilInstr::Pop,
        CsilInstr::Return,
    ];
    let code = program.main().code.clone();
    let parts = recover_cfg(&code).expect("single block");
    let n = parts.blocks.len();
    let mut entry: Vec<Option<Vec<Elem>>> = vec![None; n];
    entry[0] = Some(Vec::new());
    let mut state = ElemRound {
        program: &program,
        blocks: &parts.blocks,
        term: &parts.term,
        rpo_order: &parts.rpo_order,
        entry,
        slot_elems: vec![Elem::Unset; program.series_slots.len()],
        field_elems: BTreeMap::new(),
        site_elem: BTreeMap::new(),
        ref_getfields: BTreeSet::new(),
        site_obj: BTreeMap::new(),
        site_any: BTreeSet::new(),
        host_types: BTreeSet::new(),
        from_lanes: &BTreeMap::new(),
        flow_ok: true,
        native_into_host: false,
    };
    assert!(state.field_elems.is_empty());
    assert_eq!(state.round(false), RoundEnd::Stable);
    // The Stable round added the key (0, 0) bound to Unset.
    assert_eq!(state.field_elems.get(&(0, 0)).copied(), Some(Elem::Unset));
}

fn base_fixture() -> CsilProgram {
    pinewasm_compiler::compile(
        "indicator('review')\nplot(close)\n",
        pinewasm_compiler::CompileOptions::default(),
    )
    .program
    .expect("fixture compiles")
}

#[allow(dead_code)]
fn _uses(_: HeapGate, _: NativeCollOp, _: VTy, _: BTreeSet<usize>, _: Elem) {
    let _ = (
        gate_fail,
        native_coll_op,
        native_lane,
        pushes_literal_int,
        constructor_elem,
    );
}
