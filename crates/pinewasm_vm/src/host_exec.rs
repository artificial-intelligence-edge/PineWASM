//! Single-instruction execution surface shared by the interpreter loop and
//! the AOT host (`aot_host::VmHost`).
//!
//! Each method here IS the corresponding `run_bar` match arm, so both callers run the same
//! code: `run_bar` calls these with its local
//! budget, and the AOT `host.vm` import calls them through
//! [`Vm::exec_host_site`] with the budget the emitted module handed over.
//! Operands arrive on `self.stack` (argument order, last on top) and results
//! are left on `self.stack`, exactly like the interpreter. Metering inside a
//! method mirrors the dispatch loop's "honest metering" comments: the caller
//! has already charged the 1-unit dispatch cost; methods charge only their
//! documented extras.

use crate::format::format_placeholders;
use crate::{
    timeframe_from_seconds, AlertEvent, DrawingRegistry, LogEntry, RuntimeError, RuntimeErrorKind,
    SecurityReadSpec, TableCell, Vm, INLINE_CALL_ARGS, MAX_STRING_LEN,
    TABLE_CELL_RESERVATION_BYTES,
};
use pinewasm_csil::{CollectionFunc, CsilInstr};
use pinewasm_hir::{BuiltinSeries, CellAttr, DrawKind, HtfExpr, LogLevel, PureFunc};
use pinewasm_mir::heap::{ElemType, HeapObj};
use pinewasm_mir::{HandleId, Heap, Value};

/// Resolve references through the drawing schema, rather than assigning a
/// pair identity to missing/na boundaries or exposing heap handles to a host.
fn linefill_lines(heap: &Heap, handle: HandleId) -> [Option<HandleId>; 2] {
    let Some(HeapObj::Object { fields, .. }) = heap.get(handle) else {
        return [None, None];
    };
    let line = |name| {
        DrawKind::Linefill
            .fields()
            .iter()
            .position(|field| field.name == name)
            .and_then(|index| fields.get(index))
            .and_then(Value::as_ref)
    };
    [line("line1"), line("line2")]
}

/// The anchored chart position of a table object.
fn table_position(heap: &Heap, handle: HandleId) -> Option<&str> {
    let Some(HeapObj::Object { fields, .. }) = heap.get(handle) else {
        return None;
    };
    let position_slot = DrawKind::Table.field_index("position")? as usize;
    match fields.get(position_slot) {
        Some(Value::String(position)) => Some(position),
        _ => None,
    }
}

impl Vm {
    /// Execute the single (non-control-flow) instruction at `pc` as the AOT
    /// host: operands are already on `self.stack`, results are left there.
    /// This is the `host.vm` import's dispatch; control-flow opcodes and
    /// opcodes the native tier compiles inline never arrive here.
    pub(crate) fn exec_host_site(
        &mut self,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let program = self.program.clone();
        let Some(instr) = program.main().code.get(pc) else {
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("host site pc {pc} out of range"),
                pc,
            ));
        };
        match instr {
            // A color constant a packed native cell cannot hold (a
            // fractional transparency): the linked chart host keeps it.
            CsilInstr::PushColor { r, g, b, t } => self.push(
                crate::vm_ops::const_value(crate::vm_ops::VmConst::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: t.get(),
                }),
                pc,
            ),
            CsilInstr::FillColor {
                fill_slot,
                color_count,
            } => self.exec_fill_color(*fill_slot, *color_count, pc),
            CsilInstr::CallCollection { func, argc } => {
                self.exec_call_collection(*func, *argc, pc, budget)
            }
            CsilInstr::NewObject {
                type_id,
                field_count,
            } => self.exec_new_object(*type_id, *field_count, pc, budget),
            CsilInstr::CopyObject { type_id } => self.exec_copy_object(*type_id, pc, budget),
            CsilInstr::GetField { field_idx } => self.exec_get_field(*field_idx, pc),
            CsilInstr::SetField { field_idx } => self.exec_set_field(*field_idx, pc),
            CsilInstr::RegisterDrawing { kind } => self.exec_register_drawing(*kind, pc, budget),
            CsilInstr::ReadDrawingsAll { kind } => self.exec_read_drawings_all(*kind, pc, budget),
            CsilInstr::DeleteDrawing { kind } => self.exec_delete_drawing(*kind, pc, budget),
            CsilInstr::SetCell { attrs } => self.exec_set_cell(attrs, pc),
            CsilInstr::ClearCells => self.exec_clear_cells(pc),
            CsilInstr::MergeCells => self.exec_merge_cells(pc, budget),
            CsilInstr::Log { level, argc } => self.exec_log(*level, *argc, pc),
            CsilInstr::Alert => self.exec_alert(pc),
            CsilInstr::AlertCondition => self.exec_alert_condition(pc),
            CsilInstr::StrConcat => self.exec_str_concat(pc, budget),
            CsilInstr::CmpEq => self.exec_cmp_eq(false, pc),
            CsilInstr::CmpNe => self.exec_cmp_eq(true, pc),
            CsilInstr::CallPure { func, argc } => self.exec_call_pure_op(*func, *argc, pc, budget),
            CsilInstr::CallPivotPointLevels { argc } => {
                self.exec_call_pivot_point_levels(*argc, pc)
            }
            CsilInstr::RuntimeError => self.exec_runtime_error(pc),
            CsilInstr::ReadSecurity {
                selector,
                source,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
            } => self.exec_read_security(
                SecurityReadSpec {
                    selector: selector.as_deref(),
                    source: *source,
                    htf_seconds: *htf_seconds,
                    timeframe_expr: timeframe_expr.as_deref(),
                    calendar_months: *calendar_months,
                    gaps_on: *gaps_on,
                    lookahead_on: *lookahead_on,
                },
                pc,
                budget,
            ),
            CsilInstr::ReadSecurityExpr {
                expr,
                htf_seconds,
                timeframe_expr,
                calendar_months,
                gaps_on,
                lookahead_on,
            } => self.exec_read_security_expr(
                expr,
                *htf_seconds,
                timeframe_expr.as_deref(),
                *calendar_months,
                *gaps_on,
                *lookahead_on,
                pc,
                budget,
            ),
            CsilInstr::ReadRequest { func: _, selector } => {
                let value = self.read_request(selector);
                self.push(value, pc)
            }
            CsilInstr::BuildFootprint { selector } => {
                let value = self.build_footprint(selector, pc)?;
                self.push(value, pc)
            }
            CsilInstr::ReadSecurityLowerTf { selector } => {
                let value = self.build_security_lower_tf(selector, pc)?;
                self.push(value, pc)
            }
            CsilInstr::ReadPivotLevels {
                system,
                anchor_seconds,
            } => {
                let value = self.build_pivot_levels(*system, *anchor_seconds, pc, budget)?;
                self.push(value, pc)
            }
            CsilInstr::ReadTimeTf { close, tf_seconds } => {
                let timeframe = timeframe_from_seconds(*tf_seconds);
                let value =
                    self.time_function_value(&timeframe, "", "", *close, 0, 0, pc, budget)?;
                self.push(value, pc)
            }
            CsilInstr::FootprintRowByPrice => {
                let price = self.pop(pc)?;
                let footprint = self.pop(pc)?;
                let value = self.footprint_row_by_price(&footprint, &price, pc)?;
                self.push(value, pc)
            }
            CsilInstr::ReadInput { input } => {
                let value = self.resolve_input(*input, pc)?;
                self.push(value, pc)
            }
            CsilInstr::ReadBuiltin { source, offset } => {
                let value = self.read_builtin(*source, *offset);
                self.push(value, pc)
            }
            CsilInstr::ReadBuiltinDynamic { source } => self.exec_read_builtin_dynamic(*source, pc),
            // A strategy chart module's orders: the host's broker is the
            // interpreter's, advanced to this bar before the call.
            CsilInstr::Order { kind } => self.exec_order_operands(*kind, pc),
            other => Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("opcode {other:?} is not a host-executable site"),
                pc,
            )),
        }
    }

    #[inline]
    pub(crate) fn exec_read_builtin_dynamic(
        &mut self,
        source: BuiltinSeries,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        let offset = self.pop(pc)?;
        let value = match Self::runtime_history_offset(offset) {
            Some(offset) => {
                let offset = self.check_dynamic_history_offset(offset, pc)?;
                self.read_builtin(source, offset)
            }
            _ => Value::Na,
        };
        self.push(value, pc)
    }

    #[inline]
    pub(crate) fn exec_call_pure_op(
        &mut self,
        func: PureFunc,
        argc: u8,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let n = argc as usize;
        if self.stack.len() < n {
            return Err(self.fail(
                RuntimeErrorKind::StackUnderflow,
                "operand stack underflow (validator should have rejected this program)".into(),
                pc,
            ));
        }
        let (min, max) = func.argc_bounds();
        if !(min..=max).contains(&argc) {
            // Rejected by the validator; defense in depth.
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("CALL_PURE {} with argc {argc}", func.name()),
                pc,
            ));
        }
        let start = self.stack.len() - n;
        if let Some(value) = crate::numeric_call::evaluate(func, &self.stack[start..]) {
            self.stack.truncate(start);
            return self.push(value, pc);
        }
        let value = if n <= INLINE_CALL_ARGS {
            let mut buf: [Value; INLINE_CALL_ARGS] = std::array::from_fn(|_| Value::Na);
            self.fill_call_args(&mut buf, n);
            let projected = self.charge_pure_string_work(func, &buf[..n], pc, budget)?;
            let value = self.call_pure(func, &buf[..n], pc, budget)?;
            if projected.is_none() {
                if let Value::String(s) = &value {
                    self.reserve_string_bytes(s.len(), pc)?;
                }
            }
            value
        } else {
            let args = self.take_call_args(n);
            let result = self
                .charge_pure_string_work(func, &args, pc, budget)
                .and_then(|projected| {
                    let value = self.call_pure(func, &args, pc, budget)?;
                    if projected.is_none() {
                        if let Value::String(s) = &value {
                            self.reserve_string_bytes(s.len(), pc)?;
                        }
                    }
                    Ok(value)
                });
            self.restore_call_args(args);
            result?
        };
        self.push(value, pc)
    }

    pub(crate) fn exec_call_pivot_point_levels(
        &mut self,
        argc: u8,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        if argc != 3 || self.stack.len() < usize::from(argc) {
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("CALL_PIVOT_POINT_LEVELS requires three operands, found {argc}"),
                pc,
            ));
        }
        let mut args: [Value; 3] = std::array::from_fn(|_| Value::Na);
        self.fill_call_args(&mut args, 3);
        let system = match &args[0] {
            Value::String(name) => pinewasm_hir::PivotSystem::from_type_name(name),
            _ => None,
        }
        .ok_or_else(|| self.fail(
            RuntimeErrorKind::Unsupported,
            "ta.pivot_point_levels type must name Traditional, Fibonacci, Woodie, Classic, DM, or Camarilla".into(),
            pc,
        ))?;
        let anchor = match args[1] {
            Value::Bool(value) => value,
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    "ta.pivot_point_levels anchor must be bool".into(),
                    pc,
                ))
            }
        };
        let developing = match args[2] {
            Value::Bool(value) => value,
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    "ta.pivot_point_levels developing must be bool".into(),
                    pc,
                ))
            }
        };
        if system == pinewasm_hir::PivotSystem::Woodie && developing {
            return Err(self.fail(
                RuntimeErrorKind::Unsupported,
                "ta.pivot_point_levels cannot use developing=true with Woodie pivots".into(),
                pc,
            ));
        }

        let bar = self.bar;
        let current_open = self.data.open[bar];
        let current_high = self.data.high[bar];
        let current_low = self.data.low[bar];
        let current_close = self.data.close[bar];
        let state = self.pivot_states.entry(pc).or_default();
        if anchor {
            if let Some((open, high, low, close)) = state.period.take() {
                state.completed_period = Some((open, high, low, close, current_open));
            }
            state.period = Some((current_open, current_high, current_low, current_close));
        } else {
            match &mut state.period {
                Some((_, high, low, close)) => {
                    *high = high.max(current_high);
                    *low = low.min(current_low);
                    *close = current_close;
                }
                period @ None => {
                    *period = Some((current_open, current_high, current_low, current_close));
                }
            }
        }
        let levels = if developing {
            state
                .period
                .map(|(open, high, low, close)| system.levels(open, high, low, close, current_open))
        } else {
            state
                .completed_period
                .map(|(open, high, low, close, pivot_open)| {
                    system.levels(open, high, low, close, pivot_open)
                })
        };
        let items = levels.map_or_else(
            || vec![Value::Na; pinewasm_hir::PivotSystem::LEVEL_COUNT],
            |levels| levels.into_iter().map(Value::from_f64).collect(),
        );
        let handle = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Float,
                items,
            })
            .map_err(|error| self.heap_limit_err(error, pc))?;
        self.push(Value::Ref(handle), pc)
    }

    #[inline]
    pub(crate) fn exec_call_collection(
        &mut self,
        func: CollectionFunc,
        argc: u8,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let n = argc as usize;
        if self.stack.len() < n {
            return Err(self.fail(
                RuntimeErrorKind::StackUnderflow,
                "operand stack underflow (validator should have rejected this program)".into(),
                pc,
            ));
        }
        let (min, max) = func.argc_bounds();
        if !(min..=max).contains(&argc) {
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("CALL_COLLECTION {} with argc {argc}", func.name()),
                pc,
            ));
        }
        let start = self.stack.len() - n;
        if matches!(
            func,
            CollectionFunc::InternalDistanceScanEligible | CollectionFunc::InternalDistanceScan
        ) {
            let args = self.take_call_args(n);
            let result = if func == CollectionFunc::InternalDistanceScanEligible {
                Ok(Value::Bool(self.distance_scan_eligible(&args)))
            } else {
                self.exec_distance_scan(&args, pc, budget)
            };
            self.restore_call_args(args);
            return self.push(result?, pc);
        }
        if func == CollectionFunc::ArrayGet {
            if let Some(value) = crate::array_get::evaluate(&self.heap, &self.stack[start..]) {
                self.stack.truncate(start);
                return self.push(value, pc);
            }
        }
        let value = if n <= INLINE_CALL_ARGS {
            let mut buf: [Value; INLINE_CALL_ARGS] = std::array::from_fn(|_| Value::Na);
            self.fill_call_args(&mut buf, n);
            let args = &buf[..n];
            // Honest metering: O(n) collection operations that scan or
            // duplicate the array cost their element count (one unit was
            // already charged at dispatch); O(1) operations cost 1. See
            // docs/ir.md (CALL_COLLECTION metering).
            if func.is_linear_cost() {
                let extra = self.collection_extra_cost(func, args).saturating_sub(1);
                self.charge_extra_work(budget, extra, pc)?;
            }
            self.call_collection(func, args, pc)?
        } else {
            let args = self.take_call_args(n);
            if func.is_linear_cost() {
                let extra = self.collection_extra_cost(func, &args).saturating_sub(1);
                self.charge_extra_work(budget, extra, pc)?;
            }
            let result = self.call_collection(func, &args, pc);
            self.restore_call_args(args);
            result?
        };
        self.push(value, pc)
    }

    #[inline]
    pub(crate) fn exec_new_object(
        &mut self,
        type_id: u32,
        field_count: u32,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let n = field_count as usize;
        if self.stack.len() < n {
            return Err(self.fail(
                RuntimeErrorKind::StackUnderflow,
                "operand stack underflow (validator should have rejected this program)".into(),
                pc,
            ));
        }
        // Honest metering: constructing N fields costs N (one unit
        // was already charged at dispatch).
        let extra = (n as u64).saturating_sub(1);
        self.charge_extra_work(budget, extra, pc)?;
        // Fields were pushed in declaration order; split_off keeps
        // them in that order.
        let mut fields = self.stack.split_off(self.stack.len() - n);
        // Drawings own point coordinates at creation. A reused scratch array
        // or a mutated chart point must not alter an earlier contour.
        if self
            .program
            .object_types
            .get(type_id as usize)
            .is_some_and(|ty| ty.name == "polyline")
        {
            let point_field = DrawKind::Polyline
                .fields()
                .iter()
                .position(|field| field.name == "points");
            if let Some(index) = point_field {
                if let Some(Value::Ref(array)) = fields.get(index) {
                    let count = match self.heap.get(*array) {
                        Some(HeapObj::Array { items, .. }) => items.len(),
                        _ => {
                            return Err(self.fail(
                                RuntimeErrorKind::TypeMismatch,
                                "polyline points must be an array".into(),
                                pc,
                            ))
                        }
                    };
                    self.charge_extra_work(budget, (count as u64).saturating_mul(4), pc)?;
                    let (elem_ty, handles) = match self.heap.get(*array) {
                        Some(HeapObj::Array { elem_ty, items }) => (*elem_ty, items.clone()),
                        _ => unreachable!(),
                    };
                    let mut points = Vec::with_capacity(handles.len());
                    for point in handles {
                        let Value::Ref(handle) = point else {
                            points.push(point);
                            continue;
                        };
                        let object = match self.heap.get(handle) {
                            Some(HeapObj::Object {
                                type_id, fields, ..
                            }) => HeapObj::Object {
                                table_identity: None,
                                type_id: *type_id,
                                fields: fields.clone(),
                            },
                            _ => {
                                return Err(self.fail(
                                    RuntimeErrorKind::TypeMismatch,
                                    "polyline vertex must be a chart point".into(),
                                    pc,
                                ))
                            }
                        };
                        let copied = self
                            .heap
                            .alloc(object)
                            .map_err(|e| self.heap_limit_err(e, pc))?;
                        points.push(Value::Ref(copied));
                    }
                    let copied = self
                        .heap
                        .alloc(HeapObj::Array {
                            elem_ty,
                            items: points,
                        })
                        .map_err(|e| self.heap_limit_err(e, pc))?;
                    fields[index] = Value::Ref(copied);
                }
            }
        }
        let handle = self
            .heap
            .alloc(HeapObj::Object {
                type_id,
                fields,
                table_identity: None,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        self.push(Value::Ref(handle), pc)
    }

    #[inline]
    pub(crate) fn exec_copy_object(
        &mut self,
        type_id: u32,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let src = self.pop(pc)?;
        let h = self.object_handle(&src, pc)?;
        let field_count = match self.heap.get(h) {
            Some(HeapObj::Object { fields, .. }) => fields.len(),
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "COPY_OBJECT source is not an object".into(),
                    pc,
                ))
            }
        };
        // Honest metering: copying N fields costs N (one unit was already
        // charged at dispatch). Charged before any clone work.
        let extra = (field_count as u64).saturating_sub(1);
        self.charge_extra_work(budget, extra, pc)?;
        // Cloning a field clones each direct `Value::String` payload, so
        // those bytes are runtime-created strings: project and reserve them
        // against the cumulative string budget before cloning. `Ref` fields
        // are shallow handle copies and allocate no string bytes.
        let string_bytes = match self.heap.get(h) {
            Some(HeapObj::Object { fields, .. }) => {
                self.projected_string_clone_bytes(fields.iter(), pc)?
            }
            _ => 0,
        };
        let charged_before = self.string_bytes_allocated;
        self.reserve_string_bytes(string_bytes, pc)?;
        let fields = match self.heap.get(h) {
            Some(HeapObj::Object { fields, .. }) => fields.clone(),
            _ => Vec::new(),
        };
        // A heap-limit rejection refunds exactly this reservation.
        let handle = self.alloc_after_string_charge(
            HeapObj::Object {
                type_id,
                fields,
                table_identity: None,
            },
            charged_before,
            pc,
        )?;
        self.push(Value::Ref(handle), pc)
    }

    #[inline]
    pub(crate) fn exec_get_field(&mut self, field_idx: u32, pc: usize) -> Result<(), RuntimeError> {
        let obj = self.pop(pc)?;
        let h = self.object_handle(&obj, pc)?;
        let value = match self.heap.get(h) {
            Some(HeapObj::Object { fields, .. }) => {
                fields.get(field_idx as usize).cloned().ok_or_else(|| {
                    self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("field index {field_idx} out of range"),
                        pc,
                    )
                })?
            }
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "GET_FIELD receiver is not an object".into(),
                    pc,
                ))
            }
        };
        self.push(value, pc)
    }

    #[inline]
    pub(crate) fn exec_set_field(&mut self, field_idx: u32, pc: usize) -> Result<(), RuntimeError> {
        // The value was pushed last (on top); the object handle is
        // below it.
        let value = self.pop(pc)?;
        let obj = self.pop(pc)?;
        let h = self.object_handle(&obj, pc)?;
        let idx = field_idx as usize;
        match self.heap.get_mut(h) {
            Some(HeapObj::Object { fields, .. }) => match fields.get_mut(idx) {
                Some(slot) => *slot = value,
                None => {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        format!("field index {field_idx} out of range"),
                        pc,
                    ))
                }
            },
            _ => {
                return Err(self.fail(
                    RuntimeErrorKind::InvalidProgram,
                    "SET_FIELD receiver is not an object".into(),
                    pc,
                ))
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_register_drawing(
        &mut self,
        kind: DrawKind,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // A new fill replaces the prior fill for the unordered line pair,
        // before live-cap eviction. Its new handle, fresh id and reference
        // order are preserved. Neither stale deletion nor color mutation can
        // restore the replaced fill to the live registry.
        let obj = self.pop(pc)?;
        let h = self.object_handle(&obj, pc)?;
        if let Some(slot) = DrawingRegistry::slot_of(kind) {
            if kind == DrawKind::Table {
                // Registering an existing table again cannot create another
                // lifetime or a duplicate entry in table.all.
                if self.drawings.live[slot].contains(&h) {
                    return self.push(obj, pc);
                }
                let identity = match self.heap.get(h) {
                    Some(HeapObj::Object { table_identity, .. }) => *table_identity,
                    _ => {
                        return Err(self.fail(
                            RuntimeErrorKind::InvalidProgram,
                            "table registration receiver is not an object".into(),
                            pc,
                        ));
                    }
                };
                // Prepare before retiring the old table, but commit only once
                // every fallible alias-plan/budget admission has succeeded.
                let assigned = if identity.is_none() {
                    Some(
                        self.last_table_identity
                            .checked_add(1)
                            .and_then(std::num::NonZeroU64::new)
                            .ok_or_else(|| {
                                self.fail(
                                    RuntimeErrorKind::CollectionLimitExceeded,
                                    "table lifetime identity space exhausted".into(),
                                    pc,
                                )
                            })?,
                    )
                } else {
                    None
                };
                // Tables are scoped to this VM/script, with one table per
                // position. A script may hold at most nine tables;
                // `force_overlay` changes the target pane but does not add
                // another table slot for the same position.
                if let Some(position) = table_position(&self.heap, h) {
                    let replaced = self.drawings.live[slot].iter().copied().find(|&table| {
                        table != h && table_position(&self.heap, table) == Some(position)
                    });
                    if let Some(table) = replaced {
                        let cell_count = self
                            .drawings
                            .table_cells
                            .get(&table.0)
                            .map_or(0, |grid| grid.cells.len());
                        self.invalidate_table_aliases(table, pc, budget, cell_count, true)?;
                        self.drawings.table_cells.remove(&table.0);
                        self.drawings.unregister(slot, table);
                    }
                }
                if let Some(identity) = assigned {
                    let Some(HeapObj::Object { table_identity, .. }) = self.heap.get_mut(h) else {
                        unreachable!("validated table object remains allocated during registration")
                    };
                    *table_identity = Some(identity);
                    self.last_table_identity = identity.get();
                }
            }
            if kind == DrawKind::Linefill {
                if let [Some(first), Some(second)] = linefill_lines(&self.heap, h) {
                    self.unregister_linefills(|lines| {
                        lines == [Some(first), Some(second)] || lines == [Some(second), Some(first)]
                    });
                }
            }
            let evicted = self.drawings.register(slot, h);
            if kind == DrawKind::Line {
                if let Some(line) = evicted {
                    self.unregister_linefills(|lines| lines.contains(&Some(line)));
                }
            }
        }
        self.push(obj, pc)
    }

    /// Both engine tiers share this registry and heap, which are restored
    /// together on realtime rollback. No second pair cache can become stale.
    fn unregister_linefills(&mut self, mut matches: impl FnMut([Option<HandleId>; 2]) -> bool) {
        let heap = &self.heap;
        if let Some(slot) = DrawingRegistry::slot_of(DrawKind::Linefill) {
            self.drawings
                .unregister_where(slot, |handle| matches(linefill_lines(heap, handle)));
        }
    }

    #[inline]
    pub(crate) fn exec_read_drawings_all(
        &mut self,
        kind: DrawKind,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // Build a fresh `array<drawing>` of the kind's live objects
        // in creation order. Bounded by the per-kind live cap and
        // the heap cap; charge the copied element count.
        let handles = match DrawingRegistry::slot_of(kind) {
            Some(slot) => self.drawings.live[slot].clone(),
            None => Vec::new(),
        };
        let extra = (handles.len() as u64).saturating_sub(1);
        self.charge_extra_work(budget, extra, pc)?;
        // The array's element type is the kind's registered object
        // type (looked up by name); an empty registry still yields a
        // correctly-typed empty array.
        let type_id = self
            .program
            .object_types
            .iter()
            .position(|t| t.name == kind.type_name())
            .map_or(0, |i| i as u32);
        let items: Vec<Value> = handles.into_iter().map(Value::Ref).collect();
        let h = self
            .heap
            .alloc(HeapObj::Array {
                elem_ty: ElemType::Object(type_id),
                items,
            })
            .map_err(|e| self.heap_limit_err(e, pc))?;
        self.push(Value::Ref(h), pc)
    }

    #[inline]
    pub(crate) fn exec_delete_drawing(
        &mut self,
        kind: DrawKind,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // Pop the drawing handle and remove it from the live registry
        // (no-op if absent). An `na` receiver is also a no-op.
        // Stack effect is (1, 0): the void-mutator placeholder `na`
        // is supplied by a separate `PUSH_NA` the lowering emits
        // (see pinewasm_csil::lower). Do NOT push one here, or the
        // operand stack leaks one slot per delete.
        let obj = self.pop(pc)?;
        if obj.is_na() {
            return Ok(());
        }
        let h = self.object_handle(&obj, pc)?;
        if kind == DrawKind::Table {
            let cell_count = self
                .drawings
                .table_cells
                .get(&h.0)
                .map_or(0, |grid| grid.cells.len());
            self.invalidate_table_aliases(h, pc, budget, cell_count, false)?;
            // The grid is out-of-line from the heap object. Drop it with the
            // live id so its cells stop consuming memory and GC-root space.
            self.drawings.table_cells.remove(&h.0);
        }
        if let Some(slot) = DrawingRegistry::slot_of(kind) {
            self.drawings.unregister(slot, h);
            if kind == DrawKind::Line {
                self.unregister_linefills(|lines| lines.contains(&Some(h)));
            }
        }
        Ok(())
    }

    /// Deleting a table makes every variable and collection that refers to it
    /// become `na`. Same-position replacement retains a runtime displacement marker.
    /// Rewrite direct VM stores, then the reachable heap graph, so
    /// `na()` and later table methods observe the same value. The roots are
    /// captured before the direct stores change, preserving paths through
    /// arrays, maps and UDT fields. Realtime rollback journals non-current
    /// history cells separately; current cells follow the existing `varip`
    /// snapshot rules.
    fn invalidate_table_aliases(
        &mut self,
        table: HandleId,
        pc: usize,
        budget: &mut u64,
        deleted_cells: usize,
        displaced: bool,
    ) -> Result<(), RuntimeError> {
        let unavailable = if displaced {
            match self.heap.get(table) {
                Some(HeapObj::Object {
                    table_identity: Some(identity),
                    ..
                }) => Value::DisplacedTable(*identity),
                _ => {
                    return Err(self.fail(
                        RuntimeErrorKind::InvalidProgram,
                        "displaced table has no lifetime identity".into(),
                        pc,
                    ));
                }
            }
        } else {
            Value::Na
        };
        // Charge both direct passes (root discovery and rewrite), plus the
        // cost of dropping table cells, before reading any slot value or
        // planning over the reachable heap. This keeps a low-budget delete
        // from doing the very scan that it then refuses.
        let mut direct_values = 0u64;
        for (slot, history) in self.user_slots.iter().enumerate() {
            if history.is_empty() {
                continue;
            }
            let depth = (self.gc_history_depths[slot] as usize)
                .min(self.bar)
                .min(history.len() - 1);
            direct_values = direct_values.saturating_add(depth as u64 + 1);
        }
        direct_values = direct_values
            .saturating_add(self.stack.len() as u64)
            .saturating_add(self.call_args.len() as u64);
        // Three user-slot header walks (count, root discovery, rewrite) and
        // two walks each over stack/call_args (discovery and rewrite).
        let direct_headers = (self.user_slots.len() as u64)
            .saturating_mul(3)
            .saturating_add(4);
        let direct_work = direct_values
            .saturating_mul(2)
            .saturating_add(direct_headers);
        // Use a conservative fixed cost for each cell rather than scanning
        // the whole map to count populated attributes before charging it.
        let deleted_cell_work = u64::try_from(deleted_cells)
            .unwrap_or(u64::MAX)
            .saturating_mul(crate::TABLE_CELL_DELETE_WORK_UNITS);
        self.charge_extra_work(budget, direct_work.saturating_add(deleted_cell_work), pc)?;

        let mut roots = Vec::new();
        let realtime_active = self.realtime_active;
        let current_index = self.bar & self.slot_mask;
        let mut journal_additions = 0usize;
        for (slot, history) in self.user_slots.iter().enumerate() {
            if history.is_empty() {
                continue;
            }
            let depth = (self.gc_history_depths[slot] as usize)
                .min(self.bar)
                .min(history.len() - 1);
            for offset in 0..=depth {
                let index = (self.bar - offset) & self.slot_mask;
                if let Some(Value::Ref(handle)) = history.get(index) {
                    roots.push(*handle);
                    if realtime_active
                        && index != current_index
                        && *handle == table
                        && !self.realtime_alias_undo.contains_key(&(slot, index))
                    {
                        journal_additions = journal_additions.saturating_add(1);
                    }
                }
            }
        }
        for value in self.stack.iter().chain(&self.call_args) {
            if let Value::Ref(handle) = value {
                roots.push(*handle);
            }
        }
        let remaining_work = (*budget).min(self.instructions_remaining);
        let Some(plan) = self.heap.plan_refs_to_na(table, &roots, remaining_work) else {
            // Planning stops at the first unit beyond the remaining ceiling.
            // Charge that unit to raise the normal per-bar/run error before
            // any table or alias state changes.
            self.charge_extra_work(budget, remaining_work.saturating_add(1), pc)?;
            return Err(self.fail(
                RuntimeErrorKind::InstructionLimitExceeded,
                "table alias invalidation exceeded the instruction budget".into(),
                pc,
            ));
        };
        self.charge_extra_work(budget, plan.work_units(), pc)?;

        let journal_bytes = journal_additions
            .checked_mul(crate::REALTIME_ALIAS_UNDO_ENTRY_RESERVATION_BYTES)
            .ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::VmMemoryLimitExceeded,
                    format!(
                        "realtime alias undo journal exceeds the VM memory limit of {} bytes",
                        self.options.max_vm_bytes_per_run
                    ),
                    pc,
                )
            })?;
        if journal_bytes > 0 {
            // Journal allocations are cumulative, like table-cell and event
            // allocations. Admission precedes alias, registry, and grid edits.
            self.reserve_vm_bytes(journal_bytes, pc)?;
        }

        let replace = |value: &mut Value| {
            if matches!(value, Value::Ref(handle) if *handle == table) {
                *value = unavailable.clone();
            }
        };
        let alias_undo = &mut self.realtime_alias_undo;
        for (slot, history) in self.user_slots.iter_mut().enumerate() {
            if history.is_empty() {
                continue;
            }
            let depth = (self.gc_history_depths[slot] as usize)
                .min(self.bar)
                .min(history.len() - 1);
            for offset in 0..=depth {
                let index = (self.bar - offset) & self.slot_mask;
                if let Some(value) = history.get_mut(index) {
                    if matches!(value, Value::Ref(handle) if *handle == table) {
                        if realtime_active && index != current_index {
                            alias_undo
                                .entry((slot, index))
                                .or_insert_with(|| value.clone());
                        }
                        *value = unavailable.clone();
                    }
                }
            }
        }
        for value in &mut self.stack {
            replace(value);
        }
        for value in &mut self.call_args {
            replace(value);
        }
        if displaced {
            let Value::DisplacedTable(identity) = unavailable else {
                unreachable!("displacement has an identity-bearing unavailable value")
            };
            self.heap
                .apply_refs_to_displaced_table(table, &plan, identity);
        } else {
            self.heap.apply_refs_to_na(table, &plan);
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_set_cell(
        &mut self,
        attrs: &[CellAttr],
        pc: usize,
    ) -> Result<(), RuntimeError> {
        // Pop attribute values (last on top), then row, column, and
        // the table handle. Write
        // each attribute of the cell at (column, row); an
        // out-of-range coordinate is a structured error.
        let mut values = Vec::with_capacity(attrs.len());
        for _ in 0..attrs.len() {
            values.push(self.pop(pc)?);
        }
        values.reverse();
        let row = self.pop(pc)?;
        let column = self.pop(pc)?;
        let obj = self.pop(pc)?;
        // Full table.cell lowers all attributes, including omitted defaults.
        // A call on a deleted table completes without writing; a call on a
        // never-assigned table errors. The early return below applies only
        // when all attributes are set.
        if matches!(obj, Value::DisplacedTable(_)) && attrs == CellAttr::all() {
            return Ok(());
        }
        let h = self.object_handle(&obj, pc)?;
        let (col, r) = self.table_cell_coord(h, &column, &row, pc)?;
        let is_new = self
            .drawings
            .table_cells
            .get(&h.0)
            .is_none_or(|grid| !grid.cells.contains_key(&(col, r)));
        if is_new {
            self.reserve_vm_bytes(TABLE_CELL_RESERVATION_BYTES, pc)?;
        }
        let grid = self.drawings.table_cells.entry(h.0).or_default();
        let cell = grid.cells.entry((col, r)).or_default();
        for (attr, value) in attrs.iter().zip(values) {
            cell.set(*attr, value);
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_clear_cells(&mut self, pc: usize) -> Result<(), RuntimeError> {
        // Pop end_row, end_column, start_row, start_column, then the
        // table handle. Remove every cell in the inclusive range.
        let end_row = self.pop(pc)?;
        let end_col = self.pop(pc)?;
        let start_row = self.pop(pc)?;
        let start_col = self.pop(pc)?;
        let obj = self.pop(pc)?;
        let h = self.object_handle(&obj, pc)?;
        let (sc, sr) = self.table_cell_coord(h, &start_col, &start_row, pc)?;
        let (ec, er) = self.table_cell_coord(h, &end_col, &end_row, pc)?;
        if let Some(grid) = self.drawings.table_cells.get_mut(&h.0) {
            grid.cells.retain(|&(c, r), cell| {
                if !(c >= sc.min(ec) && c <= sc.max(ec) && r >= sr.min(er) && r <= sr.max(er)) {
                    return true;
                }
                // Clearing contents cannot unmerge a table. Preserve region
                // ownership so later writes still address the original cell.
                if cell.merge.is_some() {
                    let merge = cell.merge;
                    *cell = TableCell {
                        merged: true,
                        merge,
                        ..Default::default()
                    };
                    true
                } else {
                    false
                }
            });
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_merge_cells(
        &mut self,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // Pop end_row, end_column, start_row, start_column, then the
        // table handle. Mark every cell in the span merged.
        let end_row = self.pop(pc)?;
        let end_col = self.pop(pc)?;
        let start_row = self.pop(pc)?;
        let start_col = self.pop(pc)?;
        let obj = self.pop(pc)?;
        let h = self.object_handle(&obj, pc)?;
        let (sc, sr) = self.table_cell_coord(h, &start_col, &start_row, pc)?;
        let (ec, er) = self.table_cell_coord(h, &end_col, &end_row, pc)?;
        if sc > ec || sr > er {
            return Err(self.fail(
                RuntimeErrorKind::CollectionIndexOutOfBounds,
                "table.merge_cells requires start coordinates before end coordinates".to_string(),
                pc,
            ));
        }
        if sc == ec && sr == er {
            return Ok(());
        }
        if let Some(grid) = self.drawings.table_cells.get(&h.0) {
            let count = grid.cells.len() as u64;
            self.charge_extra_work(budget, count, pc)?;
            let overlaps = self
                .drawings
                .table_cells
                .get(&h.0)
                .unwrap()
                .cells
                .values()
                .any(|cell| {
                    cell.merge.is_some_and(|m| {
                        m != [sc, sr, ec, er]
                            && sc <= m[2]
                            && ec >= m[0]
                            && sr <= m[3]
                            && er >= m[1]
                    })
                });
            if overlaps {
                return Err(self.fail(
                    RuntimeErrorKind::CollectionIndexOutOfBounds,
                    "table.merge_cells cannot overlap an existing merged region".to_string(),
                    pc,
                ));
            }
        }
        // Meter the span: this densely materializes one cell per
        // (column, row), so a huge span (a table declared with e.g.
        // 100000x100000 dims) must trip the per-bar instruction
        // budget BEFORE allocating, never OOM/hang. Cost is the cell
        // count (saturating so it cannot overflow), charged like the
        // O(n) collection ops. One unit was already charged at
        // dispatch.
        let cols = (sc.max(ec) - sc.min(ec) + 1).max(0) as u64;
        let rows = (sr.max(er) - sr.min(er) + 1).max(0) as u64;
        let extra = cols.saturating_mul(rows).saturating_sub(1);
        self.charge_extra_work(budget, extra, pc)?;
        let cells = cols.saturating_mul(rows);
        let reservation = usize::try_from(cells)
            .ok()
            .and_then(|cells| cells.checked_mul(TABLE_CELL_RESERVATION_BYTES))
            .ok_or_else(|| {
                self.fail(
                    RuntimeErrorKind::VmMemoryLimitExceeded,
                    "table cell reservation size overflowed".to_string(),
                    pc,
                )
            })?;
        // Charge the full span even if some cells already exist. The memory
        // counter is deliberately cumulative, so clear/re-merge churn cannot
        // bypass the run-wide cap.
        self.reserve_vm_bytes(reservation, pc)?;
        let grid = self.drawings.table_cells.entry(h.0).or_default();
        for c in sc.min(ec)..=sc.max(ec) {
            for r in sr.min(er)..=sr.max(er) {
                let cell = grid.cells.entry((c, r)).or_default();
                cell.merged = true;
                cell.merge = Some([sc.min(ec), sr.min(er), sc.max(ec), sr.max(er)]);
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_log(
        &mut self,
        level: LogLevel,
        argc: u8,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        // Pop the argc operands (top-first), reverse to argument
        // order (format string first), then render the message with
        // the same placeholder rules as `str.format` and record it.
        let mut popped = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            popped.push(self.pop(pc)?);
        }
        popped.reverse();
        let (fmt_val, subs) = popped.split_first().ok_or_else(|| {
            self.fail(
                RuntimeErrorKind::InvalidProgram,
                "log requires a format string".into(),
                pc,
            )
        })?;
        // A `na` format renders no message (mirroring str.format).
        let message = match fmt_val {
            Value::Na | Value::DisplacedTable(_) => String::new(),
            // The one-argument overload is literal text, not a format string.
            Value::String(f) if subs.is_empty() => f.clone(),
            Value::String(f) => format_placeholders(f, subs, self.data.mintick),
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!("log format must be a string, found {}", other.type_name()),
                    pc,
                ))
            }
        };
        self.reserve_string_bytes(message.len(), pc)?;
        self.reserve_vm_bytes(
            std::mem::size_of::<LogEntry>().saturating_add(message.len()),
            pc,
        )?;
        self.logs.push(LogEntry {
            level,
            bar: self.bar as u32,
            message,
        });
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_alert(&mut self, pc: usize) -> Result<(), RuntimeError> {
        // Pop freq (top) then message. Honor the once-per-bar
        // dedup: freq_once_per_bar/close fire at most once per bar;
        // freq_all fires on every call.
        let freq_val = self.pop(pc)?;
        let msg_val = self.pop(pc)?;
        let freq = match freq_val {
            Value::String(s) => s,
            // `na` freq falls back to `alert.freq_once_per_bar`.
            Value::Na | Value::DisplacedTable(_) => "alert.freq_once_per_bar".to_string(),
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!("alert freq must be a string, found {}", other.type_name()),
                    pc,
                ))
            }
        };
        let message = match msg_val {
            Value::String(s) => s,
            Value::Na | Value::DisplacedTable(_) => String::new(),
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "alert message must be a string, found {}",
                        other.type_name()
                    ),
                    pc,
                ))
            }
        };
        let once_per_bar = freq != "alert.freq_all";
        // Dedup per call site (keyed by this instruction's pc), not
        // globally: distinct `alert()` calls each fire, while one
        // call executed repeatedly in a bar fires once.
        if once_per_bar && self.alert_fired_sites.contains(&pc) {
            // Deduped: this call site already fired this bar.
        } else {
            let text_bytes = message.len().saturating_add(freq.len());
            self.reserve_string_bytes(text_bytes, pc)?;
            self.reserve_vm_bytes(
                std::mem::size_of::<AlertEvent>().saturating_add(text_bytes),
                pc,
            )?;
            if once_per_bar {
                self.alert_fired_sites.insert(pc);
            }
            self.alerts.push(AlertEvent {
                bar: self.bar as u32,
                message,
                freq,
            });
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_alert_condition(&mut self, pc: usize) -> Result<(), RuntimeError> {
        // Pop message (top), title, then the bool condition. Record
        // an event only when the condition is true; alertcondition events
        // fire once per bar close.
        let msg_val = self.pop(pc)?;
        let _title = self.pop(pc)?;
        let cond_val = self.pop(pc)?;
        let fires = matches!(cond_val, Value::Bool(true));
        if fires {
            let message = match msg_val {
                Value::String(s) => s,
                Value::Na | Value::DisplacedTable(_) => String::new(),
                other => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "alertcondition message must be a string, found {}",
                            other.type_name()
                        ),
                        pc,
                    ))
                }
            };
            let freq = "alert.freq_once_per_bar_close".to_string();
            let text_bytes = message.len().saturating_add(freq.len());
            self.reserve_string_bytes(text_bytes, pc)?;
            self.reserve_vm_bytes(
                std::mem::size_of::<AlertEvent>().saturating_add(text_bytes),
                pc,
            )?;
            self.alerts.push(AlertEvent {
                bar: self.bar as u32,
                message,
                freq,
            });
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn exec_str_concat(
        &mut self,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        // String `+`: na-propagating. Pop rhs then lhs (both
        // strings after validation) and push their concatenation.
        let rhs = self.pop(pc)?;
        let lhs = self.pop(pc)?;
        let value =
            match crate::vm_ops::str_concat_operands(lhs.na_observation(), rhs.na_observation()) {
                Ok(None) => Value::Na,
                Ok(Some((l, r))) => {
                    let combined = l.len().saturating_add(r.len());
                    // Hard cap (like `str_repeat`): refuse to build a
                    // string past MAX_STRING_LEN, so a doubling loop
                    // (`s := s + s`) yields `na` instead of allocating
                    // gigabytes.
                    if combined > MAX_STRING_LEN {
                        Value::Na
                    } else {
                        // Honest metering (like `WindowMean`): `+`
                        // allocates `combined` bytes, so charge budget
                        // proportional to that length. Without this a
                        // loop of large-but-capped concatenations could
                        // do unbounded work for a flat 1-unit cost.
                        let extra = (combined as u64).saturating_sub(1);
                        self.charge_extra_work(budget, extra, pc)?;
                        self.reserve_string_bytes(combined, pc)?;
                        Value::String(format!("{l}{r}"))
                    }
                }
                Err(crate::vm_ops::OpMismatch) => {
                    return Err(self.fail(
                        RuntimeErrorKind::TypeMismatch,
                        format!(
                            "string concatenation requires strings, found {} and {}",
                            lhs.type_name(),
                            rhs.type_name()
                        ),
                        pc,
                    ))
                }
            };
        self.push(value, pc)
    }

    #[inline]
    pub(crate) fn exec_cmp_eq(&mut self, negate: bool, pc: usize) -> Result<(), RuntimeError> {
        let rhs = self.pop(pc)?;
        let lhs = self.pop(pc)?;
        let value = match crate::vm_ops::cmp_eq(negate, lhs.na_observation(), rhs.na_observation())
        {
            Ok(value) => value,
            Err(crate::vm_ops::OpMismatch) => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "cannot compare {} with {}",
                        lhs.type_name(),
                        rhs.type_name()
                    ),
                    pc,
                ))
            }
        };
        self.push(value, pc)
    }

    #[inline]
    pub(crate) fn exec_runtime_error(&mut self, pc: usize) -> Result<(), RuntimeError> {
        // Pop the message and abort the run. A `na` message
        // aborts with empty text. This propagates as an `Err` out of
        // `run_bar`, so the driver records it and stops before
        // incrementing `bars_executed` — prior bars' plots survive.
        let msg_val = self.pop(pc)?;
        let message = match &msg_val {
            Value::String(s) => s.clone(),
            Value::Na | Value::DisplacedTable(_) => String::new(),
            other => {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!(
                        "runtime.error message must be a string, found {}",
                        other.type_name()
                    ),
                    pc,
                ))
            }
        };
        Err(self.fail(RuntimeErrorKind::UserError, message, pc))
    }

    #[inline]
    pub(crate) fn exec_read_security(
        &mut self,
        spec: SecurityReadSpec<'_>,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let (htf_seconds, calendar) = self.resolve_security_timeframe(
            spec.htf_seconds,
            spec.timeframe_expr,
            spec.calendar_months,
            pc,
        )?;
        if spec.selector.is_none() {
            self.check_security_timeframe(htf_seconds, pc)?;
        }
        let value = self.read_security(
            spec.selector,
            spec.source,
            htf_seconds,
            calendar,
            spec.gaps_on,
            spec.lookahead_on,
            pc,
            budget,
        )?;
        self.push(value, pc)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)] // Mirrors the ReadSecurityExpr opcode.
    pub(crate) fn exec_read_security_expr(
        &mut self,
        expr: &HtfExpr,
        htf_seconds: i64,
        timeframe_expr: Option<&str>,
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
        pc: usize,
        budget: &mut u64,
    ) -> Result<(), RuntimeError> {
        let (htf_seconds, calendar) =
            self.resolve_security_timeframe(htf_seconds, timeframe_expr, calendar_months, pc)?;
        self.check_security_timeframe(htf_seconds, pc)?;
        let value = self.read_security_expr(
            expr,
            htf_seconds,
            calendar,
            gaps_on,
            lookahead_on,
            pc,
            budget,
        )?;
        self.push(value, pc)
    }
}

/// `COPY_OBJECT` charges the cumulative string budget for the direct string
/// fields it clones (instruction extras first, then the string reservation,
/// then clone and allocate with a refund on heap rejection). `Ref` fields are
/// shallow handle copies and are never charged.
impl Vm {
    /// Write a fill through the same typed handler in interpreted and linked chart runs.
    pub(crate) fn exec_fill_color(
        &mut self,
        fill_slot: u32,
        color_count: u8,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        let mut colors = Vec::with_capacity(color_count as usize);
        for index in (0..color_count).rev() {
            let color = self.pop(pc)?;
            let numeric = self
                .program
                .fills
                .get(fill_slot as usize)
                .and_then(|fill| fill.color_roles.get(index as usize))
                .is_some_and(|role| role.value_domain() == "float-or-na");
            if !(color.is_na()
                || if numeric {
                    matches!(color, Value::Float(_))
                } else {
                    matches!(color, Value::Color { .. })
                })
            {
                return Err(self.fail(
                    RuntimeErrorKind::TypeMismatch,
                    format!("fill requires a color, found {}", color.type_name()),
                    pc,
                ));
            }
            colors.push(color);
        }
        colors.reverse();
        let fill = fill_slot as usize;
        let Some(expected_colors) = self.fill_colors.get(fill).map(Vec::len) else {
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!("fill slot {fill_slot} out of range"),
                pc,
            ));
        };
        if expected_colors != colors.len() {
            return Err(self.fail(
                RuntimeErrorKind::InvalidProgram,
                format!(
                    "fill slot {fill_slot} expected {expected_colors} color channel(s), got {}",
                    colors.len()
                ),
                pc,
            ));
        }
        let channels = &mut self.fill_colors[fill];
        let bar = self.bar;
        for (channel, color) in channels.iter_mut().zip(colors) {
            channel[bar] = color;
        }
        Ok(())
    }
}

#[cfg(test)]
mod copy_object_string_budget {
    use crate::{MarketData, RunOptions, RuntimeErrorKind, Vm};
    use pinewasm_csil::CsilInstr;
    use pinewasm_mir::heap::HeapObj;
    use pinewasm_mir::{Heap, Value};

    const SCRIPT: &str = "//@version=6\nindicator(\"C\")\ntype P\n    string s\n    float x\np = P.new(\"a\", 1.0)\nq = P.copy(p)\nplot(q.x)\n";
    const TYPE_ID: u32 = 7;
    const BIG: u64 = 1_000_000;

    fn vm_with(max_string_bytes: usize) -> Vm {
        let program = {
            let parsed = pinewasm_parse::parse(SCRIPT);
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            let mir = pinewasm_mir::lower(&analysis.hir);
            pinewasm_csil::lower_mir(&mir).expect("CSIL lowering")
        };
        let data = MarketData {
            open: vec![1.0],
            high: vec![1.0],
            low: vec![1.0],
            close: vec![1.0],
            volume: vec![1.0],
            ..MarketData::default()
        };
        let options = RunOptions {
            max_string_bytes_per_run: max_string_bytes,
            ..RunOptions::default()
        };
        Vm::new(&program, &data, &options).expect("small VM state")
    }

    fn string(s: &str) -> Value {
        Value::String(s.to_string())
    }

    fn object(vm: &mut Vm, fields: Vec<Value>) -> Value {
        let h = vm
            .heap
            .alloc(HeapObj::Object {
                type_id: 0,
                fields,
                table_identity: None,
            })
            .expect("source object");
        Value::Ref(h)
    }

    fn snapshot(vm: &Vm, src: &Value) -> HeapObj {
        let Value::Ref(h) = src else {
            panic!("source is a handle")
        };
        vm.heap.get(*h).cloned().expect("source is live")
    }

    /// Run the actual handler with `src` on the operand stack.
    fn copy(vm: &mut Vm, src: &Value, budget: &mut u64) -> Result<(), crate::RuntimeError> {
        vm.stack.push(src.clone());
        vm.exec_copy_object(TYPE_ID, 0, budget)
    }

    fn copied_fields(vm: &Vm) -> Vec<Value> {
        match vm.stack.last() {
            Some(Value::Ref(h)) => match vm.heap.get(*h) {
                Some(HeapObj::Object {
                    type_id, fields, ..
                }) => {
                    assert_eq!(*type_id, TYPE_ID);
                    fields.clone()
                }
                other => panic!("copy is not an object: {other:?}"),
            },
            other => panic!("copy handle missing: {other:?}"),
        }
    }

    #[test]
    fn copy_object_charges_utf8_byte_lengths() {
        let mut vm = vm_with(usize::MAX);
        // 2 + 3 + 4 bytes; 3 chars.
        let fields = vec![string("é"), string("€"), string("😀")];
        let src = object(&mut vm, fields.clone());
        let before = snapshot(&vm, &src);
        let charged = vm.string_bytes_allocated;
        let mut budget = BIG;
        copy(&mut vm, &src, &mut budget).expect("copy succeeds");
        assert_eq!(vm.string_bytes_allocated - charged, 9);
        assert_eq!(copied_fields(&vm), fields);
        assert_eq!(snapshot(&vm, &src), before);
    }

    #[test]
    fn copy_object_charges_only_direct_string_fields_not_refs() {
        let mut vm = vm_with(usize::MAX);
        let inner = object(&mut vm, vec![string("referenced-bytes")]);
        let fields = vec![
            string("ab"),
            Value::Float(1.5),
            Value::Int(3),
            inner.clone(),
            Value::Na,
            Value::Bool(true),
            string("ü"),
        ];
        let src = object(&mut vm, fields.clone());
        let before = snapshot(&vm, &src);
        let inner_before = snapshot(&vm, &inner);
        let charged = vm.string_bytes_allocated;
        let objects = vm.heap.len();
        let mut budget = BIG;
        copy(&mut vm, &src, &mut budget).expect("copy succeeds");
        assert_eq!(vm.string_bytes_allocated - charged, 2 + 2);
        // Shallow: the Ref field is the same handle; nothing else allocated.
        assert_eq!(copied_fields(&vm), fields);
        assert_eq!(vm.heap.len(), objects + 1);
        assert_eq!(snapshot(&vm, &src), before);
        assert_eq!(snapshot(&vm, &inner), inner_before);
        // Extras are unchanged: 7 fields cost 6 extra units.
        assert_eq!(budget, BIG - 6);
    }

    #[test]
    fn copy_object_exact_budget_then_cumulative_refusal() {
        let probe = vm_with(usize::MAX);
        let baseline = probe.string_bytes_allocated;
        let mut vm = vm_with(baseline + 5);
        assert_eq!(vm.string_bytes_allocated, baseline);
        let src = object(&mut vm, vec![string("hé"), string("xy")]);
        // "hé" = 3 bytes, "xy" = 2 bytes: exactly the remaining 5.
        let before = snapshot(&vm, &src);
        let mut budget = BIG;
        copy(&mut vm, &src, &mut budget).expect("exact-budget copy succeeds");
        assert_eq!(vm.string_bytes_allocated, baseline + 5);
        vm.stack.pop();
        let objects = vm.heap.len();
        let error =
            copy(&mut vm, &src, &mut budget).expect_err("second copy exceeds the run budget");
        assert_eq!(error.kind, RuntimeErrorKind::StringLimitExceeded);
        assert_eq!(vm.string_bytes_allocated, baseline + 5);
        assert_eq!(vm.heap.len(), objects);
        assert_eq!(snapshot(&vm, &src), before);
    }

    #[test]
    fn copy_object_without_string_fields_reserves_nothing() {
        let probe = vm_with(usize::MAX);
        let baseline = probe.string_bytes_allocated;
        // No string headroom at all: a non-zero reservation would be refused.
        let mut vm = vm_with(baseline);
        let inner = object(&mut vm, vec![string("zzz")]);
        let fields = vec![
            Value::Float(2.0),
            Value::Int(1),
            inner,
            Value::Na,
            Value::Bool(false),
        ];
        let src = object(&mut vm, fields.clone());
        let before = snapshot(&vm, &src);
        let mut budget = BIG;
        copy(&mut vm, &src, &mut budget).expect("no string bytes to reserve");
        assert_eq!(vm.string_bytes_allocated, baseline);
        assert_eq!(copied_fields(&vm), fields);
        assert_eq!(snapshot(&vm, &src), before);
    }

    #[test]
    fn copy_object_instruction_budget_precedes_string_reservation() {
        let probe = vm_with(usize::MAX);
        let baseline = probe.string_bytes_allocated;
        // Both limits are short: the string budget has no headroom and the
        // bar budget cannot pay the 2 extra units for 3 fields.
        let mut vm = vm_with(baseline);
        let src = object(&mut vm, vec![string("a"), string("b"), string("c")]);
        let before = snapshot(&vm, &src);
        let objects = vm.heap.len();
        let mut budget = 1;
        let error = copy(&mut vm, &src, &mut budget).expect_err("instruction budget is short");
        assert_eq!(error.kind, RuntimeErrorKind::InstructionLimitExceeded);
        assert_eq!(budget, 1);
        assert_eq!(vm.string_bytes_allocated, baseline);
        assert_eq!(vm.heap.len(), objects);
        assert_eq!(snapshot(&vm, &src), before);

        // Same shape with ample string headroom: still no reservation.
        let mut vm = vm_with(usize::MAX);
        let src = object(&mut vm, vec![string("a"), string("b"), string("c")]);
        let charged = vm.string_bytes_allocated;
        let mut budget = 1;
        let error = copy(&mut vm, &src, &mut budget).expect_err("instruction budget is short");
        assert_eq!(error.kind, RuntimeErrorKind::InstructionLimitExceeded);
        assert_eq!(vm.string_bytes_allocated, charged);
    }

    #[test]
    fn copy_object_object_cap_refunds_the_reservation() {
        let mut vm = vm_with(usize::MAX);
        vm.heap = Heap::with_limits(1_000, 1);
        let src = object(&mut vm, vec![string("abc"), Value::Int(1)]);
        let before = snapshot(&vm, &src);
        let charged = vm.string_bytes_allocated;
        let objects = vm.heap.len();
        let mut budget = BIG;
        let error = copy(&mut vm, &src, &mut budget).expect_err("the copy needs a second slot");
        assert_eq!(error.kind, RuntimeErrorKind::CollectionLimitExceeded);
        assert_eq!(vm.string_bytes_allocated, charged);
        assert_eq!(vm.heap.len(), objects);
        assert_eq!(snapshot(&vm, &src), before);
    }

    #[test]
    fn copy_object_element_cap_refunds_the_reservation() {
        let mut vm = vm_with(usize::MAX);
        // Three field elements live; the copy needs three more.
        vm.heap = Heap::with_limits(4, 1_000);
        let src = object(&mut vm, vec![string("abc"), string("€"), Value::Int(1)]);
        let before = snapshot(&vm, &src);
        let charged = vm.string_bytes_allocated;
        let objects = vm.heap.len();
        let mut budget = BIG;
        let error = copy(&mut vm, &src, &mut budget).expect_err("the copy exceeds the element cap");
        assert_eq!(error.kind, RuntimeErrorKind::CollectionLimitExceeded);
        assert_eq!(vm.string_bytes_allocated, charged);
        assert_eq!(vm.heap.len(), objects);
        assert_eq!(snapshot(&vm, &src), before);
        // The refunded bytes are available to a later copy that fits.
        vm.heap = Heap::with_limits(1_000, 1_000);
        let src = object(&mut vm, vec![string("abc"), string("€"), Value::Int(1)]);
        copy(&mut vm, &src, &mut budget).expect("copy fits");
        assert_eq!(vm.string_bytes_allocated - charged, 6);
    }

    #[test]
    fn copy_object_source_errors_keep_their_kind_and_charge_nothing() {
        let mut vm = vm_with(usize::MAX);
        let array = vm
            .heap
            .alloc(HeapObj::Array {
                elem_ty: pinewasm_mir::ElemType::String,
                items: vec![string("abc")],
            })
            .expect("array");
        let cases = [
            (Value::Na, RuntimeErrorKind::NaFieldAccess),
            (string("abc"), RuntimeErrorKind::TypeMismatch),
            (Value::Ref(array), RuntimeErrorKind::InvalidProgram),
        ];
        for (src, kind) in cases {
            let charged = vm.string_bytes_allocated;
            let objects = vm.heap.len();
            let mut budget = BIG;
            let error = copy(&mut vm, &src, &mut budget).expect_err("bad source");
            assert_eq!(error.kind, kind, "{src:?}");
            assert_eq!(budget, BIG, "{src:?}");
            assert_eq!(vm.string_bytes_allocated, charged, "{src:?}");
            assert_eq!(vm.heap.len(), objects, "{src:?}");
        }
    }

    /// The AOT host dispatcher reaches the same charge for a compiled
    /// `COPY_OBJECT` site.
    #[test]
    fn copy_object_host_site_charges_string_fields() {
        let mut vm = vm_with(usize::MAX);
        let program = vm.program.clone();
        let pc = program
            .main()
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::CopyObject { .. }))
            .expect("the script compiles a COPY_OBJECT site");
        let src = object(&mut vm, vec![string("日本"), Value::Float(1.0)]);
        let before = snapshot(&vm, &src);
        let charged = vm.string_bytes_allocated;
        vm.stack.push(src.clone());
        let mut budget = BIG;
        vm.exec_host_site(pc, &mut budget).expect("host site copy");
        assert_eq!(vm.string_bytes_allocated - charged, 6);
        assert_eq!(snapshot(&vm, &src), before);
    }
}
