//! Conservative integer-field domains from every native constructor/write.
//! Copies preserve cells; aliases cannot invalidate a type-wide proof because
//! all writes to that type/field participate, regardless of receiver identity.
use super::*;

fn input_fact(program: &CsilProgram, input: u32, boundary: IntInputBoundary) -> IntFact {
    let Some(decl) = program.inputs.iter().find(|decl| decl.id == input) else {
        return IntFact::Unknown;
    };
    if !matches!(decl.kind, InputKind::Int | InputKind::Time) {
        return IntFact::Unknown;
    }
    let mut min = match decl.constraints.min {
        Some(InputValue::Int(value)) => value,
        _ if boundary == IntInputBoundary::RejectsSentinel => i64::MIN + 1,
        _ => i64::MIN,
    };
    let mut max = match decl.constraints.max {
        Some(InputValue::Int(value)) => value,
        _ => i64::MAX,
    };
    if !decl.constraints.options.is_empty() {
        let mut lo = i64::MAX;
        let mut hi = i64::MIN;
        for option in &decl.constraints.options {
            let InputValue::Int(value) = option else {
                return IntFact::Unknown;
            };
            lo = lo.min(*value);
            hi = hi.max(*value);
        }
        min = min.max(lo);
        max = max.min(hi);
    }
    if min > i64::MIN && min <= max {
        IntFact::Range(min, max)
    } else {
        IntFact::Unknown
    }
}

pub(super) fn proven_field_ranges(
    program: &CsilProgram,
    blocks: &[Block],
    typed: &TypedOut,
    heap: Option<&HeapPlan>,
    boundary: IntInputBoundary,
) -> BTreeMap<usize, IntFact> {
    let Some(heap) = heap else {
        return BTreeMap::new();
    };
    let mut fields: BTreeMap<(u32, u32), IntFact> = BTreeMap::new();
    let mut observe = |tid: u32, field: u32, fact: IntFact| {
        fields
            .entry((tid, field))
            .and_modify(|old| *old = old.combine(fact))
            .or_insert(fact);
    };
    for (block_id, block) in blocks.iter().enumerate() {
        // Cross-block operands have no provenance in this proof. In
        // particular, a conditional write cannot inherit a favorable fact
        // from the preceding block's textual order.
        let mut stack: Vec<_> = typed.entry_stack[block_id]
            .iter()
            .map(|ty| {
                if *ty == VTy::I64 {
                    IntFact::Unknown
                } else {
                    IntFact::NonInt
                }
            })
            .collect();
        for pc in block.start..block.end {
            let instr = &program.main().code[pc];
            let Ok(effect) = instr.resolve_stack_effect(&program.ta_states) else {
                return BTreeMap::new();
            };
            let pops = effect.pops as usize;
            let pushes = effect.pushes as usize;
            if stack.len() < pops {
                return BTreeMap::new();
            }
            let base = stack.len() - pops;
            match instr {
                CsilInstr::NewObject {
                    type_id,
                    field_count,
                } if !heap.host_types.contains(type_id) => {
                    for field in 0..*field_count {
                        if heap.field_ty(*type_id, field) == VTy::I64 {
                            // Native I64 storage has I64 operands: numeric
                            // mixed writes instead widen the layout to F64.
                            let fact =
                                if typed.operand_tys[pc].get(field as usize) == Some(&VTy::I64) {
                                    stack
                                        .get(base + field as usize)
                                        .copied()
                                        .unwrap_or(IntFact::Unknown)
                                } else {
                                    IntFact::Unknown
                                };
                            observe(*type_id, field, fact);
                        }
                    }
                }
                CsilInstr::SetField { field_idx } => {
                    if let Some(&tid) = heap.site_obj.get(&pc) {
                        if !heap.host_types.contains(&tid)
                            && heap.field_ty(tid, *field_idx) == VTy::I64
                        {
                            let fact = if typed.operand_tys[pc].get(1) == Some(&VTy::I64) {
                                stack.get(base + 1).copied().unwrap_or(IntFact::Unknown)
                            } else {
                                IntFact::Unknown
                            };
                            observe(tid, *field_idx, fact);
                        }
                    }
                }
                _ => {}
            }
            let op = match instr {
                CsilInstr::ReadInput { input }
                    if typed.result_tys[pc].first() == Some(&VTy::I64) =>
                {
                    match input_fact(program, *input, boundary) {
                        IntFact::Range(lo, hi) => IntOp::PushRange(lo, hi),
                        _ => IntOp::Other,
                    }
                }
                _ => int_op(instr),
            };
            if int_fact_step(&mut stack, op, pops, pushes, &typed.result_tys[pc]) {
                // A failed arithmetic proof invalidates its facts, not the
                // entire compile. The producer gate makes the final verdict.
                stack.clear();
                stack.resize(base + pushes, IntFact::Unknown);
            }
        }
    }
    let mut reads = BTreeMap::new();
    for (&pc, &tid) in &heap.site_obj {
        if heap.host_types.contains(&tid) {
            continue;
        }
        let CsilInstr::GetField { field_idx } = program.main().code[pc] else {
            continue;
        };
        if typed.result_tys[pc].first() != Some(&VTy::I64) {
            continue;
        }
        match fields.get(&(tid, field_idx)) {
            Some(fact @ IntFact::NaOnly) => {
                reads.insert(pc, *fact);
            }
            Some(fact @ IntFact::Range(lo, _)) if *lo > i64::MIN => {
                reads.insert(pc, *fact);
            }
            _ => {}
        }
    }
    reads
}
