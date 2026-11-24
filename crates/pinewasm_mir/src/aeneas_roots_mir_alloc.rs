//! Charon/Aeneas extraction roots for the MIR allocator kernels (allocator
//! freshness, `color_hex` alpha). Compiled only with the `aeneas`
//! feature. Each root is a thin free-function wrapper so the kernels (and the
//! inherent `LowerCtx` methods' behavior they carry) can be named with
//! `--start-from`; root names differ from their callees to avoid name capture.

use crate::lower::{
    alloc_value_id, clamp_channel, emit_into, push_block, terminate_block, BlockBuilder,
};
use crate::value::color_alpha;
use crate::{BlockId, ValueId};

/// `LowerCtx::new_value`.
pub(crate) fn root_new_value(next_value: &mut u32) -> ValueId {
    alloc_value_id(next_value)
}

/// `LowerCtx::new_block`.
pub(crate) fn root_new_block<I, T>(blocks: &mut Vec<BlockBuilder<I, T>>) -> BlockId {
    push_block(blocks)
}

/// `LowerCtx::emit`.
pub(crate) fn root_emit<I, T>(blocks: &mut [BlockBuilder<I, T>], current: BlockId, instr: I) {
    emit_into(blocks, current, instr);
}

/// `LowerCtx::terminate`.
pub(crate) fn root_terminate<I, T>(
    blocks: &mut [BlockBuilder<I, T>],
    current: BlockId,
    terminator: T,
) {
    terminate_block(blocks, current, terminator);
}

/// Integer core of the literal color-channel clamp in `literal_color_constructor`.
pub(crate) fn root_clamp_channel(value: i64, max: i64) -> u8 {
    clamp_channel(value, max)
}

/// Integer alpha core of `Value::color_hex`.
pub(crate) fn root_color_alpha(t: u8) -> Option<u8> {
    color_alpha(t)
}
