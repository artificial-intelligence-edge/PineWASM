//! Aeneas extraction roots for the `CsilStackify` extraction (MIR-to-CSIL
//! stackification; `--features aeneas` only).
//!
//! Each function is a thin, behavior-free wrapper whose only job is to make
//! Charon translate the real code it calls: Charon (pinned revision) cannot
//! start a translation from an inherent method. Production builds never
//! enable the feature, so this file is not compiled there.
//!
//! Extraction boundary: `const_instr` (MIR `Value` constants, whose
//! payloads carry `f64` and `String`, and the constant-pool string
//! interning) is meant to be marked opaque together with
//! `pinewasm_mir::Value` and `pinewasm_hir::HtfExpr`.

use super::{apply_fixups, stackify, LowerCtx, LowerError};
use crate::CsilInstr;
use pinewasm_hir::PlotColorRole;
use pinewasm_mir::{MirBlock, MirInstr, Terminator, ValueId};
use pinewasm_span::Span;

/// Root for `stackify`: lower every block, jump targets still unpatched.
///
/// # Errors
///
/// Every [`LowerError`] that `stackify` reports.
pub fn root_stackify(blocks: &[MirBlock]) -> Result<LowerCtx, LowerError> {
    stackify(blocks)
}

/// Root for `apply_fixups`: patch the recorded jump targets.
#[must_use]
pub fn root_apply_fixups(
    code: &mut [CsilInstr],
    block_starts: &[u32],
    fixups: &[(u32, u32)],
) -> Option<usize> {
    apply_fixups(code, block_starts, fixups)
}

/// Root for the inherent method `LowerCtx::new`.
#[must_use]
pub fn root_new_ctx(block_count: usize) -> LowerCtx {
    LowerCtx::new(block_count)
}

/// Root for the inherent method `LowerCtx::lower_block`.
///
/// # Errors
///
/// Every [`LowerError`] that `lower_block` reports.
pub fn root_lower_block(ctx: &mut LowerCtx, block: &MirBlock) -> Result<(), LowerError> {
    ctx.lower_block(block)
}

/// Root for the inherent method `LowerCtx::lower_instr`.
///
/// # Errors
///
/// Every [`LowerError`] that `lower_instr` reports.
pub fn root_lower_instr(ctx: &mut LowerCtx, instr: &MirInstr) -> Result<(), LowerError> {
    ctx.lower_instr(instr)
}

/// Root for the inherent method `LowerCtx::lower_terminator`.
///
/// # Errors
///
/// Every [`LowerError`] that `lower_terminator` reports.
pub fn root_lower_terminator(
    ctx: &mut LowerCtx,
    terminator: &Terminator,
) -> Result<(), LowerError> {
    ctx.lower_terminator(terminator)
}

/// Root for the inherent method `LowerCtx::check_balanced`.
///
/// # Errors
///
/// Every [`LowerError`] that `check_balanced` reports.
pub fn root_check_balanced(ctx: &LowerCtx) -> Result<(), LowerError> {
    ctx.check_balanced()
}

/// Root for the inherent method `LowerCtx::emit`.
pub fn root_emit(ctx: &mut LowerCtx, instr: CsilInstr, span: Span) {
    ctx.emit(instr, span);
}

/// Root for the inherent method `LowerCtx::push_value`.
pub fn root_push_value(ctx: &mut LowerCtx, value: ValueId) {
    ctx.push_value(value);
}

/// Root for the inherent method `LowerCtx::pop_value`.
///
/// # Errors
///
/// [`LowerError::StackMismatch`], as `pop_value` reports it.
pub fn root_pop_value(ctx: &mut LowerCtx, expected: ValueId, span: Span) -> Result<(), LowerError> {
    ctx.pop_value(expected, span)
}

/// Root for the inherent method `LowerCtx::pop_values_rev`.
///
/// # Errors
///
/// [`LowerError::StackMismatch`], as `pop_values_rev` reports it.
pub fn root_pop_values_rev(
    ctx: &mut LowerCtx,
    values: &[ValueId],
    span: Span,
) -> Result<(), LowerError> {
    ctx.pop_values_rev(values, span)
}

/// Root for the inherent method `LowerCtx::pop_colors_rev`.
///
/// # Errors
///
/// [`LowerError::StackMismatch`], as `pop_colors_rev` reports it.
pub fn root_pop_colors_rev(
    ctx: &mut LowerCtx,
    colors: &[(PlotColorRole, ValueId)],
    span: Span,
) -> Result<(), LowerError> {
    ctx.pop_colors_rev(colors, span)
}

/// Root for the inherent method `LowerCtx::take_guard`.
pub fn root_take_guard(ctx: &mut LowerCtx, lhs: ValueId) -> Option<u32> {
    ctx.take_guard(lhs)
}
