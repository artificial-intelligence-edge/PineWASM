//! Charon/Aeneas extraction roots for the MIR lowering code (the MIR builder
//! produces admissible MIR, and its allocator operations are
//! the allocator kernels). Compiled only with the `aeneas`
//! feature. Each root is a thin free-function wrapper around a function of
//! `lower::kernel`; root names differ from their callees to avoid name
//! capture.

use crate::lower::kernel::{
    emit, finish_blocks, lower_body, lower_expr, lower_prelude, lower_stmt, lower_stmts, new_block,
    new_value, terminate, LowerHost, LowerState,
};
use crate::lower::BlockBuilder;
use crate::{BlockId, MirBlock, MirInstr, Terminator, ValueId};

/// `lower`'s block construction (`kernel::lower_body`).
pub(crate) fn root_lower_body<E, S, H: LowerHost<E, S>>(
    host: &H,
    body: &[S],
) -> (Vec<MirBlock>, BlockId, u32, u32) {
    lower_body(host, body)
}

/// `kernel::lower_stmts`.
pub(crate) fn root_lower_stmts<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    stmts: &[S],
) {
    lower_stmts(host, st, stmts);
}

/// `kernel::lower_stmt`.
pub(crate) fn root_lower_stmt<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState, stmt: &S) {
    lower_stmt(host, st, stmt);
}

/// `kernel::lower_expr`.
pub(crate) fn root_lower_expr<E, S, H: LowerHost<E, S>>(
    host: &H,
    st: &mut LowerState,
    expr: &E,
) -> ValueId {
    lower_expr(host, st, expr)
}

/// `kernel::lower_prelude`.
pub(crate) fn root_lower_prelude<E, S, H: LowerHost<E, S>>(host: &H, st: &mut LowerState) {
    lower_prelude(host, st);
}

/// `kernel::finish_blocks`.
pub(crate) fn root_finish_blocks(blocks: Vec<BlockBuilder>) -> Vec<MirBlock> {
    finish_blocks(blocks)
}

/// The builder's allocator operations (`kernel::new_value`), for the replay
/// against the allocator model.
pub(crate) fn root_new_value(st: &mut LowerState) -> ValueId {
    new_value(st)
}

/// `kernel::new_block`.
pub(crate) fn root_new_block(st: &mut LowerState) -> BlockId {
    new_block(st)
}

/// `kernel::emit`.
pub(crate) fn root_emit(st: &mut LowerState, instr: MirInstr) {
    emit(st, instr);
}

/// `kernel::terminate`.
pub(crate) fn root_terminate(st: &mut LowerState, terminator: Terminator) {
    terminate(st, terminator);
}
