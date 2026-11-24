//! Differential test of the typed worklist pass (`typed_stacks` and the
//! rendering glue in `validate_function`) against a copy of its abstract
//! interpretation, run on every function the unit tests validate.

use super::{
    abstract_ty_merge, collection_operand_reqs, collection_result_ty, count_htf_history_nodes,
    count_htf_ta_nodes, height_successors, instruction_offset, order_operand_reqs,
    pure_operand_reqs, pure_result_ty, validate_htf_expr, AbstractTy, ProgramTables, Req,
    TypedTables, ValidationError,
};
use crate::{CsilFunction, CsilInstr, CsilProgram, PureFunc};
use pinewasm_hir::{InputKind, TaOperand, TaResult};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Number of functions compared (so a test can assert the check ran).
pub(super) static COMPARED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Per-thread count of comparisons that reached the typed pass, and of
    /// those whose copy-pass verdict was a rejection (the random test below
    /// asserts both are exercised, independently of parallel tests).
    static LOCAL: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Copy of the typed pass used for the differential check. Early returns are
/// changed to `return (errors, None);`, and the final `max_stack` comparison is
/// left to the caller (`Some(max_stack_seen)` on completion).
#[allow(clippy::too_many_lines)]
fn typed_pass_copy(
    program: &CsilProgram,
    func: &CsilFunction,
    name: &str,
) -> (Vec<ValidationError>, Option<u32>) {
    let name = name.to_string();
    let code_len = func.code.len();
    let mut errors: Vec<ValidationError> = Vec::new();
    // Abstract interpretation over all paths.
    let mut states: Vec<Option<Vec<AbstractTy>>> = vec![None; code_len];
    let mut worklist = vec![0usize];
    states[0] = Some(Vec::new());
    let mut max_stack_seen: u32 = 0;

    let fail = |offset: usize, message: String| ValidationError::AtInstruction {
        function: name.clone(),
        offset: instruction_offset(offset),
        message,
    };

    while let Some(pc) = worklist.pop() {
        let mut stack = states[pc]
            .clone()
            .expect("worklist entries always have state");
        let instr = &func.code[pc];

        // Pops with type checks. CALL_TA's pop count comes from the
        // ta_states table (the instruction alone cannot know it).
        let pops = match instr.resolve_stack_effect(&program.ta_states) {
            Ok(effect) => effect.pops,
            Err(error) => {
                errors.push(fail(pc, format!("cannot resolve stack effect: {error}")));
                return (errors, None);
            }
        };
        let Ok(pops_usize) = usize::try_from(pops) else {
            errors.push(fail(
                pc,
                format!("operand count {pops} exceeds this target's index width"),
            ));
            return (errors, None);
        };
        if stack.len() < pops_usize {
            errors.push(fail(
                pc,
                format!(
                    "stack underflow: needs {pops} operand(s), stack has {}",
                    stack.len()
                ),
            ));
            return (errors, None);
        }
        // CALL_PURE / CALL_TA operand requirements depend on the function,
        // so they are built dynamically; everything else uses static
        // slices.
        let dynamic_reqs: Vec<Req>;
        let required: &[Req] = match instr {
            CsilInstr::CallPure { func, argc } => {
                dynamic_reqs = pure_operand_reqs(*func, *argc);
                &dynamic_reqs
            }
            CsilInstr::CallCollection { func, argc } => {
                dynamic_reqs = collection_operand_reqs(*func, *argc);
                &dynamic_reqs
            }
            CsilInstr::Order { kind } => {
                dynamic_reqs = order_operand_reqs(*kind);
                &dynamic_reqs
            }
            // NEW_OBJECT pops `field_count` field values (any type each).
            CsilInstr::NewObject { field_count, .. } => {
                dynamic_reqs = vec![Req::AnyValue; *field_count as usize];
                &dynamic_reqs
            }
            // COPY_OBJECT / GET_FIELD pop one object handle (a Ref).
            CsilInstr::CopyObject { .. } | CsilInstr::GetField { .. } => {
                &[Req::Exact(AbstractTy::Ref)]
            }
            // FOOTPRINT_ROW_BY_PRICE pops the price (top, numeric) then the
            // footprint handle. The receiver may be `na` at runtime (structured
            // error), so accept any value there and let the VM raise it.
            CsilInstr::FootprintRowByPrice => &[Req::Numeric, Req::AnyValue],
            // SET_FIELD pops the value (top, any type) then the object handle.
            CsilInstr::SetField { .. } => &[Req::AnyValue, Req::Exact(AbstractTy::Ref)],
            // REGISTER_DRAWING / DELETE_DRAWING pop one drawing-object handle
            // (a Ref). REGISTER accepts `na` too? No — construction always
            // yields a handle; a `na` here would be an invalid program.
            CsilInstr::RegisterDrawing { .. } => &[Req::Exact(AbstractTy::Ref)],
            // READ_DRAWINGS_ALL takes no operand (it builds a fresh array).
            CsilInstr::ReadDrawingsAll { .. } => &[],
            // DELETE_DRAWING pops one handle; the receiver may be `na` at
            // runtime (a no-op in the VM), so accept any
            // value here.
            CsilInstr::DeleteDrawing { .. } => &[Req::AnyValue],
            // SET_CELL pops (top-first): each attribute value (any type), then
            // row (int), column (int), then the table handle. The receiver may
            // be `na` at runtime (structured error), so accept any value there.
            CsilInstr::SetCell { attrs } => {
                let mut reqs = vec![Req::AnyValue; attrs.len()];
                // row (int), column (int), then the table handle.
                reqs.push(Req::Numeric);
                reqs.push(Req::Numeric);
                reqs.push(Req::AnyValue);
                dynamic_reqs = reqs;
                &dynamic_reqs
            }
            // CLEAR_CELLS / MERGE_CELLS pop (top-first) end_row, end_column,
            // start_row, start_column (all int), then the table handle.
            CsilInstr::ClearCells | CsilInstr::MergeCells => &[
                Req::Numeric,
                Req::Numeric,
                Req::Numeric,
                Req::Numeric,
                Req::AnyValue,
            ],
            // LOG pops the format string plus its substitution args (any scalar
            // each; the format string, deepest, is guaranteed Str by the
            // checker). Requirements are top-first — all `AnyValue`.
            CsilInstr::Log { argc, .. } => {
                dynamic_reqs = vec![Req::AnyValue; *argc as usize];
                &dynamic_reqs
            }
            // ALERT pops freq (top) then message (both strings; freq may be na).
            CsilInstr::Alert => &[Req::Exact(AbstractTy::Str), Req::Exact(AbstractTy::Str)],
            // RUNTIME_ERROR pops one string message (may be na).
            CsilInstr::RuntimeError => &[Req::Exact(AbstractTy::Str)],
            // ALERT_CONDITION pops message (top), title, then a bool condition.
            CsilInstr::AlertCondition => &[
                Req::Exact(AbstractTy::Str),
                Req::Exact(AbstractTy::Str),
                Req::Exact(AbstractTy::Bool),
            ],
            // Requirements are listed top-of-stack first: reverse the
            // argument-order operand table (last operand on top).
            CsilInstr::CallTa { state } => {
                dynamic_reqs = program.ta_states[*state as usize]
                    .func
                    .engine_operands()
                    .iter()
                    .rev()
                    .map(|op| match op {
                        TaOperand::Num => Req::Exact(AbstractTy::F64),
                        TaOperand::Cond => Req::Exact(AbstractTy::Bool),
                        TaOperand::Bool => Req::Exact(AbstractTy::Bool),
                        TaOperand::Color => Req::Exact(AbstractTy::Color),
                    })
                    .collect();
                &dynamic_reqs
            }
            CsilInstr::PlotStyledF64 { color_count, .. } => {
                dynamic_reqs = (0..*color_count)
                    .map(|_| Req::Exact(AbstractTy::Color))
                    .chain(std::iter::once(Req::Exact(AbstractTy::F64)))
                    .collect();
                &dynamic_reqs
            }
            CsilInstr::FillColor { fill_slot, .. } => {
                dynamic_reqs = ProgramTables { program }.fill_reqs(*fill_slot);
                &dynamic_reqs
            }
            CsilInstr::VisualColor { color_count, .. } => {
                dynamic_reqs = (0..*color_count)
                    .map(|_| Req::Exact(AbstractTy::Color))
                    .collect();
                &dynamic_reqs
            }
            CsilInstr::F64Add
            | CsilInstr::F64Sub
            | CsilInstr::F64Mul
            | CsilInstr::F64Div
            | CsilInstr::F64Rem => &[Req::Exact(AbstractTy::F64), Req::Exact(AbstractTy::F64)],
            CsilInstr::IntAdd
            | CsilInstr::IntSub
            | CsilInstr::IntMul
            | CsilInstr::IntDiv
            | CsilInstr::PineIntDiv
            | CsilInstr::IntRem => &[Req::Exact(AbstractTy::Int), Req::Exact(AbstractTy::Int)],
            // Comparisons are numeric-polymorphic (the VM promotes a mixed
            // int/float pair, though the type checker never emits one).
            CsilInstr::CmpGt | CsilInstr::CmpGe | CsilInstr::CmpLt | CsilInstr::CmpLe => {
                &[Req::Numeric, Req::Numeric]
            }
            CsilInstr::F64Neg | CsilInstr::PlotF64 { .. } => &[Req::Exact(AbstractTy::F64)],
            // The VM accepts numeric offsets and lengths and truncates them at runtime.
            CsilInstr::ReadBuiltinDynamic { .. }
            | CsilInstr::ReadUserSlotDynamic { .. }
            | CsilInstr::WindowMeanDynamic { .. }
            | CsilInstr::WindowExtremumDynamic { .. } => &[Req::Numeric],
            CsilInstr::WindowStatisticDynamic { .. } => &[Req::Exact(AbstractTy::Int)],
            CsilInstr::EmaSimpleDynamic { .. } => {
                &[Req::Exact(AbstractTy::Int), Req::Exact(AbstractTy::F64)]
            }
            CsilInstr::IntNeg | CsilInstr::IntToF64 => &[Req::Exact(AbstractTy::Int)],
            CsilInstr::BoolAnd | CsilInstr::BoolOr => {
                &[Req::Exact(AbstractTy::Bool), Req::Exact(AbstractTy::Bool)]
            }
            CsilInstr::StrConcat => &[Req::Exact(AbstractTy::Str), Req::Exact(AbstractTy::Str)],
            CsilInstr::BoolNot | CsilInstr::JumpIfFalse { .. } | CsilInstr::JumpIfExact { .. } => {
                &[Req::Exact(AbstractTy::Bool)]
            }
            // SELECT pops rhs, lhs (any value types), then a bool condition.
            CsilInstr::Select => &[Req::AnyValue, Req::AnyValue, Req::Exact(AbstractTy::Bool)],
            // Equality accepts any matching operand pair (checked below;
            // the VM re-checks dynamically).
            _ => &[],
        };
        for (slot_from_top, req) in required.iter().enumerate() {
            let ty = stack[stack.len() - 1 - slot_from_top];
            if !req.accepts(ty) {
                errors.push(fail(
                    pc,
                    format!(
                        "operand {} must be {}, found {ty:?}",
                        slot_from_top + 1,
                        req.describe()
                    ),
                ));
                return (errors, None);
            }
        }
        // Pop, remembering the operand types (top of stack first).
        let mut popped: Vec<AbstractTy> = Vec::with_capacity(pops_usize);
        for _ in 0..pops {
            popped.push(stack.pop().expect("underflow checked above"));
        }
        if matches!(instr, CsilInstr::CmpEq | CsilInstr::CmpNe) {
            let (a, b) = (popped[0], popped[1]);
            if a != b && a != AbstractTy::Any && b != AbstractTy::Any {
                errors.push(fail(
                    pc,
                    format!("equality operands must have matching types, found {a:?} and {b:?}"),
                ));
                return (errors, None);
            }
        }
        // NZ's source/replacement pair is relational, like equality: the
        // replacement must have the source's type where statically known.
        if let CsilInstr::CallPure {
            func: PureFunc::Nz, ..
        } = instr
        {
            let (a, b) = (popped[0], popped[1]);
            if a != b && a != AbstractTy::Any && b != AbstractTy::Any {
                errors.push(fail(
                    pc,
                    format!("NZ operands must have matching types, found {a:?} and {b:?}"),
                ));
                return (errors, None);
            }
        }

        // Pushes.
        match instr {
                CsilInstr::PushNa => stack.push(AbstractTy::Any),
                CsilInstr::PushBool(_) => stack.push(AbstractTy::Bool),
                CsilInstr::PushI64(_) => stack.push(AbstractTy::Int),
                CsilInstr::PushF64(_) => stack.push(AbstractTy::F64),
                CsilInstr::PushStr(_) => stack.push(AbstractTy::Str),
                CsilInstr::PushColor { .. } => stack.push(AbstractTy::Color),
                CsilInstr::Dup => {
                    stack.push(popped[0]);
                    stack.push(popped[0]);
                }
                // `bar_index` and the epoch/bar-index time builtins are int
                // series; the `barstate.*` predicates are bool series; the OHLCV
                // series and derived averages are floats.
                CsilInstr::ReadBuiltin { source, .. } => {
                    stack.push(if source.is_bool() {
                        AbstractTy::Bool
                    } else if source.is_int() {
                        AbstractTy::Int
                    } else if source.is_string() {
                        AbstractTy::Str
                    } else {
                        AbstractTy::F64
                    });
                }
                // `request.security` over a float series pushes a float (or na on
                // gap / not-yet-available bars). Only float-producing sources
                // lower here (the analyzer rejects int/bool `expression`s).
                CsilInstr::ReadSecurity { .. } => stack.push(AbstractTy::F64),
                // A compound `request.security` secondary context preserves
                // its root result type; validate the HtfExpr state-slot bounds.
                CsilInstr::ReadSecurityExpr { expr, .. } => {
                    if let Err(msg) = validate_htf_expr(
                        expr,
                        count_htf_ta_nodes(expr),
                        count_htf_history_nodes(expr),
                        &program.inputs,
                    ) {
                        errors.push(fail(pc, msg));
                    }
                    stack.push(if expr.result_is_bool() {
                        AbstractTy::Bool
                    } else {
                        AbstractTy::F64
                    });
                }
                // A `request.*` interface read pushes a float (the host value or na).
                CsilInstr::ReadRequest { .. } => stack.push(AbstractTy::F64),
                // `request.footprint(...)` builds and pushes a footprint object
                // handle (a Ref), materialized from the optional host table.
                CsilInstr::BuildFootprint { .. } => stack.push(AbstractTy::Ref),
                // `request.security_lower_tf(...)` builds and pushes an
                // `array<float>` handle (a Ref) from the optional host table.
                CsilInstr::ReadSecurityLowerTf { .. } => stack.push(AbstractTy::Ref),
                // `ta.pivot_point_levels(...)` builds and pushes an `array<float>`
                // handle (a Ref) of the prior anchor period's pivot levels.
                CsilInstr::ReadPivotLevels { .. } | CsilInstr::CallPivotPointLevels { .. } => {
                    stack.push(AbstractTy::Ref);
                }
                // `time(timeframe)` / `time_close(timeframe)` push the enclosing
                // bucket's open/close time — a `series int` (epoch ms).
                CsilInstr::ReadTimeTf { .. } => stack.push(AbstractTy::Int),
                // The matching row handle, or `na` when none matches — so `Any`.
                CsilInstr::FootprintRowByPrice => stack.push(AbstractTy::Any),
                // User slots are dynamically typed storage.
                CsilInstr::ReadUserSlot { .. } => stack.push(AbstractTy::Any),
                // Dynamic-offset reads mirror the literal forms' result type: the
                // builtin's static type, or `Any` for a user slot. An out-of-range
                // or `na` offset yields `na` at run time (still that type slot).
                CsilInstr::ReadBuiltinDynamic { source } => {
                    stack.push(if source.is_bool() {
                        AbstractTy::Bool
                    } else if source.is_int() {
                        AbstractTy::Int
                    } else if source.is_string() {
                        AbstractTy::Str
                    } else {
                        AbstractTy::F64
                    });
                }
                CsilInstr::ReadUserSlotDynamic { .. } => stack.push(AbstractTy::Any),
                // An input read pushes the declaration's kind type.
                CsilInstr::ReadInput { input } => stack.push(match program.inputs[*input as usize].kind {
                    // An enum input reads its member's variant index (an int).
                    InputKind::Int | InputKind::Time | InputKind::Enum => AbstractTy::Int,
                    // input.source reads a series float sample.
                    InputKind::Float | InputKind::Source | InputKind::Price => AbstractTy::F64,
                    InputKind::Bool => AbstractTy::Bool,
                    InputKind::String => AbstractTy::Str,
                    InputKind::Color => AbstractTy::Color,
                }),
                CsilInstr::IntAdd
                | CsilInstr::IntSub
                | CsilInstr::IntMul
                | CsilInstr::IntDiv
                | CsilInstr::PineIntDiv
                | CsilInstr::IntRem
                | CsilInstr::IntNeg => stack.push(AbstractTy::Int),
                CsilInstr::F64Add
                | CsilInstr::F64Sub
                | CsilInstr::F64Mul
                | CsilInstr::F64Div
                | CsilInstr::F64Rem
                | CsilInstr::F64Neg
                | CsilInstr::IntToF64
                // A window mean is a float or na.
                | CsilInstr::WindowMean { .. }
                | CsilInstr::WindowMeanDynamic { .. }
                | CsilInstr::WindowExtremumDynamic { .. }
                | CsilInstr::EmaSimpleDynamic { .. } => stack.push(AbstractTy::F64),
                CsilInstr::WindowStatisticDynamic { func, .. } => {
                    stack.push(if func.engine_result() == pinewasm_hir::TaResult::Int {
                        AbstractTy::Int
                    } else {
                        AbstractTy::F64
                    });
                }
                CsilInstr::CallPure { func, argc } => {
                    stack.push(pure_result_ty(*func, *argc, &popped));
                }
                CsilInstr::CallCollection { func, .. } => stack.push(collection_result_ty(*func)),
                // NEW_OBJECT / COPY_OBJECT push an object handle (a Ref); GET_FIELD
                // pushes the field value (any type — the checker owns field typing);
                // SET_FIELD pushes nothing.
                CsilInstr::NewObject { .. } | CsilInstr::CopyObject { .. } => stack.push(AbstractTy::Ref),
                CsilInstr::GetField { .. } => stack.push(AbstractTy::Any),
                // REGISTER_DRAWING pushes the same object handle back; DELETE
                // pushes nothing.
                CsilInstr::RegisterDrawing { .. } => stack.push(AbstractTy::Ref),
                CsilInstr::ReadDrawingsAll { .. } => stack.push(AbstractTy::Ref),
                CsilInstr::CallTa { state } => {
                    let func = program.ta_states[*state as usize].func;
                    let ty = match func.engine_result() {
                        TaResult::Float => AbstractTy::F64,
                        TaResult::Int => AbstractTy::Int,
                        TaResult::Bool => AbstractTy::Bool,
                        TaResult::Color => AbstractTy::Color,
                    };
                    // Push one value per tuple element (scalar functions push
                    // one; tuple functions push their whole tuple, all of the same
                    // element type here).
                    for _ in 0..func.engine_result_arity() {
                        stack.push(ty);
                    }
                }
                // The result is one of the two branch values: merge their
                // abstract types (popped[0] is rhs, popped[1] is lhs).
                CsilInstr::Select => stack.push(abstract_ty_merge(popped[0], popped[1])),
                CsilInstr::CmpGt
                | CsilInstr::CmpGe
                | CsilInstr::CmpLt
                | CsilInstr::CmpLe
                | CsilInstr::CmpEq
                | CsilInstr::CmpNe
                | CsilInstr::BoolAnd
                | CsilInstr::BoolOr
                | CsilInstr::BoolNot => stack.push(AbstractTy::Bool),
                CsilInstr::StrConcat => stack.push(AbstractTy::Str),
                _ => {}
            }
        max_stack_seen = max_stack_seen.max(
            u32::try_from(stack.len()).expect("the preflighted abstract stack ceiling fits in u32"),
        );

        // Successors.
        if matches!(instr, CsilInstr::Return) {
            if !stack.is_empty() {
                errors.push(fail(
                    pc,
                    format!(
                        "operand stack must be empty at RETURN (has {} values)",
                        stack.len()
                    ),
                ));
                return (errors, None);
            }
            continue;
        }
        // The conditional jumps merge their two successor paths (target
        // first); JumpIfSlotWritten branches on VM slot state, not the stack.
        // The same successor function as the height preflight.
        let mut successors: Vec<usize> = Vec::new();
        let (first, second) = height_successors(instr, pc);
        successors.push(first);
        if let Some(fallthrough) = second {
            successors.push(fallthrough);
        }
        for succ in successors {
            if succ >= code_len {
                errors.push(fail(
                    pc,
                    "execution falls off the end of the function".to_string(),
                ));
                return (errors, None);
            }
            match &states[succ] {
                None => {
                    states[succ] = Some(stack.clone());
                    worklist.push(succ);
                }
                Some(existing) => {
                    if existing.len() != stack.len() {
                        errors.push(fail(
                            succ,
                            format!(
                                "inconsistent stack heights at merge point ({} vs {})",
                                existing.len(),
                                stack.len()
                            ),
                        ));
                        return (errors, None);
                    }
                    let merged: Vec<AbstractTy> = existing
                        .iter()
                        .zip(&stack)
                        .map(|(a, b)| abstract_ty_merge(*a, *b))
                        .collect();
                    if merged != *existing {
                        states[succ] = Some(merged);
                        worklist.push(succ);
                    }
                }
            }
        }
    }
    (errors, Some(max_stack_seen))
}

/// Asserts the typed pass reported exactly the copy's errors and
/// the same `max_stack` usage.
pub(super) fn assert_typed_matches_baseline(
    program: &CsilProgram,
    func: &CsilFunction,
    name: &str,
    typed_errors: &[ValidationError],
    typed_result: Result<u32, ()>,
) {
    let (baseline_errors, baseline_seen) = typed_pass_copy(program, func, name);
    assert_eq!(
        typed_errors,
        baseline_errors.as_slice(),
        "typed errors differ for `{name}`"
    );
    assert_eq!(
        typed_result.ok(),
        baseline_seen,
        "max_stack usage differs for `{name}`"
    );
    COMPARED.fetch_add(1, Ordering::Relaxed);
    let rejected = usize::from(baseline_seen.is_none() || !baseline_errors.is_empty());
    LOCAL.with(|c| {
        let (n, r) = c.get();
        c.set((n + 1, r + rejected));
    });
}

/// Random, mostly ill-typed code over the instruction palette of the
/// repository's own fixture programs (plus random jumps and returns), run
/// through `validate`; every function that reaches the typed pass is
/// compared against the copy of the typed pass by the hook above.
#[test]
fn random_code_typed_pass_matches_baseline() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut programs = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(root.join("tests/compile-pass"))
        .expect("fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pine"))
        .collect();
    paths.sort();
    for path in paths.iter().take(40) {
        let source = std::fs::read_to_string(path).expect("fixture source");
        let parsed = pinewasm_parse::parse(&source);
        if pinewasm_span::has_errors(&parsed.diagnostics) {
            continue;
        }
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        if pinewasm_span::has_errors(&analysis.diagnostics) {
            continue;
        }
        let mir = pinewasm_mir::lower(&analysis.hir);
        if let Ok(program) = crate::lower_mir(&mir) {
            programs.push(program);
        }
    }
    assert!(!programs.is_empty());
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut below = |n: usize| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        usize::try_from(rng % u64::try_from(n).unwrap()).unwrap()
    };
    LOCAL.with(|c| c.set((0, 0)));
    for case in 0..60_000 {
        let base = &programs[case % programs.len()];
        let fidx = below(base.functions.len());
        let palette: Vec<CsilInstr> = base
            .functions
            .iter()
            .flat_map(|func| func.code.iter())
            .filter(|instr| {
                !matches!(
                    instr,
                    CsilInstr::Jump { .. }
                        | CsilInstr::JumpIfFalse { .. }
                        | CsilInstr::JumpIfExact { .. }
                        | CsilInstr::JumpIfSlotWritten { .. }
                        | CsilInstr::Return
                )
            })
            .cloned()
            .collect();
        if palette.is_empty() {
            continue;
        }
        let len = 1 + below(if case % 4 == 0 { 16 } else { 6 });
        let mut code = Vec::with_capacity(len + 1);
        for _ in 0..len {
            let roll = below(100);
            let target = u32::try_from(below(len + 1)).unwrap();
            code.push(if roll < 8 {
                CsilInstr::Jump { target }
            } else if roll < 16 {
                CsilInstr::JumpIfFalse { target }
            } else if roll < 22 {
                CsilInstr::Return
            } else {
                palette[below(palette.len())].clone()
            });
        }
        code.push(CsilInstr::Return);
        let mut program = base.clone();
        let func = &mut program.functions[fidx];
        let span = func.source_map[0];
        func.source_map = vec![span; code.len()];
        func.code = code;
        func.budget_reference = None;
        func.max_stack = super::MAX_DECLARED_STACK;
        let _ = super::validate(&program);
    }
    let (compared, rejected) = LOCAL.with(std::cell::Cell::get);
    assert!(
        compared > 5_000,
        "only {compared} functions reached the typed pass"
    );
    assert!(rejected > 300, "only {rejected} typed rejections exercised");
}
