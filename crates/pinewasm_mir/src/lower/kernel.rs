//! The HIR → MIR lowering kernel: block structure, evaluation order and the
//! instruction each HIR form emits.
//!
//! It covers every HIR form that reaches MIR: literals, series/input reads,
//! unary/binary operators (including the lazy `and`/`or` guard), int→float
//! widening, ternaries, history materialization, slot-based `ta` calls,
//! call-like nodes (pure, collection and engine calls, objects, drawings,
//! request/security reads), field writes, `line.get_price`, table cell
//! operations, assignments (including `var` initialize-once), tuple
//! destructuring, expression statements, effect statements (plots, visuals,
//! fills, runtime errors, logs, alerts, orders), `if`, `for`, `while`,
//! `break` and `continue`, the hoisted risk directives and the program
//! prologue/epilogue.
//!
//! It is generic over the HIR representation through [`LowerHost`], a small
//! view of one expression or statement (its shape, children, arguments, span
//! and literal value). The only implementation used by the compiler is
//! `HirHost` in the parent module. Keeping `HirExpr`/`Value` (which carry
//! `f64`) behind the trait keeps this module inside the Aeneas-extractable
//! Rust subset. The bodies replace `self.hir`/`HirExprKind` accesses with
//! host calls and write iterator loops as index loops.

use super::{alloc_value_id, emit_into, push_block, terminate_block, BlockBuilder};
use crate::{BlockId, MirBlock, MirInstr, Terminator, Value, ValueId};
use pinewasm_hir::{
    BinOp, BuiltinSeries, CellAttr, CollectionFunc, DrawKind, FillSlot, HtfExpr, LogLevel,
    OrderKind, PivotSystem, PlotColorRole, PlotSlot, PureFunc, RequestFunc, SeriesSlot, SymbolId,
    UnOp, VisualSlot,
};
use pinewasm_span::Span;

/// The jump targets `break`/`continue` resolve to for the innermost loop.
pub(crate) struct LoopTargets {
    /// Where `continue` jumps: the step block (`for`) or the header (`while`).
    pub(crate) continue_target: BlockId,
    /// Where `break` jumps: the loop's exit block.
    pub(crate) break_target: BlockId,
}

/// The mutable lowering state: the blocks built so far, the block receiving
/// instructions, the value-id counter and the enclosing loops.
pub(crate) struct LowerState {
    pub(crate) blocks: Vec<BlockBuilder>,
    pub(crate) current: BlockId,
    pub(crate) next_value: u32,
    /// Dense slot assigned to the next branch-expression result. The slots
    /// begin after the HIR slots and are appended to MIR metadata by the
    /// caller.
    pub(crate) next_temp_slot: u32,
    pub(crate) temp_slot_count: u32,
    /// Stack of enclosing loops; the top is the innermost, so `break` and
    /// `continue` affect it.
    pub(crate) loops: Vec<LoopTargets>,
}

/// The one-result instruction a call-like expression emits. Its operands are
/// the node's arguments ([`LowerHost::expr_arg`], `arity` of them), evaluated
/// left to right; the instruction consumes them in order (the stack lowering
/// pops them in reverse) and defines one fresh register.
pub(crate) enum CallKind {
    /// `CallPure`: arguments in order.
    Pure(PureFunc),
    /// `CallCollection`: arguments in order (receiver handle first).
    Collection(CollectionFunc),
    /// A single-result engine call (`CallTaState`): the runtime source
    /// operands in order; the const params live in the `ta_states` table.
    Ta(u32),
    /// `NewObject`: the field initializers in declaration order.
    NewObject(u32),
    /// `CopyObject`: argument 0 is the object.
    CopyObject(u32),
    /// `GetField`: argument 0 is the object.
    GetField(u32),
    /// Arguments 0, 1: the footprint, the price.
    FootprintRowByPrice,
    /// `RegisterDrawing`: argument 0 is the drawing object.
    RegisterDrawing(DrawKind),
    /// `DeleteDrawing`: argument 0 is the drawing id.
    DeleteDrawing(DrawKind),
    /// The remaining kinds read external data and take no argument.
    ReadSecurity {
        selector: Option<String>,
        source: BuiltinSeries,
        htf_seconds: i64,
        timeframe_expr: Option<String>,
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    ReadSecurityExpr {
        expr: HtfExpr,
        htf_seconds: i64,
        timeframe_expr: Option<String>,
        calendar_months: Option<u32>,
        gaps_on: bool,
        lookahead_on: bool,
    },
    ReadRequest {
        func: RequestFunc,
        selector: String,
    },
    BuildFootprint(String),
    ReadSecurityLowerTf(String),
    ReadPivotLevels {
        system: PivotSystem,
        anchor_seconds: i64,
    },
    /// Stateful Pine pivot levels: three runtime operands in signature order.
    CallPivotPointLevels,
    ReadTimeTf {
        close: bool,
        tf_seconds: i64,
    },
    ReadDrawingsAll(DrawKind),
}

/// How the kernel lowers one expression. Fixed children are fetched with
/// [`LowerHost::expr_child`] in the order given per variant, argument lists
/// with [`LowerHost::expr_arg`].
pub(crate) enum ExprShape {
    /// A literal: `Const*`, `IntToFloat` of an int/na literal, or a literal
    /// color constructor (folded). The constant is
    /// [`LowerHost::literal_value`].
    Literal,
    ReadBuiltin {
        series: BuiltinSeries,
        offset: u32,
    },
    ReadVar {
        symbol: SymbolId,
        offset: u32,
    },
    /// Child 0: the offset.
    ReadBuiltinDynamic {
        series: BuiltinSeries,
    },
    /// Child 0: the offset.
    ReadVarDynamic {
        symbol: SymbolId,
    },
    ReadInput {
        input: u32,
    },
    /// Child 0: the operand.
    Unary {
        op: UnOp,
        is_int: bool,
    },
    /// Children 0, 1: lhs, rhs.
    Binary {
        op: BinOp,
        is_int: bool,
        is_string: bool,
    },
    /// Child 0: the (non-literal) operand.
    IntToFloat,
    /// Children 0, 1, 2: condition, then, else.
    Ternary,
    /// Child 0: the materialized expression.
    MaterializeHistory {
        slot: SeriesSlot,
        offset: u32,
    },
    /// Children 0, 1: the materialized expression, the offset.
    MaterializeHistoryDynamic {
        slot: SeriesSlot,
    },
    /// `ta.change` (slot-based): child 0 is the source.
    TaChange {
        slot: SeriesSlot,
        length: u32,
    },
    /// `ta.sma` (slot-based): child 0 is the source.
    TaSma {
        slot: SeriesSlot,
        length: u32,
    },
    /// A call-like node with `arity` arguments.
    Call {
        kind: CallKind,
        arity: usize,
    },
    /// `obj.field := value` as an expression: children 0, 1 are the object
    /// and the value.
    SetFieldExpr {
        field_idx: u32,
    },
    /// Several field writes on one receiver: child 0 is the receiver,
    /// argument `i` the value written to `field_idxs[i]`.
    SetFields {
        recv_slot: SeriesSlot,
        field_idxs: Vec<u32>,
    },
    /// `line.get_price`: children 0, 1 are the line and `x`.
    LineGetPrice {
        recv_slot: SeriesSlot,
    },
    /// `table.cell*` setters: children 0, 1, 2 are the table, column and
    /// row; argument `i` is the value of `attrs[i]`.
    SetCell {
        recv_slot: SeriesSlot,
        attrs: Vec<CellAttr>,
    },
    /// `table.clear` (`merge = false`) / `table.merge_cells` (`merge =
    /// true`): children 0..=4 are the table, start column/row, end
    /// column/row.
    Cells {
        recv_slot: SeriesSlot,
        merge: bool,
    },
    /// Any other form: lowered by [`LowerHost::lower_other_expr`].
    Other,
}

/// The effect instruction an effect statement emits. Its operands are the
/// statement's arguments ([`LowerHost::stmt_arg`]), evaluated left to right.
pub(crate) enum EffectKind {
    /// Argument 0: the object; argument 1: the value.
    SetField(u32),
    /// Argument 0: the plotted value; argument `1 + i`: the color of
    /// `roles[i]`.
    Plot {
        slot: PlotSlot,
        roles: Vec<PlotColorRole>,
    },
    /// Argument `i`: the color of `roles[i]`.
    Visual {
        slot: VisualSlot,
        roles: Vec<PlotColorRole>,
    },
    /// Argument `i`: the color of `roles[i]`.
    Fill {
        slot: FillSlot,
        roles: Vec<PlotColorRole>,
    },
    /// Argument 0: the message.
    RuntimeError,
    /// Argument 0: the format string; then the substitution values.
    Log(LogLevel),
    /// Arguments 0, 1: message, frequency.
    Alert,
    /// Arguments 0, 1, 2: condition, title, message.
    AlertCondition,
    /// The operands in slot order (the VM broker loop pops them in reverse).
    Order(OrderKind),
}

/// How the kernel lowers one statement. Expressions are fetched with
/// [`LowerHost::stmt_expr`], argument lists with [`LowerHost::stmt_arg`],
/// statement lists with [`LowerHost::stmt_body`].
pub(crate) enum StmtShape {
    /// Expr 0: the value.
    Assign {
        symbol: SymbolId,
        init: bool,
    },
    /// A tuple-returning engine call stored into the targets' slots:
    /// argument `i` is the call's `i`-th runtime operand.
    Destructure {
        state: u32,
        arity: usize,
        targets: Vec<SymbolId>,
    },
    /// An effect statement with `arity` arguments.
    Effect {
        kind: EffectKind,
        arity: usize,
    },
    /// A statement that emits nothing where it stands (a hoisted risk
    /// directive; see [`LowerHost::prelude`]).
    Nop,
    /// Expr 0: the evaluated expression.
    Eval,
    /// Expr 0: the condition; body 0: then; body 1: else.
    If,
    /// Exprs 0, 1, 2: start, end, step; body 0: the loop body.
    For {
        counter: SymbolId,
        end_slot: SeriesSlot,
        step_slot: SeriesSlot,
        is_int: bool,
        implicit_step: bool,
    },
    /// Expr 0: the condition; body 0: the loop body.
    While,
    Break,
    Continue,
    /// Any other form: lowered by [`LowerHost::lower_other_stmt`].
    Other,
}

/// The view of the HIR the kernel needs. `E` is the expression type and `S`
/// the statement type.
pub(crate) trait LowerHost<E, S> {
    fn expr_shape(&self, expr: &E) -> ExprShape;
    /// Whether evaluating either ternary arm eagerly is observationally safe.
    /// Only literal arms and unary operators on literals qualify; every other
    /// ternary branches to a join block so the unselected arm never runs.
    fn ternary_arms_total(&self, expr: &E) -> bool;
    fn expr_child<'a>(&self, expr: &'a E, index: u32) -> &'a E;
    /// Argument `index` of a [`ExprShape::Call`], [`ExprShape::SetFields`]
    /// or [`ExprShape::SetCell`] node.
    fn expr_arg<'a>(&self, expr: &'a E, index: usize) -> &'a E;
    fn expr_span(&self, expr: &E) -> Span;
    fn literal_value(&self, expr: &E) -> Value;
    fn positive_literal_step(&self, _expr: &E) -> bool {
        false
    }
    fn lower_other_expr(&self, st: &mut LowerState, expr: &E) -> ValueId;
    fn stmt_shape(&self, stmt: &S) -> StmtShape;
    fn stmt_expr<'a>(&self, stmt: &'a S, index: u32) -> &'a E;
    /// Argument `index` of a [`StmtShape::Destructure`] or
    /// [`StmtShape::Effect`] statement, or of a prelude directive.
    fn stmt_arg<'a>(&self, stmt: &'a S, index: usize) -> &'a E;
    fn stmt_body<'a>(&self, stmt: &'a S, index: u32) -> &'a [S];
    fn stmt_span(&self, stmt: &S) -> Span;
    fn lower_other_stmt(&self, st: &mut LowerState, stmt: &S);
    /// The series slot of a symbol.
    fn slot_of(&self, symbol: SymbolId) -> SeriesSlot;
    /// Number of slots already assigned by HIR before MIR creates temporary
    /// branch-result slots.
    fn slot_count(&self) -> u32;
    /// Whether a symbol is `var`/`varip` (initialize-once).
    fn is_persistent(&self, symbol: SymbolId) -> bool;
    /// The hoisted risk directives, emitted into the entry block before the
    /// body.
    fn prelude(&self) -> &[S];
    /// The order kind and operand count of a prelude directive.
    fn prelude_order(&self, stmt: &S) -> (OrderKind, usize);
}

pub(crate) fn new_block(st: &mut LowerState) -> BlockId {
    push_block(&mut st.blocks)
}

pub(crate) fn new_value(st: &mut LowerState) -> ValueId {
    alloc_value_id(&mut st.next_value)
}

pub(crate) fn emit(st: &mut LowerState, instr: MirInstr) {
    emit_into(&mut st.blocks, st.current, instr);
}

pub(crate) fn terminate(st: &mut LowerState, terminator: Terminator) {
    terminate_block(&mut st.blocks, st.current, terminator);
}

pub(crate) fn emit_const(st: &mut LowerState, value: Value, span: Span) -> ValueId {
    let dst = new_value(st);
    emit(st, MirInstr::Const { dst, value, span });
    dst
}

/// Read a user series slot at offset 0 (the current bar's cell).
pub(crate) fn read_slot(st: &mut LowerState, slot: SeriesSlot, span: Span) -> ValueId {
    let dst = new_value(st);
    emit(
        st,
        MirInstr::ReadUserSeries {
            dst,
            slot,
            offset: 0,
            span,
        },
    );
    dst
}

/// Emit `dst = lhs op rhs` with float/non-string flags (the loop scaffolding,
/// `ta.change` and `line.get_price`).
fn binary_flags(
    st: &mut LowerState,
    op: BinOp,
    lhs: ValueId,
    rhs: ValueId,
    is_int: bool,
    span: Span,
) -> ValueId {
    let dst = new_value(st);
    emit(
        st,
        MirInstr::Binary {
            dst,
            op,
            lhs,
            rhs,
            is_int,
            is_string: false,
            span,
        },
    );
    dst
}

/// Emit `GetField` and return the resulting value.
fn get_field(st: &mut LowerState, obj: ValueId, field_idx: u32, span: Span) -> ValueId {
    let dst = new_value(st);
    emit(
        st,
        MirInstr::GetField {
            dst,
            obj,
            field_idx,
            span,
        },
    );
    dst
}

/// Read an int-typed field and widen it to float (bar indices in
/// `line.get_price`).
fn get_field_as_float(st: &mut LowerState, obj: ValueId, field_idx: u32, span: Span) -> ValueId {
    let v = get_field(st, obj, field_idx, span);
    let dst = new_value(st);
    emit(
        st,
        MirInstr::IntToFloat {
            dst,
            operand: v,
            span,
        },
    );
    dst
}

/// The instruction of a call-like expression. (`CallKind::Ta` builds its
/// one-element `dsts` with `push`: `vec![dst]` expands to a boxed-slice
/// conversion outside the Aeneas-extractable subset.)
#[allow(clippy::vec_init_then_push)]
fn call_instr(kind: CallKind, dst: ValueId, args: Vec<ValueId>, span: Span) -> MirInstr {
    match kind {
        CallKind::Pure(func) => MirInstr::CallPure {
            dst,
            func,
            args,
            span,
        },
        CallKind::Collection(func) => MirInstr::CallCollection {
            dst,
            func,
            args,
            span,
        },
        CallKind::Ta(state) => {
            let mut dsts = Vec::new();
            dsts.push(dst);
            MirInstr::CallTa {
                dsts,
                state,
                args,
                span,
            }
        }
        CallKind::NewObject(type_id) => MirInstr::NewObject {
            dst,
            type_id,
            fields: args,
            span,
        },
        CallKind::CopyObject(type_id) => MirInstr::CopyObject {
            dst,
            type_id,
            obj: args[0],
            span,
        },
        CallKind::GetField(field_idx) => MirInstr::GetField {
            dst,
            obj: args[0],
            field_idx,
            span,
        },
        CallKind::FootprintRowByPrice => MirInstr::FootprintRowByPrice {
            dst,
            footprint: args[0],
            price: args[1],
            span,
        },
        CallKind::RegisterDrawing(kind) => MirInstr::RegisterDrawing {
            dst,
            kind,
            obj: args[0],
            span,
        },
        CallKind::DeleteDrawing(kind) => MirInstr::DeleteDrawing {
            dst,
            kind,
            id: args[0],
            span,
        },
        CallKind::CallPivotPointLevels => MirInstr::CallPivotPointLevels { dst, args, span },
        CallKind::ReadSecurity {
            selector,
            source,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
        } => MirInstr::ReadSecurity {
            dst,
            selector,
            source,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
            span,
        },
        CallKind::ReadSecurityExpr {
            expr,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
        } => MirInstr::ReadSecurityExpr {
            dst,
            expr,
            htf_seconds,
            timeframe_expr,
            calendar_months,
            gaps_on,
            lookahead_on,
            span,
        },
        CallKind::ReadRequest { func, selector } => MirInstr::ReadRequest {
            dst,
            func,
            selector,
            span,
        },
        CallKind::BuildFootprint(selector) => MirInstr::BuildFootprint {
            dst,
            selector,
            span,
        },
        CallKind::ReadSecurityLowerTf(selector) => MirInstr::ReadSecurityLowerTf {
            dst,
            selector,
            span,
        },
        CallKind::ReadPivotLevels {
            system,
            anchor_seconds,
        } => MirInstr::ReadPivotLevels {
            dst,
            system,
            anchor_seconds,
            span,
        },
        CallKind::ReadTimeTf { close, tf_seconds } => MirInstr::ReadTimeTf {
            dst,
            close,
            tf_seconds,
            span,
        },
        CallKind::ReadDrawingsAll(kind) => MirInstr::ReadDrawingsAll { dst, kind, span },
    }
}

/// Pair `roles[i]` with `values[first + i]` (the color operands of a plot,
/// visual or fill).
fn color_pairs(
    roles: Vec<PlotColorRole>,
    values: &[ValueId],
    first: usize,
) -> Vec<(PlotColorRole, ValueId)> {
    let mut colors = Vec::new();
    let mut i = 0;
    while i < roles.len() {
        colors.push((roles[i], values[first + i]));
        i += 1;
    }
    colors
}

/// The instruction of an effect statement.
fn effect_instr(kind: EffectKind, args: Vec<ValueId>, span: Span) -> MirInstr {
    match kind {
        EffectKind::SetField(field_idx) => MirInstr::SetField {
            obj: args[0],
            field_idx,
            value: args[1],
            span,
        },
        EffectKind::Plot { slot, roles } => {
            let colors = color_pairs(roles, &args, 1);
            MirInstr::Plot {
                plot_slot: slot,
                value: args[0],
                colors,
                span,
            }
        }
        EffectKind::Visual { slot, roles } => MirInstr::Visual {
            visual_slot: slot,
            colors: color_pairs(roles, &args, 0),
            span,
        },
        EffectKind::Fill { slot, roles } => MirInstr::Fill {
            fill_slot: slot,
            colors: color_pairs(roles, &args, 0),
            span,
        },
        EffectKind::RuntimeError => MirInstr::RuntimeError {
            message: args[0],
            span,
        },
        EffectKind::Log(level) => MirInstr::Log {
            level,
            operands: args,
            span,
        },
        EffectKind::Alert => MirInstr::Alert {
            message: args[0],
            freq: args[1],
            span,
        },
        EffectKind::AlertCondition => MirInstr::AlertCondition {
            condition: args[0],
            title: args[1],
            message: args[2],
            span,
        },
        EffectKind::Order(kind) => MirInstr::Order {
            kind,
            operands: args,
            span,
        },
    }
}

/// Lower arguments `0..n` of an expression, left to right.
pub(crate) fn lower_args<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
    n: usize,
) -> Vec<ValueId> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < n {
        let v = lower_expr(host, st, host.expr_arg(expr, i));
        values.push(v);
        i += 1;
    }
    values
}

/// Lower arguments `0..n` of a statement, left to right.
pub(crate) fn lower_stmt_args<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    stmt: &S,
    n: usize,
) -> Vec<ValueId> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < n {
        let v = lower_expr(host, st, host.stmt_arg(stmt, i));
        values.push(v);
        i += 1;
    }
    values
}

/// Evaluate the receiver (child 0) once and capture it in its hidden slot.
fn capture_receiver<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
    recv_slot: SeriesSlot,
    span: Span,
) {
    let obj_v = lower_expr(host, st, host.expr_child(expr, 0));
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: recv_slot,
            value: obj_v,
            span,
        },
    );
}

/// The field writes of a `SetFields` node: re-read the captured receiver for
/// each write (so a side-effecting `<ns>.new(...)` receiver is not
/// re-evaluated).
fn lower_field_writes<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
    recv_slot: SeriesSlot,
    field_idxs: &[u32],
    span: Span,
) {
    let mut i = 0;
    while i < field_idxs.len() {
        let obj_read = read_slot(st, recv_slot, span);
        let value_v = lower_expr(host, st, host.expr_arg(expr, i));
        emit(
            st,
            MirInstr::SetField {
                obj: obj_read,
                field_idx: field_idxs[i],
                value: value_v,
                span,
            },
        );
        i += 1;
    }
}

/// `line.get_price`: evaluate the line handle once into a hidden slot;
/// re-read it for each of the four endpoint field reads (x1,y1,x2,y2),
/// coerce the int bar indices to float, and compute
/// `y1 + (x - x1) * (y2 - y1) / (x2 - x1)`. Emitted in strict stack order so
/// each binary's two operands are the top-of-stack pair when it runs.
fn lower_line_get_price<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
    recv_slot: SeriesSlot,
    span: Span,
) -> ValueId {
    capture_receiver(host, st, expr, recv_slot, span);
    // 1) y1 (kept at the bottom for the final Add).
    let obj_read = read_slot(st, recv_slot, span);
    let y1 = get_field(st, obj_read, 1, span);
    // 2) dx = x - x1
    let xf = lower_expr(host, st, host.expr_child(expr, 1));
    let obj_read = read_slot(st, recv_slot, span);
    let x1 = get_field_as_float(st, obj_read, 0, span);
    let dx = binary_flags(st, BinOp::Sub, xf, x1, false, span);
    // 3) dy = y2 - y1
    let obj_read = read_slot(st, recv_slot, span);
    let y2 = get_field(st, obj_read, 3, span);
    let obj_read = read_slot(st, recv_slot, span);
    let y1b = get_field(st, obj_read, 1, span);
    let dy = binary_flags(st, BinOp::Sub, y2, y1b, false, span);
    // 4) prod = dx * dy
    let prod = binary_flags(st, BinOp::Mul, dx, dy, false, span);
    // 5) span = x2 - x1
    let obj_read = read_slot(st, recv_slot, span);
    let x2 = get_field_as_float(st, obj_read, 2, span);
    let obj_read = read_slot(st, recv_slot, span);
    let x1b = get_field_as_float(st, obj_read, 0, span);
    let span_x = binary_flags(st, BinOp::Sub, x2, x1b, false, span);
    // 6) slope = prod / span; 7) y1 + slope
    let slope = binary_flags(st, BinOp::Div, prod, span_x, false, span);
    binary_flags(st, BinOp::Add, y1, slope, false, span)
}

/// `table.clear` / `table.merge_cells`: capture the receiver, read it back,
/// evaluate the four coordinates in source order, then the op.
fn lower_cells<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
    recv_slot: SeriesSlot,
    merge: bool,
    span: Span,
) -> ValueId {
    capture_receiver(host, st, expr, recv_slot, span);
    let obj_read = read_slot(st, recv_slot, span);
    let sc = lower_expr(host, st, host.expr_child(expr, 1));
    let sr = lower_expr(host, st, host.expr_child(expr, 2));
    let ec = lower_expr(host, st, host.expr_child(expr, 3));
    let er = lower_expr(host, st, host.expr_child(expr, 4));
    let dst = new_value(st);
    if merge {
        emit(
            st,
            MirInstr::MergeCells {
                dst,
                obj: obj_read,
                start_column: sc,
                start_row: sr,
                end_column: ec,
                end_row: er,
                span,
            },
        );
    } else {
        emit(
            st,
            MirInstr::ClearCells {
                dst,
                obj: obj_read,
                start_column: sc,
                start_row: sr,
                end_column: ec,
                end_row: er,
                span,
            },
        );
    }
    dst
}

/// Lower one expression; its value is left in a fresh register.
#[allow(clippy::too_many_lines)]
pub(crate) fn lower_expr<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
) -> ValueId {
    let span = host.expr_span(expr);
    match host.expr_shape(expr) {
        ExprShape::Literal => {
            let value = host.literal_value(expr);
            emit_const(st, value, span)
        }
        ExprShape::ReadBuiltin { series, offset } => {
            let dst = new_value(st);
            emit(
                st,
                MirInstr::ReadBuiltinSeries {
                    dst,
                    source: series,
                    offset,
                    span,
                },
            );
            dst
        }
        ExprShape::ReadVar { symbol, offset } => {
            let dst = new_value(st);
            let slot = host.slot_of(symbol);
            emit(
                st,
                MirInstr::ReadUserSeries {
                    dst,
                    slot,
                    offset,
                    span,
                },
            );
            dst
        }
        ExprShape::ReadBuiltinDynamic { series } => {
            let offset_value = lower_expr(host, st, host.expr_child(expr, 0));
            let dst = new_value(st);
            emit(
                st,
                MirInstr::ReadBuiltinSeriesDynamic {
                    dst,
                    source: series,
                    offset: offset_value,
                    span,
                },
            );
            dst
        }
        ExprShape::ReadVarDynamic { symbol } => {
            let offset_value = lower_expr(host, st, host.expr_child(expr, 0));
            let dst = new_value(st);
            let slot = host.slot_of(symbol);
            emit(
                st,
                MirInstr::ReadUserSeriesDynamic {
                    dst,
                    slot,
                    offset: offset_value,
                    span,
                },
            );
            dst
        }
        ExprShape::ReadInput { input } => {
            let dst = new_value(st);
            emit(st, MirInstr::ReadInput { dst, input, span });
            dst
        }
        ExprShape::Unary { op, is_int } => {
            let operand_value = lower_expr(host, st, host.expr_child(expr, 0));
            let dst = new_value(st);
            emit(
                st,
                MirInstr::Unary {
                    dst,
                    op,
                    operand: operand_value,
                    is_int,
                    span,
                },
            );
            dst
        }
        ExprShape::Binary {
            op,
            is_int,
            is_string,
        } => {
            let lhs_value = lower_expr(host, st, host.expr_child(expr, 0));
            // Pine Script evaluates `and`/`or` lazily: when the lhs alone
            // decides (`false and …`, `true or …`) the rhs must not run.
            // The guard jumps over the rhs instructions and the combining
            // Binary below; an na lhs falls through so the strong-Kleene
            // combine still sees the rhs.
            if is_lazy(op) {
                emit(
                    st,
                    MirInstr::LazyBoolGuard {
                        op,
                        lhs: lhs_value,
                        span,
                    },
                );
            }
            let rhs_value = lower_expr(host, st, host.expr_child(expr, 1));
            let dst = new_value(st);
            emit(
                st,
                MirInstr::Binary {
                    dst,
                    op,
                    lhs: lhs_value,
                    rhs: rhs_value,
                    is_int,
                    is_string,
                    span,
                },
            );
            dst
        }
        ExprShape::IntToFloat => {
            let operand_value = lower_expr(host, st, host.expr_child(expr, 0));
            let dst = new_value(st);
            emit(
                st,
                MirInstr::IntToFloat {
                    dst,
                    operand: operand_value,
                    span,
                },
            );
            dst
        }
        ExprShape::Ternary => {
            let cond_value = lower_expr(host, st, host.expr_child(expr, 0));
            if host.ternary_arms_total(expr) {
                let lhs = lower_expr(host, st, host.expr_child(expr, 1));
                let rhs = lower_expr(host, st, host.expr_child(expr, 2));
                let dst = new_value(st);
                emit(
                    st,
                    MirInstr::Select {
                        dst,
                        cond: cond_value,
                        lhs,
                        rhs,
                        span: host.expr_span(expr),
                    },
                );
                return dst;
            }
            let then_block = new_block(st);
            let else_block = new_block(st);
            let join_block = new_block(st);
            let result_slot = SeriesSlot(st.next_temp_slot);
            st.next_temp_slot += 1;
            st.temp_slot_count += 1;
            let span = host.expr_span(expr);
            terminate(
                st,
                Terminator::Branch {
                    cond: cond_value,
                    then_block,
                    else_block,
                    span,
                },
            );

            st.current = then_block;
            let then_value = lower_expr(host, st, host.expr_child(expr, 1));
            emit(
                st,
                MirInstr::StoreUserSeriesCurrent {
                    slot: result_slot,
                    value: then_value,
                    span,
                },
            );
            terminate(
                st,
                Terminator::Jump {
                    target: join_block,
                    span,
                },
            );

            st.current = else_block;
            let else_value = lower_expr(host, st, host.expr_child(expr, 2));
            emit(
                st,
                MirInstr::StoreUserSeriesCurrent {
                    slot: result_slot,
                    value: else_value,
                    span,
                },
            );
            terminate(
                st,
                Terminator::Jump {
                    target: join_block,
                    span,
                },
            );

            st.current = join_block;
            let dst = new_value(st);
            emit(
                st,
                MirInstr::ReadUserSeries {
                    dst,
                    slot: result_slot,
                    offset: 0,
                    span,
                },
            );
            dst
        }
        ExprShape::MaterializeHistory { slot, offset } => {
            // Store the evaluated expression into the hidden slot for the
            // current bar, then read the slot at the offset.
            let value = lower_expr(host, st, host.expr_child(expr, 0));
            emit(st, MirInstr::StoreUserSeriesCurrent { slot, value, span });
            let dst = new_value(st);
            emit(
                st,
                MirInstr::ReadUserSeries {
                    dst,
                    slot,
                    offset,
                    span,
                },
            );
            dst
        }
        ExprShape::MaterializeHistoryDynamic { slot } => {
            let value = lower_expr(host, st, host.expr_child(expr, 0));
            emit(st, MirInstr::StoreUserSeriesCurrent { slot, value, span });
            let offset_value = lower_expr(host, st, host.expr_child(expr, 1));
            let dst = new_value(st);
            emit(
                st,
                MirInstr::ReadUserSeriesDynamic {
                    dst,
                    slot,
                    offset: offset_value,
                    span,
                },
            );
            dst
        }
        ExprShape::TaChange { slot, length } => {
            // Every slot-based ta call site first materializes its (float)
            // source into its own hidden slot for the current bar, then
            // computes the function's value from the slot's history.
            // ta.change: source now minus source `length` bars ago; the
            // float subtraction propagates na from either side.
            let value = lower_expr(host, st, host.expr_child(expr, 0));
            emit(st, MirInstr::StoreUserSeriesCurrent { slot, value, span });
            let now = read_slot(st, slot, span);
            let past = new_value(st);
            emit(
                st,
                MirInstr::ReadUserSeries {
                    dst: past,
                    slot,
                    offset: length,
                    span,
                },
            );
            binary_flags(st, BinOp::Sub, now, past, false, span)
        }
        ExprShape::TaSma { slot, length } => {
            // ta.sma: the dedicated windowed-mean instruction.
            let value = lower_expr(host, st, host.expr_child(expr, 0));
            emit(st, MirInstr::StoreUserSeriesCurrent { slot, value, span });
            let dst = new_value(st);
            emit(
                st,
                MirInstr::WindowMean {
                    dst,
                    slot,
                    length,
                    span,
                },
            );
            dst
        }
        ExprShape::Call { kind, arity } => {
            let args = lower_args(host, st, expr, arity);
            let dst = new_value(st);
            emit(st, call_instr(kind, dst, args, span));
            dst
        }
        ExprShape::SetFieldExpr { field_idx } => {
            // Evaluate the object handle first, then the value (source
            // order), write the field, then push a placeholder `na` so the
            // void setter has a value for the enclosing discard.
            let obj_v = lower_expr(host, st, host.expr_child(expr, 0));
            let value_v = lower_expr(host, st, host.expr_child(expr, 1));
            emit(
                st,
                MirInstr::SetField {
                    obj: obj_v,
                    field_idx,
                    value: value_v,
                    span,
                },
            );
            emit_const(st, super::na_value(), span)
        }
        ExprShape::SetFields {
            recv_slot,
            field_idxs,
        } => {
            // Evaluate the receiver ONCE and capture it in a hidden slot;
            // re-read the handle for each field write. Void: push a
            // placeholder `na`.
            capture_receiver(host, st, expr, recv_slot, span);
            lower_field_writes(host, st, expr, recv_slot, &field_idxs, span);
            emit_const(st, super::na_value(), span)
        }
        ExprShape::LineGetPrice { recv_slot } => {
            lower_line_get_price(host, st, expr, recv_slot, span)
        }
        ExprShape::SetCell { recv_slot, attrs } => {
            // Evaluate the receiver ONCE into a hidden slot, then read it
            // back for the SetCell op (so a `table.new(...)` receiver is not
            // re-registered). column/row/values evaluate in source order.
            capture_receiver(host, st, expr, recv_slot, span);
            let obj_read = read_slot(st, recv_slot, span);
            let column_v = lower_expr(host, st, host.expr_child(expr, 1));
            let row_v = lower_expr(host, st, host.expr_child(expr, 2));
            let values = lower_args(host, st, expr, attrs.len());
            let dst = new_value(st);
            emit(
                st,
                MirInstr::SetCell {
                    dst,
                    obj: obj_read,
                    column: column_v,
                    row: row_v,
                    attrs,
                    values,
                    span,
                },
            );
            dst
        }
        ExprShape::Cells { recv_slot, merge } => {
            lower_cells(host, st, expr, recv_slot, merge, span)
        }
        ExprShape::Other => host.lower_other_expr(st, expr),
    }
}

/// `and`/`or`, the operators with a lazy guard.
fn is_lazy(op: BinOp) -> bool {
    matches!(op, BinOp::And | BinOp::Or)
}

/// Lower a statement list in order.
pub(crate) fn lower_stmts<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState, stmts: &[S]) {
    let mut i = 0;
    while i < stmts.len() {
        lower_stmt(host, st, &stmts[i]);
        i += 1;
    }
}

/// Lower an effect statement (or prelude directive): its arguments, then the
/// instruction.
fn lower_effect<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    stmt: &S,
    kind: EffectKind,
    arity: usize,
    span: Span,
) {
    let args = lower_stmt_args(host, st, stmt, arity);
    emit(st, effect_instr(kind, args, span));
}

/// A tuple-returning engine call: one multi-push `CallTa` producing N stack
/// values (pushed left to right), then one store per target. Stores pop from
/// the top of the stack, so the *last* target is stored first (its value,
/// `dsts[N-1]`, is on top) down to the first.
fn lower_destructure<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    stmt: &S,
    state: u32,
    arity: usize,
    targets: &[SymbolId],
    span: Span,
) {
    let args = lower_stmt_args(host, st, stmt, arity);
    let mut dsts = Vec::new();
    let mut stored = Vec::new();
    let mut i = 0;
    while i < targets.len() {
        let dst = new_value(st);
        dsts.push(dst);
        stored.push(dst);
        i += 1;
    }
    emit(
        st,
        MirInstr::CallTa {
            dsts,
            state,
            args,
            span,
        },
    );
    let mut j = targets.len();
    while j > 0 {
        j -= 1;
        let slot = host.slot_of(targets[j]);
        emit(
            st,
            MirInstr::StoreUserSeriesCurrent {
                slot,
                value: stored[j],
                span,
            },
        );
    }
}

/// Lower one statement.
#[allow(clippy::too_many_lines)]
pub(crate) fn lower_stmt<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState, stmt: &S) {
    let span = host.stmt_span(stmt);
    match host.stmt_shape(stmt) {
        StmtShape::Assign { symbol, init } => {
            let slot = host.slot_of(symbol);
            if init && host.is_persistent(symbol) {
                // `var x = e`: initialize-once. Branch on the slot's
                // written flag; only the unwritten path evaluates the
                // initializer, so a `var` initializer inside an `if`
                // branch runs the first time the statement actually
                // executes, which may be a later bar.
                let unwritten_block = new_block(st);
                let written_block = new_block(st);
                let join_block = new_block(st);

                terminate(
                    st,
                    Terminator::BranchIfSlotWritten {
                        slot,
                        written_block,
                        unwritten_block,
                        span,
                    },
                );

                st.current = unwritten_block;
                let v = lower_expr(host, st, host.stmt_expr(stmt, 0));
                emit(
                    st,
                    MirInstr::StoreUserSeriesCurrent {
                        slot,
                        value: v,
                        span,
                    },
                );
                terminate(
                    st,
                    Terminator::Jump {
                        target: join_block,
                        span,
                    },
                );

                st.current = written_block;
                terminate(
                    st,
                    Terminator::Jump {
                        target: join_block,
                        span,
                    },
                );

                st.current = join_block;
            } else {
                // Plain declaration or `:=` reassignment: a store into
                // the current bar's cell.
                let v = lower_expr(host, st, host.stmt_expr(stmt, 0));
                emit(
                    st,
                    MirInstr::StoreUserSeriesCurrent {
                        slot,
                        value: v,
                        span,
                    },
                );
            }
        }
        StmtShape::Destructure {
            state,
            arity,
            targets,
        } => lower_destructure(host, st, stmt, state, arity, &targets, span),
        StmtShape::Effect { kind, arity } => lower_effect(host, st, stmt, kind, arity, span),
        StmtShape::Nop => {}
        StmtShape::Eval => {
            let v = lower_expr(host, st, host.stmt_expr(stmt, 0));
            emit(st, MirInstr::Discard { value: v, span });
        }
        StmtShape::If => {
            let cond_value = lower_expr(host, st, host.stmt_expr(stmt, 0));
            let then_block = new_block(st);
            let else_block = new_block(st);
            let join_block = new_block(st);

            terminate(
                st,
                Terminator::Branch {
                    cond: cond_value,
                    then_block,
                    else_block,
                    span,
                },
            );

            st.current = then_block;
            lower_stmts(host, st, host.stmt_body(stmt, 0));
            terminate(
                st,
                Terminator::Jump {
                    target: join_block,
                    span,
                },
            );

            st.current = else_block;
            lower_stmts(host, st, host.stmt_body(stmt, 1));
            terminate(
                st,
                Terminator::Jump {
                    target: join_block,
                    span,
                },
            );

            st.current = join_block;
        }
        StmtShape::For {
            counter,
            end_slot,
            step_slot,
            is_int,
            implicit_step,
        } => lower_for(
            host,
            st,
            stmt,
            counter,
            end_slot,
            step_slot,
            is_int,
            implicit_step,
            span,
        ),
        StmtShape::While => lower_while(host, st, stmt, span),
        StmtShape::Break => {
            // Jump to the innermost loop's exit block. Semantics
            // guarantees a `break` only reaches here inside a loop.
            let n = st.loops.len();
            let () = assert!(n != 0, "break outside a loop reached MIR");
            let target = st.loops[n - 1].break_target;
            terminate(st, Terminator::Jump { target, span });
            // Statements after an unconditional `break` in the same block
            // are dead; lower them into a fresh unreachable block so the
            // stack-lowering invariants still hold.
            let dead = new_block(st);
            st.current = dead;
        }
        StmtShape::Continue => {
            let n = st.loops.len();
            let () = assert!(n != 0, "continue outside a loop reached MIR");
            let target = st.loops[n - 1].continue_target;
            terminate(st, Terminator::Jump { target, span });
            let dead = new_block(st);
            st.current = dead;
        }
        StmtShape::Other => host.lower_other_stmt(st, stmt),
    }
}

/// Lower `for counter = start to end [by step]` into a header/body/step/
/// exit block graph with a back-edge from the step block to the header.
/// No new opcodes: the counter, `end`, and `step` are ordinary user
/// series slots; the loop uses comparisons + `Select` + `Jump`/`Branch`.
#[allow(clippy::too_many_arguments)]
fn lower_for<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    stmt: &S,
    counter: SymbolId,
    end_slot: SeriesSlot,
    step_slot: SeriesSlot,
    is_int: bool,
    implicit_step: bool,
    span: Span,
) {
    let counter_slot = host.slot_of(counter);

    // Entry: evaluate start and the initial end once. The initial end is
    // retained for implicit-step direction selection; the stopping boundary
    // is re-evaluated at the top of every iteration.
    let start_v = lower_expr(host, st, host.stmt_expr(stmt, 0));
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: counter_slot,
            value: start_v,
            span,
        },
    );
    let end_v = lower_expr(host, st, host.stmt_expr(stmt, 1));
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: end_slot,
            value: end_v,
            span,
        },
    );

    // Step: an explicit `by` value, or the implicit `+1`/`-1` selected by
    // the direction of the captured bounds (`start <= end ? +1 : -1`).
    let step_v = if implicit_step {
        let start_read = read_slot(st, counter_slot, span);
        let end_read = read_slot(st, end_slot, span);
        let asc = binary_flags(st, BinOp::Le, start_read, end_read, false, span);
        let one = emit_const(st, super::one_value(is_int), span);
        let neg_one = emit_const(st, super::neg_one_value(is_int), span);
        let sel = new_value(st);
        emit(
            st,
            MirInstr::Select {
                dst: sel,
                cond: asc,
                lhs: one,
                rhs: neg_one,
                span,
            },
        );
        sel
    } else {
        let step_expr = host.stmt_expr(stmt, 2);
        let magnitude = lower_expr(host, st, step_expr);
        emit(
            st,
            MirInstr::StoreUserSeriesCurrent {
                slot: step_slot,
                value: magnitude,
                span,
            },
        );
        // Report nonpositive dynamic steps only when this loop executes.
        // Literal positive steps need no guard, which also keeps ordinary
        // loops AOT eligible.
        if !host.positive_literal_step(step_expr) {
            let guard_magnitude = read_slot(st, step_slot, span);
            let zero = emit_const(st, super::zero_value(is_int), span);
            let positive = binary_flags(st, BinOp::Gt, guard_magnitude, zero, false, span);
            let valid = new_block(st);
            let invalid = new_block(st);
            terminate(
                st,
                Terminator::Branch {
                    cond: positive,
                    then_block: valid,
                    else_block: invalid,
                    span,
                },
            );
            st.current = invalid;
            let message = emit_const(st, super::loop_step_error_value(), span);
            emit(st, MirInstr::RuntimeError { message, span });
            terminate(
                st,
                Terminator::Jump {
                    target: valid,
                    span,
                },
            );
            st.current = valid;
        }
        let start_read = read_slot(st, counter_slot, span);
        let end_read = read_slot(st, end_slot, span);
        let ascending = binary_flags(st, BinOp::Le, start_read, end_read, false, span);
        let magnitude = read_slot(st, step_slot, span);
        let neg_input = read_slot(st, step_slot, span);
        let negative = new_value(st);
        emit(
            st,
            MirInstr::Unary {
                dst: negative,
                op: UnOp::Neg,
                operand: neg_input,
                is_int,
                span,
            },
        );
        let signed = new_value(st);
        emit(
            st,
            MirInstr::Select {
                dst: signed,
                cond: ascending,
                lhs: magnitude,
                rhs: negative,
                span,
            },
        );
        signed
    };
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: step_slot,
            value: step_v,
            span,
        },
    );

    let header = new_block(st);
    let body_block = new_block(st);
    let step_block = new_block(st);
    let exit = new_block(st);

    terminate(
        st,
        Terminator::Jump {
            target: header,
            span,
        },
    );

    // Header: evaluate the stopping boundary on every iteration, then run
    // while the counter has not passed it, with the direction taken from the step's sign
    // (`step >= 0 ? counter <= end : counter >= end`). An `na` bound makes
    // the comparison `na`, which `Branch` treats as false → the loop
    // exits, matching `if`.
    st.current = header;
    let end_now = lower_expr(host, st, host.stmt_expr(stmt, 1));
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: end_slot,
            value: end_now,
            span,
        },
    );
    let step_read = read_slot(st, step_slot, span);
    let zero = emit_const(st, super::zero_value(is_int), span);
    let ascending = binary_flags(st, BinOp::Ge, step_read, zero, false, span);
    let counter_read_up = read_slot(st, counter_slot, span);
    let end_read_up = read_slot(st, end_slot, span);
    let le = binary_flags(st, BinOp::Le, counter_read_up, end_read_up, false, span);
    let counter_read_down = read_slot(st, counter_slot, span);
    let end_read_down = read_slot(st, end_slot, span);
    let ge = binary_flags(st, BinOp::Ge, counter_read_down, end_read_down, false, span);
    let cond = new_value(st);
    emit(
        st,
        MirInstr::Select {
            dst: cond,
            cond: ascending,
            lhs: le,
            rhs: ge,
            span,
        },
    );
    terminate(
        st,
        Terminator::Branch {
            cond,
            then_block: body_block,
            else_block: exit,
            span,
        },
    );

    // Body: `continue` jumps to the step block, `break` to the exit.
    st.current = body_block;
    st.loops.push(LoopTargets {
        continue_target: step_block,
        break_target: exit,
    });
    lower_stmts(host, st, host.stmt_body(stmt, 0));
    st.loops.pop();
    terminate(
        st,
        Terminator::Jump {
            target: step_block,
            span,
        },
    );

    // Step: advance the counter by the captured step, then loop back.
    st.current = step_block;
    let counter_now = read_slot(st, counter_slot, span);
    let step_now = read_slot(st, step_slot, span);
    let next = binary_flags(st, BinOp::Add, counter_now, step_now, is_int, span);
    emit(
        st,
        MirInstr::StoreUserSeriesCurrent {
            slot: counter_slot,
            value: next,
            span,
        },
    );
    terminate(
        st,
        Terminator::Jump {
            target: header,
            span,
        },
    );

    st.current = exit;
}

/// Lower `while cond` into a header/body/exit block graph with a back-edge
/// from the body to the header.
fn lower_while<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState, stmt: &S, span: Span) {
    let header = new_block(st);
    let body_block = new_block(st);
    let exit = new_block(st);

    terminate(
        st,
        Terminator::Jump {
            target: header,
            span,
        },
    );

    st.current = header;
    let cond_v = lower_expr(host, st, host.stmt_expr(stmt, 0));
    terminate(
        st,
        Terminator::Branch {
            cond: cond_v,
            then_block: body_block,
            else_block: exit,
            span,
        },
    );

    st.current = body_block;
    st.loops.push(LoopTargets {
        continue_target: header,
        break_target: exit,
    });
    lower_stmts(host, st, host.stmt_body(stmt, 0));
    st.loops.pop();
    terminate(
        st,
        Terminator::Jump {
            target: header,
            span,
        },
    );

    st.current = exit;
}

/// Emit the hoisted risk directives (`LowerHost::prelude`) in order: each is
/// an `Order` whose operands evaluate left to right.
pub(crate) fn lower_prelude<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState) {
    let directives = host.prelude();
    let mut i = 0;
    while i < directives.len() {
        let d = &directives[i];
        let (kind, arity) = host.prelude_order(d);
        let span = host.stmt_span(d);
        lower_effect(host, st, d, EffectKind::Order(kind), arity, span);
        i += 1;
    }
}

/// Convert the finished builders into MIR blocks; every block must have been
/// terminated.
pub(crate) fn finish_blocks(blocks: Vec<BlockBuilder>) -> Vec<MirBlock> {
    let mut out = Vec::new();
    for b in blocks {
        // `let ... else panic!` rather than `Option::expect`: same panic message, and it keeps
        // the message out of the extracted code (Aeneas encodes `expect` string literals with a
        // `native_decide` proof).
        let Some(terminator) = b.terminator else {
            panic!("every block is terminated")
        };
        out.push(MirBlock {
            id: b.id,
            instrs: b.instrs,
            terminator,
        });
    }
    out
}

/// Lower a whole program body: a fresh entry block, the hoisted risk
/// directives, the statements, and a final `Return` spanning the last
/// statement. Returns the blocks, the entry block and the number of values
/// allocated.
pub(crate) fn lower_body<E, S, H: LowerHost<E, S>>(
    host: &H,
    body: &[S],
) -> (Vec<MirBlock>, BlockId, u32, u32) {
    let mut st = LowerState {
        blocks: Vec::new(),
        current: BlockId(0),
        next_value: 0,
        next_temp_slot: host.slot_count(),
        temp_slot_count: 0,
        loops: Vec::new(),
    };
    let entry = new_block(&mut st);
    st.current = entry;
    lower_prelude(host, &mut st);
    lower_stmts(host, &mut st, body);
    let n = body.len();
    let end_span = if n == 0 {
        Span::dummy()
    } else {
        host.stmt_span(&body[n - 1])
    };
    terminate(&mut st, Terminator::Return { span: end_span });
    let LowerState {
        blocks,
        next_value,
        temp_slot_count,
        ..
    } = st;
    (finish_blocks(blocks), entry, next_value, temp_slot_count)
}
