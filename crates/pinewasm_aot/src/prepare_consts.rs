//! Pure derivation of every layout immediate `emit_prepare` bakes into the
//! `prepare` function.
//!
//! [`prepare_consts`] maps the verified [`LayoutFootprint`] coefficients to
//! the `i32.const` immediates of `prepare`'s fixed instruction template, and
//! [`lane_offset`] derives the per-lane offsets inside a per-bar region. Both
//! check every step instead of reproducing Wasm i32 wrap, so an immediate is
//! either exactly the footprint coefficient the bar cap was derived from or
//! the plan is rejected. `emit_prepare` only splices these values.
//!
//! Like `layout_footprint`, this module has no `wasm-encoder`, `CsilProgram`
//! or `AotError` dependency and stays inside the subset of Rust that Aeneas
//! can translate to Lean: no floats, closures, iterators, `?`, or
//! `Option`/`Result` combinators. The `AotError` mapping lives in `emit.rs`.

use crate::layout_footprint::LayoutFootprint;

/// `i32::MAX` as a `u64`, spelled as a literal rather than a cast.
const I32_MAX: u64 = 0x7FFF_FFFF;

/// Why a layout cannot be expressed as `prepare` immediates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareConstsError {
    /// A coefficient (or the written-flag alignment addend) does not fit a
    /// nonnegative `i32` immediate.
    ImmediateOutOfRange,
    /// A lane index lies outside its region's per-bar stride.
    LaneOutOfRange,
}

/// The layout-derived `i32.const` immediates of `prepare`, each equal to the
/// [`LayoutFootprint`] coefficient named in its doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrepareConsts {
    /// `builtin_base` for every bar count: [`LayoutFootprint::base`].
    pub(crate) builtin_base: i32,
    /// Builtin-lane bytes per bar: [`LayoutFootprint::builtin_bytes_per_bar`].
    pub(crate) builtin_stride: i32,
    /// Complete slot-history region: [`LayoutFootprint::slot_bytes`].
    pub(crate) slot_bytes: i32,
    /// Addend before the `& !7` that aligns `plots_base`:
    /// [`LayoutFootprint::slot_count`] `+ 7`.
    pub(crate) written_align_addend: i32,
    /// Plot-lane bytes per bar: [`LayoutFootprint::plot_bytes_per_bar`].
    pub(crate) plot_stride: i32,
    /// Color-lane bytes per bar: [`LayoutFootprint::color_bytes_per_bar`].
    pub(crate) color_stride: i32,
    /// Scratch region size: [`LayoutFootprint::scratch_bytes`].
    pub(crate) scratch_bytes: i32,
    /// Alert-lane bytes per bar: [`LayoutFootprint::alert_bytes_per_bar`].
    pub(crate) alert_stride: i32,
    /// Linked ta engine region: [`LayoutFootprint::ta_bytes`].
    pub(crate) ta_bytes: i32,
    /// Written-flag bytes zeroed by `memory.fill`: [`LayoutFootprint::slot_count`].
    pub(crate) written_len: i32,
}

/// `value` as a nonnegative `i32` immediate.
pub(crate) fn immediate(value: u64) -> Result<i32, PrepareConstsError> {
    if value > I32_MAX {
        Err(PrepareConstsError::ImmediateOutOfRange)
    } else {
        Ok(value as i32)
    }
}

/// Unwrap a `Result`, returning its error from the enclosing function.
/// Expands to a plain `match`, unlike `?`, which goes through the `Try`
/// trait.
macro_rules! ok_or_return {
    ($value:expr) => {
        match $value {
            Ok(value) => value,
            Err(error) => return Err(error),
        }
    };
}

/// Unwrap an `Option`, returning `None` from the enclosing function when it
/// is empty. Expands to a plain `match`, unlike `?`.
macro_rules! some_or_return_none {
    ($value:expr) => {
        match $value {
            Some(value) => value,
            None => return None,
        }
    };
}

/// Every layout-derived `i32` immediate of `prepare`, read off `footprint`.
pub(crate) fn prepare_consts(
    footprint: LayoutFootprint,
) -> Result<PrepareConsts, PrepareConstsError> {
    let written_align_addend = match footprint.slot_count.checked_add(7) {
        Some(value) => value,
        None => return Err(PrepareConstsError::ImmediateOutOfRange),
    };
    Ok(PrepareConsts {
        builtin_base: ok_or_return!(immediate(footprint.base)),
        builtin_stride: ok_or_return!(immediate(footprint.builtin_bytes_per_bar)),
        slot_bytes: ok_or_return!(immediate(footprint.slot_bytes)),
        written_align_addend: ok_or_return!(immediate(written_align_addend)),
        plot_stride: ok_or_return!(immediate(footprint.plot_bytes_per_bar)),
        color_stride: ok_or_return!(immediate(footprint.color_bytes_per_bar)),
        scratch_bytes: ok_or_return!(immediate(footprint.scratch_bytes)),
        alert_stride: ok_or_return!(immediate(footprint.alert_bytes_per_bar)),
        ta_bytes: ok_or_return!(immediate(footprint.ta_bytes)),
        written_len: ok_or_return!(immediate(footprint.slot_count)),
    })
}

/// Byte offset `lane * width` of one lane inside a per-bar region whose
/// stride is `stride` (the region's lane count times `width`): the
/// `i32.const` `prepare` multiplies by `bars` for that lane's base. Rejects a
/// lane at or past the end of the region.
pub(crate) fn lane_offset(lane: u64, width: u64, stride: i32) -> Result<i32, PrepareConstsError> {
    let offset = match lane.checked_mul(width) {
        Some(value) => value,
        None => return Err(PrepareConstsError::LaneOutOfRange),
    };
    if stride < 0 || offset >= stride as u64 {
        return Err(PrepareConstsError::LaneOutOfRange);
    }
    immediate(offset)
}

/// Total packed-color lane count: the sum of every plot's, visual's and
/// fill's declared color roles, in the flat order the color region is laid
/// out in. `None` if the sum overflows `u64`.
pub(crate) fn total_color_lanes(
    plot_roles: &[u32],
    visual_roles: &[u32],
    fill_roles: &[u32],
) -> Option<u64> {
    let with_plots = some_or_return_none!(sum_roles(0, plot_roles));
    let with_visuals = some_or_return_none!(sum_roles(with_plots, visual_roles));
    sum_roles(with_visuals, fill_roles)
}

/// `acc` plus every entry of `roles`, or `None` on `u64` overflow.
fn sum_roles(acc: u64, roles: &[u32]) -> Option<u64> {
    let mut sum = acc;
    let mut i = 0usize;
    while i < roles.len() {
        sum = some_or_return_none!(sum.checked_add(roles[i] as u64));
        i += 1;
    }
    Some(sum)
}

/// One instruction of `prepare`'s layout prefix: the `wasm_encoder`
/// instructions `emit_prepare` emits for it, one for one (`emit.rs`,
/// `prepare_op_instruction`). Local and global operands are indices; block
/// types are always empty and memory operands always memory 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareOp {
    LocalGet(u32),
    LocalSet(u32),
    GlobalSet(u32),
    I32Const(i32),
    I32Add,
    I32Sub,
    I32Mul,
    I32And,
    I32ShrU,
    I32GtU,
    I32Eq,
    /// `if` with an empty block type.
    If,
    Return,
    End,
    /// `memory.size` of memory 0.
    MemorySize,
    /// `memory.grow` of memory 0.
    MemoryGrow,
}

/// The two phase D heap globals `prepare` points at the heap end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrepareHeapGlobals {
    pub(crate) base: u32,
    pub(crate) ptr: u32,
}

/// The global indices (and the linked runtime's stack top) `prepare`'s
/// layout prefix writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrepareGlobals {
    /// The `bars` global.
    pub(crate) bars: u32,
    /// The linked runtime's stack-pointer global and the value `prepare`
    /// arms it with (`reserved_hi`), when a runtime is linked.
    pub(crate) stack_pointer: Option<PrepareStackPointer>,
    /// The ta engine's base global, when the program has `CallTa` sites.
    pub(crate) ta_base: Option<u32>,
    /// The phase D heap globals, for native-heap modules.
    pub(crate) heap: Option<PrepareHeapGlobals>,
}

/// The linked runtime's stack pointer global and its initial value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrepareStackPointer {
    pub(crate) global: u32,
    pub(crate) value: i32,
}

/// `prepare`'s local 0: the `bars` parameter.
pub(crate) const PREPARE_BARS: u32 = 0;
/// `prepare`'s locals 1..=10: the region bases, the heap end and the page count.
pub(crate) const PREPARE_BUILTIN_BASE: u32 = 1;
pub(crate) const PREPARE_SLOTS_BASE: u32 = 2;
pub(crate) const PREPARE_WRITTEN_BASE: u32 = 3;
pub(crate) const PREPARE_PLOTS_BASE: u32 = 4;
pub(crate) const PREPARE_COLORS_BASE: u32 = 5;
pub(crate) const PREPARE_SCRATCH_BASE: u32 = 6;
pub(crate) const PREPARE_ALERTS_BASE: u32 = 7;
pub(crate) const PREPARE_TA_BASE: u32 = 8;
pub(crate) const PREPARE_HEAP_END: u32 = 9;
pub(crate) const PREPARE_PAGES: u32 = 10;

/// `prepare`'s layout prefix, from the bar guard to the memory growth: the
/// guard `bars >u max_bars`, every region base computed from the spliced
/// `consts`, and `memory.grow` to the page count covering the heap end.
/// `emit_prepare` emits exactly these instructions, in this order, before
/// the header writes.
// One `push` per instruction keeps the Aeneas translation a plain sequence
// (`vec![..]` goes through a boxed slice).
#[allow(clippy::vec_init_then_push)]
pub(crate) fn prepare_layout_ops(
    consts: PrepareConsts,
    max_bars: u32,
    globals: PrepareGlobals,
) -> Vec<PrepareOp> {
    let mut ops = Vec::new();
    // Bar guard.
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::I32Const(max_bars as i32));
    ops.push(PrepareOp::I32GtU);
    ops.push(PrepareOp::If);
    ops.push(PrepareOp::I32Const(crate::status::TOO_MANY_BARS));
    ops.push(PrepareOp::Return);
    ops.push(PrepareOp::End);
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::GlobalSet(globals.bars));
    // The linked numeric runtime's shadow stack starts at the top of its
    // reserved block.
    if let Some(sp) = globals.stack_pointer {
        ops.push(PrepareOp::I32Const(sp.value));
        ops.push(PrepareOp::GlobalSet(sp.global));
    }
    // builtin_base
    ops.push(PrepareOp::I32Const(consts.builtin_base));
    ops.push(PrepareOp::LocalSet(PREPARE_BUILTIN_BASE));
    // slots_base = builtin_base + bars * builtin_stride
    ops.push(PrepareOp::LocalGet(PREPARE_BUILTIN_BASE));
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::I32Const(consts.builtin_stride));
    ops.push(PrepareOp::I32Mul);
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_SLOTS_BASE));
    // written_base = slots_base + slot_bytes
    ops.push(PrepareOp::LocalGet(PREPARE_SLOTS_BASE));
    ops.push(PrepareOp::I32Const(consts.slot_bytes));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_WRITTEN_BASE));
    // plots_base = (written_base + slot_count + 7) & !7
    ops.push(PrepareOp::LocalGet(PREPARE_WRITTEN_BASE));
    ops.push(PrepareOp::I32Const(consts.written_align_addend));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::I32Const(ALIGN8_MASK));
    ops.push(PrepareOp::I32And);
    ops.push(PrepareOp::LocalSet(PREPARE_PLOTS_BASE));
    // colors_base = plots_base + bars * plot_stride
    ops.push(PrepareOp::LocalGet(PREPARE_PLOTS_BASE));
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::I32Const(consts.plot_stride));
    ops.push(PrepareOp::I32Mul);
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_COLORS_BASE));
    // scratch_base = (colors_base + bars * color_stride + 7) & !7
    ops.push(PrepareOp::LocalGet(PREPARE_COLORS_BASE));
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::I32Const(consts.color_stride));
    ops.push(PrepareOp::I32Mul);
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::I32Const(7));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::I32Const(ALIGN8_MASK));
    ops.push(PrepareOp::I32And);
    ops.push(PrepareOp::LocalSet(PREPARE_SCRATCH_BASE));
    // alerts_base = scratch_base + scratch_bytes
    ops.push(PrepareOp::LocalGet(PREPARE_SCRATCH_BASE));
    ops.push(PrepareOp::I32Const(consts.scratch_bytes));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_ALERTS_BASE));
    // ta_base = alerts_base + bars * alert_stride
    ops.push(PrepareOp::LocalGet(PREPARE_ALERTS_BASE));
    ops.push(PrepareOp::LocalGet(PREPARE_BARS));
    ops.push(PrepareOp::I32Const(consts.alert_stride));
    ops.push(PrepareOp::I32Mul);
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_TA_BASE));
    if let Some(global) = globals.ta_base {
        ops.push(PrepareOp::LocalGet(PREPARE_TA_BASE));
        ops.push(PrepareOp::GlobalSet(global));
    }
    // heap_end = ta_base + ta_bytes
    ops.push(PrepareOp::LocalGet(PREPARE_TA_BASE));
    ops.push(PrepareOp::I32Const(consts.ta_bytes));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::LocalSet(PREPARE_HEAP_END));
    if let Some(heap) = globals.heap {
        ops.push(PrepareOp::LocalGet(PREPARE_HEAP_END));
        ops.push(PrepareOp::GlobalSet(heap.base));
        ops.push(PrepareOp::LocalGet(PREPARE_HEAP_END));
        ops.push(PrepareOp::GlobalSet(heap.ptr));
    }
    // pages = (heap_end + 0xFFFF) >> 16; grow memory if needed.
    ops.push(PrepareOp::LocalGet(PREPARE_HEAP_END));
    ops.push(PrepareOp::I32Const(0xFFFF));
    ops.push(PrepareOp::I32Add);
    ops.push(PrepareOp::I32Const(16));
    ops.push(PrepareOp::I32ShrU);
    ops.push(PrepareOp::LocalSet(PREPARE_PAGES));
    ops.push(PrepareOp::LocalGet(PREPARE_PAGES));
    ops.push(PrepareOp::MemorySize);
    ops.push(PrepareOp::I32GtU);
    ops.push(PrepareOp::If);
    ops.push(PrepareOp::LocalGet(PREPARE_PAGES));
    ops.push(PrepareOp::MemorySize);
    ops.push(PrepareOp::I32Sub);
    ops.push(PrepareOp::MemoryGrow);
    ops.push(PrepareOp::I32Const(-1));
    ops.push(PrepareOp::I32Eq);
    ops.push(PrepareOp::If);
    ops.push(PrepareOp::I32Const(crate::status::ALLOC_FAILED));
    ops.push(PrepareOp::Return);
    ops.push(PrepareOp::End);
    ops.push(PrepareOp::End);
    ops
}

/// `!7` as an `i32`: the mask that rounds an address down to eight bytes.
const ALIGN8_MASK: i32 = -8;

#[cfg(test)]
mod tests {
    use super::*;

    fn footprint(values: [u64; 9]) -> LayoutFootprint {
        LayoutFootprint {
            base: values[0],
            builtin_bytes_per_bar: values[1],
            plot_bytes_per_bar: values[2],
            color_bytes_per_bar: values[3],
            alert_bytes_per_bar: values[4],
            slot_bytes: values[5],
            slot_count: values[6],
            scratch_bytes: values[7],
            ta_bytes: values[8],
        }
    }

    #[test]
    fn consts_are_the_footprint_coefficients() {
        let consts = prepare_consts(footprint([96, 48, 8, 12, 8, 64, 3, 32, 128])).unwrap();
        assert_eq!(
            consts,
            PrepareConsts {
                builtin_base: 96,
                builtin_stride: 48,
                slot_bytes: 64,
                written_align_addend: 10,
                plot_stride: 8,
                color_stride: 12,
                scratch_bytes: 32,
                alert_stride: 8,
                ta_bytes: 128,
                written_len: 3,
            }
        );
    }

    #[test]
    fn every_coefficient_is_range_checked() {
        for field in 0..9 {
            let mut values = [0u64; 9];
            values[field] = I32_MAX;
            // slot_count + 7 is the one derived immediate.
            let expected_ok = field != 6;
            assert_eq!(
                prepare_consts(footprint(values)).is_ok(),
                expected_ok,
                "field {field} at i32::MAX"
            );
            values[field] = I32_MAX + 1;
            assert_eq!(
                prepare_consts(footprint(values)),
                Err(PrepareConstsError::ImmediateOutOfRange),
                "field {field} past i32::MAX"
            );
        }
        let mut values = [0u64; 9];
        values[6] = u64::MAX;
        assert_eq!(
            prepare_consts(footprint(values)),
            Err(PrepareConstsError::ImmediateOutOfRange)
        );
    }

    #[test]
    fn lane_offsets_stay_inside_the_stride() {
        assert_eq!(lane_offset(0, 8, 48), Ok(0));
        assert_eq!(lane_offset(5, 8, 48), Ok(40));
        assert_eq!(
            lane_offset(6, 8, 48),
            Err(PrepareConstsError::LaneOutOfRange)
        );
        assert_eq!(
            lane_offset(0, 4, 0),
            Err(PrepareConstsError::LaneOutOfRange)
        );
        assert_eq!(
            lane_offset(0, 4, -1),
            Err(PrepareConstsError::LaneOutOfRange)
        );
        assert_eq!(
            lane_offset(u64::MAX, 8, i32::MAX),
            Err(PrepareConstsError::LaneOutOfRange)
        );
    }

    #[test]
    fn color_lanes_sum_every_family_and_detect_overflow() {
        assert_eq!(total_color_lanes(&[], &[], &[]), Some(0));
        assert_eq!(total_color_lanes(&[1, 2], &[3], &[4, 0, 5]), Some(15));
        let many = vec![u32::MAX; 4];
        assert_eq!(
            total_color_lanes(&many, &many, &many),
            Some(12 * u64::from(u32::MAX))
        );
        assert_eq!(sum_roles(u64::MAX, &[1]), None);
        assert_eq!(sum_roles(u64::MAX, &[0]), Some(u64::MAX));
    }
}
