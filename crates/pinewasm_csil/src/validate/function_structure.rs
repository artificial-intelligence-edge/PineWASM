//! Production structural instruction issues, in guard/check order.
//! `instruction_issues` is formatting-free; diagnostic rendering is separate
//! and must not be included in a future extraction's kernel roots.
use super::{structure_kernel, MAX_WINDOW_LENGTH};
use crate::{CsilInstr, CsilProgram, PureFunc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StructureIssue {
    StringConstant {
        id: u32,
    },
    NonfiniteFloat,
    ColorTransparency {
        t: u64,
    },
    SeriesSlot {
        slot: u32,
    },
    SeriesOffset {
        offset: u32,
    },
    WindowLength {
        length: u32,
    },
    PureArity {
        func: PureFunc,
        argc: u8,
        min: u8,
        max: u8,
    },
    InternalScanAbi,
    CollectionArity {
        func: pinewasm_hir::CollectionFunc,
        argc: u8,
        min: u8,
        max: u8,
    },
    PivotArity {
        argc: u8,
    },
    TaState {
        state: u32,
    },
    Input {
        input: u32,
    },
    PlotSlot {
        plot_slot: u32,
    },
    PlotColors {
        plot_slot: u32,
        declared: usize,
        color_count: u8,
    },
    VisualSlot {
        visual_slot: u32,
    },
    VisualColors {
        visual_slot: u32,
        declared: usize,
        color_count: u8,
    },
    FillSlot {
        fill_slot: u32,
    },
    FillColors {
        fill_slot: u32,
        declared: usize,
        color_count: u8,
    },
    FillPlots {
        fill_slot: u32,
        plot1: u32,
        plot2: u32,
    },
    ObjectType {
        type_id: u32,
    },
    ObjectFields {
        type_id: u32,
        declared: u32,
        field_count: u32,
    },
    JumpTarget {
        target: u32,
    },
}

impl StructureIssue {
    pub(crate) fn message(self) -> String {
        match self {
            Self::StringConstant { id } => format!("string constant {id} out of range"),
            Self::NonfiniteFloat => "float constants must be finite (use PUSH_NA)".to_string(),
            Self::ColorTransparency { t } => format!("color transparency {} out of range (0..=100)", f64::from_bits(t)),
            Self::SeriesSlot { slot } => format!("series slot {slot} out of range"),
            Self::SeriesOffset { offset } => format!(
                    "series slot offset {offset} exceeds the limit of {MAX_WINDOW_LENGTH}"
                ),
            Self::WindowLength { length } => format!(
                        "window length {length} out of range (1..={MAX_WINDOW_LENGTH})"
                    ),
            Self::PureArity { func, argc, min, max } => {
                let name = func.name();
                format!("CALL_PURE {name} argc {argc} out of range ({min}..={max})")
            },
            Self::InternalScanAbi => "internal distance scan requires runtime ABI 2".into(),
            Self::CollectionArity { func, argc, min, max } => {
                let name = func.name();
                format!("CALL_COLLECTION {name} argc {argc} out of range ({min}..={max})")
            },
            Self::PivotArity { argc } => format!("CALL_PIVOT_POINT_LEVELS argc {argc}, expected 3"),
            Self::TaState { state } => format!("ta state {state} out of range"),
            Self::Input { input } => format!("input {input} out of range"),
            Self::PlotSlot { plot_slot } => format!("plot slot {plot_slot} out of range"),
            Self::PlotColors { plot_slot, declared, color_count } => format!(
                        "plot slot {plot_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            Self::VisualSlot { visual_slot } => format!("visual slot {visual_slot} out of range"),
            Self::VisualColors { visual_slot, declared, color_count } => format!(
                        "visual slot {visual_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            Self::FillSlot { fill_slot } => format!("fill slot {fill_slot} out of range"),
            Self::FillColors { fill_slot, declared, color_count } => format!(
                        "fill slot {fill_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            Self::FillPlots { fill_slot, plot1, plot2 } => format!(
                        "fill slot {fill_slot} references plot slot(s) {plot1}/{plot2} out of range"
                    ),
            Self::ObjectType { type_id } => format!("object type {type_id} out of range"),
            Self::ObjectFields { type_id, declared, field_count } => format!(
                        "NEW_OBJECT type {type_id} expects {declared} field(s), got {field_count}"
                    ),
            Self::JumpTarget { target } => format!("jump target {target} out of range"),
        }
    }
}

/// Extraction root for the production renderer. Direct inherent
/// method selectors are not supported by the extraction tool's root matcher.
#[cfg(feature = "aeneas")]
#[allow(dead_code, reason = "Extraction root, absent from production builds")]
pub(crate) fn structure_issue_message(issue: StructureIssue) -> String {
    issue.message()
}

/// Primitive operations left explicit for extraction and scalar independence.
/// Production supplies the Rust implementations of these operations.
pub(crate) trait StructurePrimitives {
    fn finite(&self, value: crate::F64Bits) -> bool;
    fn color_transparency(&self, value: crate::F64Bits) -> bool;
    fn window_length(&self, length: u32) -> bool;
    fn pure_arity(&self, func: PureFunc) -> (u8, u8);
    fn collection_arity(&self, func: pinewasm_hir::CollectionFunc) -> (u8, u8);
}

pub(crate) struct RustStructurePrimitives;

impl StructurePrimitives for RustStructurePrimitives {
    fn finite(&self, value: crate::F64Bits) -> bool {
        value.get().is_finite()
    }
    fn color_transparency(&self, value: crate::F64Bits) -> bool {
        value.get().is_finite() && (0.0..=100.0).contains(&value.get())
    }
    fn window_length(&self, length: u32) -> bool {
        (1..=MAX_WINDOW_LENGTH).contains(&length)
    }
    fn pure_arity(&self, func: PureFunc) -> (u8, u8) {
        func.argc_bounds()
    }
    fn collection_arity(&self, func: pinewasm_hir::CollectionFunc) -> (u8, u8) {
        func.argc_bounds()
    }
}

#[cfg(test)]
pub(crate) fn instruction_issues(
    program: &CsilProgram,
    instr: &CsilInstr,
    code_len: usize,
) -> Vec<StructureIssue> {
    instruction_issues_with_primitives(&RustStructurePrimitives, program, instr, code_len)
}

/// Streaming structural diagnostics. Offsets stay in the in-memory index
/// domain; production renders them with its preflighted wire conversion.
#[cfg(test)]
pub(crate) trait StructureIssueSink {
    fn emit(&mut self, offset: usize, issue: StructureIssue);
}

#[cfg(test)]
pub(crate) fn structural_issues<S: StructureIssueSink>(
    sink: &mut S,
    program: &CsilProgram,
    func: &crate::CsilFunction,
) -> bool {
    structural_issues_with_primitives(&RustStructurePrimitives, sink, program, func)
}

/// Production scan: every row, including unreachable rows, is checked
/// once and its issues are emitted immediately in diagnostic order.
/// No aggregate issue vector is allocated; a valid body never invokes the sink.
/// Production equivalence is within `validate`'s instruction-count preflight:
/// it guarantees every index fits the diagnostic u32 domain. This root leaves
/// that conversion to the sink. Calling the validator directly on an oversized
/// body that bypasses the preflight is outside this guarantee.
pub(crate) trait StructureEmitter<S> {
    fn emit(&self, state: &mut S, offset: usize, issue: StructureIssue);
}
#[cfg(test)]
struct SinkEmitter;
#[cfg(test)]
impl<S: StructureIssueSink> StructureEmitter<S> for SinkEmitter {
    fn emit(&self, state: &mut S, offset: usize, issue: StructureIssue) {
        StructureIssueSink::emit(state, offset, issue);
    }
}
#[cfg(test)]
pub(crate) fn structural_issues_with_primitives<P: StructurePrimitives, S: StructureIssueSink>(
    primitives: &P,
    sink: &mut S,
    program: &CsilProgram,
    func: &crate::CsilFunction,
) -> bool {
    structural_issues_with_emitter(&SinkEmitter, primitives, sink, program, func)
}
pub(crate) fn structural_issues_with_emitter<E: StructureEmitter<S>, P: StructurePrimitives, S>(
    emitter: &E,
    primitives: &P,
    state: &mut S,
    program: &CsilProgram,
    func: &crate::CsilFunction,
) -> bool {
    let code_len = func.code.len();
    let mut structural_ok = true;
    for (offset, instr) in func.code.iter().enumerate() {
        for issue in instruction_issues_with_primitives(primitives, program, instr, code_len) {
            emitter.emit(state, offset, issue);
            structural_ok = false;
        }
    }
    structural_ok
}

#[allow(
    clippy::manual_range_contains,
    reason = "Keep primitive arity comparisons explicit in the extracted structural kernel"
)]
pub(crate) fn instruction_issues_with_primitives<P: StructurePrimitives>(
    primitives: &P,
    program: &CsilProgram,
    instr: &CsilInstr,
    code_len: usize,
) -> Vec<StructureIssue> {
    let mut issues = Vec::new();
    match instr {
        CsilInstr::PushStr(id) if *id as usize >= program.constants.len() => {
            issues.push(StructureIssue::StringConstant { id: *id });
        }
        CsilInstr::PushF64(v) if !primitives.finite(*v) => {
            issues.push(StructureIssue::NonfiniteFloat);
        }
        CsilInstr::PushColor { t, .. } if !primitives.color_transparency(*t) => {
            issues.push(StructureIssue::ColorTransparency { t: t.to_bits() });
        }
        CsilInstr::ReadUserSlot { slot, .. }
        | CsilInstr::ReadUserSlotDynamic { slot }
        | CsilInstr::StoreUserSlotCurrent { slot }
            if !structure_kernel::index_in_range(*slot, program.series_slots.len()) =>
        {
            issues.push(StructureIssue::SeriesSlot { slot: *slot });
        }
        // A literal offset is never checked at runtime (see the
        // `max_bars_back` doc comment), and it sizes the slot ring:
        // `plan_layout` takes `deepest = max(.., offset)` and both tiers
        // allocate `slot_count * (deepest + 1).next_power_of_two() * 8`.
        // Unbounded, that is an allocation an adversarial program picks —
        // and past 2^31 it overflows the sizing arithmetic outright.
        // `MAX_WINDOW_LENGTH` is the ceiling because `WindowMean.length`
        // is the other literal reach-back and already permits exactly
        // this ring size, so this adds no new bound on the layout.
        CsilInstr::ReadUserSlot { offset, .. }
            if !structure_kernel::window_offset_admitted(*offset) =>
        {
            issues.push(StructureIssue::SeriesOffset { offset: *offset });
        }
        CsilInstr::WindowMean { slot, length } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
            if !primitives.window_length(*length) {
                issues.push(StructureIssue::WindowLength { length: *length });
            }
        }
        CsilInstr::WindowMeanDynamic { slot }
        | CsilInstr::WindowExtremumDynamic { slot, .. }
        | CsilInstr::WindowStatisticDynamic { slot, .. }
        | CsilInstr::EmaSimpleDynamic { slot } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
        }
        CsilInstr::CallPure { func, argc } => {
            let (min, max) = primitives.pure_arity(*func);
            if *argc < min || *argc > max {
                issues.push(StructureIssue::PureArity {
                    func: *func,
                    argc: *argc,
                    min,
                    max,
                });
            }
        }
        CsilInstr::CallCollection { func, argc } => {
            if matches!(
                func,
                pinewasm_hir::CollectionFunc::InternalDistanceScan
                    | pinewasm_hir::CollectionFunc::InternalDistanceScanEligible
            ) && program.header.runtime_abi_version != crate::INDEXED_SCAN_ABI_VERSION
            {
                issues.push(StructureIssue::InternalScanAbi);
            }
            let (min, max) = primitives.collection_arity(*func);
            if *argc < min || *argc > max {
                issues.push(StructureIssue::CollectionArity {
                    func: *func,
                    argc: *argc,
                    min,
                    max,
                });
            }
        }
        CsilInstr::CallPivotPointLevels { argc } if *argc != 3 => {
            issues.push(StructureIssue::PivotArity { argc: *argc });
        }
        CsilInstr::CallTa { state } if *state as usize >= program.ta_states.len() => {
            issues.push(StructureIssue::TaState { state: *state });
        }
        CsilInstr::ReadInput { input } if *input as usize >= program.inputs.len() => {
            issues.push(StructureIssue::Input { input: *input });
        }
        CsilInstr::PlotF64 { plot_slot }
            if !structure_kernel::index_in_range(*plot_slot, program.plots.len()) =>
        {
            issues.push(StructureIssue::PlotSlot {
                plot_slot: *plot_slot,
            });
        }
        CsilInstr::PlotStyledF64 {
            plot_slot,
            color_count,
        } => match program.plots.get(*plot_slot as usize) {
            None => {
                issues.push(StructureIssue::PlotSlot {
                    plot_slot: *plot_slot,
                });
            }
            Some(plot) if plot.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::PlotColors {
                    plot_slot: *plot_slot,
                    declared: plot.color_roles.len(),
                    color_count: *color_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::VisualColor {
            visual_slot,
            color_count,
        } => match program.visuals.get(*visual_slot as usize) {
            None => {
                issues.push(StructureIssue::VisualSlot {
                    visual_slot: *visual_slot,
                });
            }
            Some(visual) if visual.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::VisualColors {
                    visual_slot: *visual_slot,
                    declared: visual.color_roles.len(),
                    color_count: *color_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::FillColor {
            fill_slot,
            color_count,
        } => match program.fills.get(*fill_slot as usize) {
            None => {
                issues.push(StructureIssue::FillSlot {
                    fill_slot: *fill_slot,
                });
            }
            Some(fill) if fill.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::FillColors {
                    fill_slot: *fill_slot,
                    declared: fill.color_roles.len(),
                    color_count: *color_count,
                });
            }
            // Both endpoints must name plots that exist: the renderer reads
            // the band's geometry from those two series.
            Some(fill)
                if fill.plot1 as usize >= program.plots.len()
                    || fill.plot2 as usize >= program.plots.len() =>
            {
                issues.push(StructureIssue::FillPlots {
                    fill_slot: *fill_slot,
                    plot1: fill.plot1,
                    plot2: fill.plot2,
                });
            }
            Some(_) => {}
        },
        CsilInstr::NewObject {
            type_id,
            field_count,
        } => match program.object_types.get(*type_id as usize) {
            None => {
                issues.push(StructureIssue::ObjectType { type_id: *type_id });
            }
            Some(decl) if decl.field_count != *field_count => {
                issues.push(StructureIssue::ObjectFields {
                    type_id: *type_id,
                    declared: decl.field_count,
                    field_count: *field_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::CopyObject { type_id } if *type_id as usize >= program.object_types.len() => {
            issues.push(StructureIssue::ObjectType { type_id: *type_id });
        }
        CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
            if *target as usize >= code_len =>
        {
            issues.push(StructureIssue::JumpTarget { target: *target });
        }
        CsilInstr::JumpIfSlotWritten { slot, target } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
            if *target as usize >= code_len {
                issues.push(StructureIssue::JumpTarget { target: *target });
            }
        }
        _ => {}
    }
    issues
}

// Test-only kernel for behavioral differential tests.
#[cfg(test)]
fn baseline_instruction_issues(
    program: &CsilProgram,
    instr: &CsilInstr,
    code_len: usize,
) -> Vec<StructureIssue> {
    let mut issues = Vec::new();
    match instr {
        CsilInstr::PushStr(id) if *id as usize >= program.constants.len() => {
            issues.push(StructureIssue::StringConstant { id: *id });
        }
        CsilInstr::PushF64(v) if !v.get().is_finite() => {
            issues.push(StructureIssue::NonfiniteFloat);
        }
        CsilInstr::PushColor { t, .. }
            if !t.get().is_finite() || !(0.0..=100.0).contains(&t.get()) =>
        {
            issues.push(StructureIssue::ColorTransparency { t: t.to_bits() });
        }
        CsilInstr::ReadUserSlot { slot, .. }
        | CsilInstr::ReadUserSlotDynamic { slot }
        | CsilInstr::StoreUserSlotCurrent { slot }
            if !structure_kernel::index_in_range(*slot, program.series_slots.len()) =>
        {
            issues.push(StructureIssue::SeriesSlot { slot: *slot });
        }
        // A literal offset is never checked at runtime (see the
        // `max_bars_back` doc comment), and it sizes the slot ring:
        // `plan_layout` takes `deepest = max(.., offset)` and both tiers
        // allocate `slot_count * (deepest + 1).next_power_of_two() * 8`.
        // Unbounded, that is an allocation an adversarial program picks —
        // and past 2^31 it overflows the sizing arithmetic outright.
        // `MAX_WINDOW_LENGTH` is the ceiling because `WindowMean.length`
        // is the other literal reach-back and already permits exactly
        // this ring size, so this adds no new bound on the layout.
        CsilInstr::ReadUserSlot { offset, .. }
            if !structure_kernel::window_offset_admitted(*offset) =>
        {
            issues.push(StructureIssue::SeriesOffset { offset: *offset });
        }
        CsilInstr::WindowMean { slot, length } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
            if !(1..=MAX_WINDOW_LENGTH).contains(length) {
                issues.push(StructureIssue::WindowLength { length: *length });
            }
        }
        CsilInstr::WindowMeanDynamic { slot }
        | CsilInstr::WindowExtremumDynamic { slot, .. }
        | CsilInstr::WindowStatisticDynamic { slot, .. }
        | CsilInstr::EmaSimpleDynamic { slot } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
        }
        CsilInstr::CallPure { func, argc } => {
            let (min, max) = func.argc_bounds();
            if !(min..=max).contains(argc) {
                issues.push(StructureIssue::PureArity {
                    func: *func,
                    argc: *argc,
                    min,
                    max,
                });
            }
        }
        CsilInstr::CallCollection { func, argc } => {
            let (min, max) = func.argc_bounds();
            if !(min..=max).contains(argc) {
                issues.push(StructureIssue::CollectionArity {
                    func: *func,
                    argc: *argc,
                    min,
                    max,
                });
            }
        }
        CsilInstr::CallPivotPointLevels { argc } if *argc != 3 => {
            issues.push(StructureIssue::PivotArity { argc: *argc });
        }
        CsilInstr::CallTa { state } if *state as usize >= program.ta_states.len() => {
            issues.push(StructureIssue::TaState { state: *state });
        }
        CsilInstr::ReadInput { input } if *input as usize >= program.inputs.len() => {
            issues.push(StructureIssue::Input { input: *input });
        }
        CsilInstr::PlotF64 { plot_slot }
            if !structure_kernel::index_in_range(*plot_slot, program.plots.len()) =>
        {
            issues.push(StructureIssue::PlotSlot {
                plot_slot: *plot_slot,
            });
        }
        CsilInstr::PlotStyledF64 {
            plot_slot,
            color_count,
        } => match program.plots.get(*plot_slot as usize) {
            None => {
                issues.push(StructureIssue::PlotSlot {
                    plot_slot: *plot_slot,
                });
            }
            Some(plot) if plot.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::PlotColors {
                    plot_slot: *plot_slot,
                    declared: plot.color_roles.len(),
                    color_count: *color_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::VisualColor {
            visual_slot,
            color_count,
        } => match program.visuals.get(*visual_slot as usize) {
            None => {
                issues.push(StructureIssue::VisualSlot {
                    visual_slot: *visual_slot,
                });
            }
            Some(visual) if visual.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::VisualColors {
                    visual_slot: *visual_slot,
                    declared: visual.color_roles.len(),
                    color_count: *color_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::FillColor {
            fill_slot,
            color_count,
        } => match program.fills.get(*fill_slot as usize) {
            None => {
                issues.push(StructureIssue::FillSlot {
                    fill_slot: *fill_slot,
                });
            }
            Some(fill) if fill.color_roles.len() != *color_count as usize => {
                issues.push(StructureIssue::FillColors {
                    fill_slot: *fill_slot,
                    declared: fill.color_roles.len(),
                    color_count: *color_count,
                });
            }
            // Both endpoints must name plots that exist: the renderer reads
            // the band's geometry from those two series.
            Some(fill)
                if fill.plot1 as usize >= program.plots.len()
                    || fill.plot2 as usize >= program.plots.len() =>
            {
                issues.push(StructureIssue::FillPlots {
                    fill_slot: *fill_slot,
                    plot1: fill.plot1,
                    plot2: fill.plot2,
                });
            }
            Some(_) => {}
        },
        CsilInstr::NewObject {
            type_id,
            field_count,
        } => match program.object_types.get(*type_id as usize) {
            None => {
                issues.push(StructureIssue::ObjectType { type_id: *type_id });
            }
            Some(decl) if decl.field_count != *field_count => {
                issues.push(StructureIssue::ObjectFields {
                    type_id: *type_id,
                    declared: decl.field_count,
                    field_count: *field_count,
                });
            }
            Some(_) => {}
        },
        CsilInstr::CopyObject { type_id } if *type_id as usize >= program.object_types.len() => {
            issues.push(StructureIssue::ObjectType { type_id: *type_id });
        }
        CsilInstr::Jump { target }
        | CsilInstr::JumpIfFalse { target }
        | CsilInstr::JumpIfExact { target, .. }
            if *target as usize >= code_len =>
        {
            issues.push(StructureIssue::JumpTarget { target: *target });
        }
        CsilInstr::JumpIfSlotWritten { slot, target } => {
            if *slot as usize >= program.series_slots.len() {
                issues.push(StructureIssue::SeriesSlot { slot: *slot });
            }
            if *target as usize >= code_len {
                issues.push(StructureIssue::JumpTarget { target: *target });
            }
        }
        _ => {}
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::{CsilFill, CsilVisual, F64Bits, SlotDef};

    // Test-only renderer for the structure issue messages. The transparency
    // arm applies the same finiteness and 0..=100 range check as the production
    // primitive. The two name borrows are taken outside format's argument
    // evaluation. It is used only by tests.
    fn baseline_issue_message(issue: StructureIssue) -> String {
        match issue {
            StructureIssue::InternalScanAbi => "internal distance scan requires runtime ABI 2".into(),
            StructureIssue::StringConstant { id } => format!("string constant {id} out of range"),
            StructureIssue::NonfiniteFloat => "float constants must be finite (use PUSH_NA)".to_string(),
            StructureIssue::ColorTransparency { t } => format!("color transparency {} out of range (0..=100)", f64::from_bits(t)),
            StructureIssue::SeriesSlot { slot } => format!("series slot {slot} out of range"),
            StructureIssue::SeriesOffset { offset } => format!(
                    "series slot offset {offset} exceeds the limit of {MAX_WINDOW_LENGTH}"
                ),
            StructureIssue::WindowLength { length } => format!(
                        "window length {length} out of range (1..={MAX_WINDOW_LENGTH})"
                    ),
            StructureIssue::PureArity { func, argc, min, max } => format!(
                        "CALL_PURE {} argc {argc} out of range ({min}..={max})",
                        func.name()
                    ),
            StructureIssue::CollectionArity { func, argc, min, max } => format!(
                        "CALL_COLLECTION {} argc {argc} out of range ({min}..={max})",
                        func.name()
                    ),
            StructureIssue::PivotArity { argc } => format!("CALL_PIVOT_POINT_LEVELS argc {argc}, expected 3"),
            StructureIssue::TaState { state } => format!("ta state {state} out of range"),
            StructureIssue::Input { input } => format!("input {input} out of range"),
            StructureIssue::PlotSlot { plot_slot } => format!("plot slot {plot_slot} out of range"),
            StructureIssue::PlotColors { plot_slot, declared, color_count } => format!(
                        "plot slot {plot_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            StructureIssue::VisualSlot { visual_slot } => format!("visual slot {visual_slot} out of range"),
            StructureIssue::VisualColors { visual_slot, declared, color_count } => format!(
                        "visual slot {visual_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            StructureIssue::FillSlot { fill_slot } => format!("fill slot {fill_slot} out of range"),
            StructureIssue::FillColors { fill_slot, declared, color_count } => format!(
                        "fill slot {fill_slot} declares {declared} color channel(s), instruction has {color_count}"
                    ),
            StructureIssue::FillPlots { fill_slot, plot1, plot2 } => format!(
                        "fill slot {fill_slot} references plot slot(s) {plot1}/{plot2} out of range"
                    ),
            StructureIssue::ObjectType { type_id } => format!("object type {type_id} out of range"),
            StructureIssue::ObjectFields { type_id, declared, field_count } => format!(
                        "NEW_OBJECT type {type_id} expects {declared} field(s), got {field_count}"
                    ),
            StructureIssue::JumpTarget { target } => format!("jump target {target} out of range"),
        }
    }

    #[cfg(feature = "aeneas")]
    #[test]
    fn renderer_private_root_forwards_actual_body() {
        let issues = [
            StructureIssue::NonfiniteFloat,
            StructureIssue::PureArity {
                func: PureFunc::MathRound,
                argc: 255,
                min: 0,
                max: 255,
            },
            StructureIssue::CollectionArity {
                func: pinewasm_hir::CollectionFunc::MatrixNew,
                argc: 0,
                min: 1,
                max: 255,
            },
        ];
        for issue in issues {
            assert_eq!(structure_issue_message(issue.clone()), issue.message());
        }
    }

    #[test]
    fn renderer_all_issue_variants_match_frozen_messages() {
        for n in [0, 1, MAX_WINDOW_LENGTH, MAX_WINDOW_LENGTH + 1, u32::MAX] {
            for byte in [0, 1, 100, 101, u8::MAX] {
                for declared in [0, 1, u32::MAX as usize, usize::MAX] {
                    let issues = [
                        StructureIssue::StringConstant { id: n },
                        StructureIssue::NonfiniteFloat,
                        StructureIssue::ColorTransparency {
                            t: f64::from(byte).to_bits(),
                        },
                        StructureIssue::SeriesSlot { slot: n },
                        StructureIssue::SeriesOffset { offset: n },
                        StructureIssue::WindowLength { length: n },
                        StructureIssue::PivotArity { argc: byte },
                        StructureIssue::TaState { state: n },
                        StructureIssue::Input { input: n },
                        StructureIssue::PlotSlot { plot_slot: n },
                        StructureIssue::PlotColors {
                            plot_slot: n,
                            declared,
                            color_count: byte,
                        },
                        StructureIssue::VisualSlot { visual_slot: n },
                        StructureIssue::VisualColors {
                            visual_slot: n,
                            declared,
                            color_count: byte,
                        },
                        StructureIssue::FillSlot { fill_slot: n },
                        StructureIssue::FillColors {
                            fill_slot: n,
                            declared,
                            color_count: byte,
                        },
                        StructureIssue::FillPlots {
                            fill_slot: n,
                            plot1: n,
                            plot2: n,
                        },
                        StructureIssue::ObjectType { type_id: n },
                        StructureIssue::ObjectFields {
                            type_id: n,
                            declared: n,
                            field_count: n,
                        },
                        StructureIssue::JumpTarget { target: n },
                    ];
                    for issue in issues {
                        assert_eq!(issue.clone().message(), baseline_issue_message(issue));
                    }
                }
            }
        }
    }

    #[test]
    fn renderer_binary64_transparency_matches_current_messages() {
        for value in [
            0.0,
            -0.0,
            0.1,
            47.6,
            100.0,
            100.1,
            -1.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            let issue = StructureIssue::ColorTransparency { t: value.to_bits() };
            assert_eq!(issue.clone().message(), baseline_issue_message(issue));
        }
    }

    #[test]
    fn renderer_all_function_name_families_match_frozen_messages() {
        let pure = [
            pinewasm_hir::PureFunc::Na,
            pinewasm_hir::PureFunc::Nz,
            pinewasm_hir::PureFunc::MathAbs,
            pinewasm_hir::PureFunc::MathSign,
            pinewasm_hir::PureFunc::MathSqrt,
            pinewasm_hir::PureFunc::MathPow,
            pinewasm_hir::PureFunc::MathExp,
            pinewasm_hir::PureFunc::MathLog,
            pinewasm_hir::PureFunc::MathLog10,
            pinewasm_hir::PureFunc::MathSin,
            pinewasm_hir::PureFunc::MathCos,
            pinewasm_hir::PureFunc::MathTan,
            pinewasm_hir::PureFunc::MathAsin,
            pinewasm_hir::PureFunc::MathAcos,
            pinewasm_hir::PureFunc::MathAtan,
            pinewasm_hir::PureFunc::MathFloor,
            pinewasm_hir::PureFunc::MathCeil,
            pinewasm_hir::PureFunc::MathRound,
            pinewasm_hir::PureFunc::MathMin,
            pinewasm_hir::PureFunc::MathMax,
            pinewasm_hir::PureFunc::MathAvg,
            pinewasm_hir::PureFunc::MathToDegrees,
            pinewasm_hir::PureFunc::MathToRadians,
            pinewasm_hir::PureFunc::MathRandom,
            pinewasm_hir::PureFunc::MathRoundToMintick,
            pinewasm_hir::PureFunc::StrTostring,
            pinewasm_hir::PureFunc::StrTonumber,
            pinewasm_hir::PureFunc::StrFormat,
            pinewasm_hir::PureFunc::StrLength,
            pinewasm_hir::PureFunc::StrSubstring,
            pinewasm_hir::PureFunc::StrReplace,
            pinewasm_hir::PureFunc::StrReplaceAll,
            pinewasm_hir::PureFunc::StrContains,
            pinewasm_hir::PureFunc::StrStartswith,
            pinewasm_hir::PureFunc::StrEndswith,
            pinewasm_hir::PureFunc::StrPos,
            pinewasm_hir::PureFunc::StrUpper,
            pinewasm_hir::PureFunc::StrLower,
            pinewasm_hir::PureFunc::StrTrim,
            pinewasm_hir::PureFunc::StrRepeat,
            pinewasm_hir::PureFunc::StrMatch,
            pinewasm_hir::PureFunc::StrFormatTime,
            pinewasm_hir::PureFunc::ColorNew,
            pinewasm_hir::PureFunc::ColorRgb,
            pinewasm_hir::PureFunc::ColorFromGradient,
            pinewasm_hir::PureFunc::ColorR,
            pinewasm_hir::PureFunc::ColorG,
            pinewasm_hir::PureFunc::ColorB,
            pinewasm_hir::PureFunc::ColorT,
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::EntryPrice),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ExitPrice),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::Size),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::Profit),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ProfitPercent),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::Commission),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::EntryBarIndex),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ExitBarIndex),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::EntryTime),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ExitTime),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::MaxRunup),
            pinewasm_hir::PureFunc::StrategyClosedTrade(
                pinewasm_hir::TradeAccessor::MaxRunupPercent,
            ),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::MaxDrawdown),
            pinewasm_hir::PureFunc::StrategyClosedTrade(
                pinewasm_hir::TradeAccessor::MaxDrawdownPercent,
            ),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::EntryId),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ExitId),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::EntryComment),
            pinewasm_hir::PureFunc::StrategyClosedTrade(pinewasm_hir::TradeAccessor::ExitComment),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::EntryPrice),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ExitPrice),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::Size),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::Profit),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ProfitPercent),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::Commission),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::EntryBarIndex),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ExitBarIndex),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::EntryTime),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ExitTime),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::MaxRunup),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::MaxRunupPercent),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::MaxDrawdown),
            pinewasm_hir::PureFunc::StrategyOpenTrade(
                pinewasm_hir::TradeAccessor::MaxDrawdownPercent,
            ),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::EntryId),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ExitId),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::EntryComment),
            pinewasm_hir::PureFunc::StrategyOpenTrade(pinewasm_hir::TradeAccessor::ExitComment),
            pinewasm_hir::PureFunc::StrategyConvertToAccount,
            pinewasm_hir::PureFunc::StrategyConvertToSymbol,
            pinewasm_hir::PureFunc::StrategyDefaultEntryQty,
            pinewasm_hir::PureFunc::TimeframeInSeconds,
            pinewasm_hir::PureFunc::TimeframeFromSeconds,
            pinewasm_hir::PureFunc::TimeOpen,
            pinewasm_hir::PureFunc::TimeClose,
            pinewasm_hir::PureFunc::Timestamp,
            pinewasm_hir::PureFunc::TimestampTz,
            pinewasm_hir::PureFunc::CastInt,
            pinewasm_hir::PureFunc::CastFloat,
            pinewasm_hir::PureFunc::CastBool,
            pinewasm_hir::PureFunc::CastColor,
            pinewasm_hir::PureFunc::WeekOfYear,
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::Year),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::Month),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::DayOfMonth),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::DayOfWeek),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::Hour),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::Minute),
            pinewasm_hir::PureFunc::Calendar(pinewasm_hir::CalendarField::Second),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::New),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Modify),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Heikinashi),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Kagi),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Linebreak),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Pointfigure),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Renko),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Standard),
            pinewasm_hir::PureFunc::Ticker(pinewasm_hir::TickerFunc::Inherit),
            pinewasm_hir::PureFunc::SyminfoPrefix,
            pinewasm_hir::PureFunc::SyminfoTicker,
        ];
        let collections = [
            pinewasm_hir::CollectionFunc::ArrayNewFloat,
            pinewasm_hir::CollectionFunc::ArrayNewInt,
            pinewasm_hir::CollectionFunc::ArrayNewBool,
            pinewasm_hir::CollectionFunc::ArrayNewString,
            pinewasm_hir::CollectionFunc::ArrayNewColor,
            pinewasm_hir::CollectionFunc::ArrayNewObject,
            pinewasm_hir::CollectionFunc::ArrayFrom,
            pinewasm_hir::CollectionFunc::StrSplit,
            pinewasm_hir::CollectionFunc::ArrayPush,
            pinewasm_hir::CollectionFunc::ArrayPop,
            pinewasm_hir::CollectionFunc::ArrayGet,
            pinewasm_hir::CollectionFunc::ArraySet,
            pinewasm_hir::CollectionFunc::ArraySize,
            pinewasm_hir::CollectionFunc::ArrayClear,
            pinewasm_hir::CollectionFunc::ArrayShift,
            pinewasm_hir::CollectionFunc::ArrayUnshift,
            pinewasm_hir::CollectionFunc::ArrayInsert,
            pinewasm_hir::CollectionFunc::ArrayRemove,
            pinewasm_hir::CollectionFunc::ArrayIncludes,
            pinewasm_hir::CollectionFunc::ArrayIndexOf,
            pinewasm_hir::CollectionFunc::ArrayLastIndexOf,
            pinewasm_hir::CollectionFunc::ArrayReverse,
            pinewasm_hir::CollectionFunc::ArrayFirst,
            pinewasm_hir::CollectionFunc::ArrayLast,
            pinewasm_hir::CollectionFunc::ArrayConcat,
            pinewasm_hir::CollectionFunc::ArraySlice,
            pinewasm_hir::CollectionFunc::ArrayCopy,
            pinewasm_hir::CollectionFunc::ArrayFill,
            pinewasm_hir::CollectionFunc::ArrayJoin,
            pinewasm_hir::CollectionFunc::ArraySum,
            pinewasm_hir::CollectionFunc::ArrayAvg,
            pinewasm_hir::CollectionFunc::ArrayMin,
            pinewasm_hir::CollectionFunc::ArrayMax,
            pinewasm_hir::CollectionFunc::ArrayMedian,
            pinewasm_hir::CollectionFunc::ArrayRange,
            pinewasm_hir::CollectionFunc::ArrayMode,
            pinewasm_hir::CollectionFunc::ArrayVariance,
            pinewasm_hir::CollectionFunc::ArrayStdev,
            pinewasm_hir::CollectionFunc::ArrayCovariance,
            pinewasm_hir::CollectionFunc::ArrayEvery,
            pinewasm_hir::CollectionFunc::ArraySome,
            pinewasm_hir::CollectionFunc::ArrayPercentRank,
            pinewasm_hir::CollectionFunc::ArrayPercentileNearestRank,
            pinewasm_hir::CollectionFunc::ArrayPercentileLinearInterpolation,
            pinewasm_hir::CollectionFunc::ArrayBinarySearch,
            pinewasm_hir::CollectionFunc::ArrayBinarySearchLeftmost,
            pinewasm_hir::CollectionFunc::ArrayBinarySearchRightmost,
            pinewasm_hir::CollectionFunc::ArrayAbs,
            pinewasm_hir::CollectionFunc::ArrayStandardize,
            pinewasm_hir::CollectionFunc::ArraySort,
            pinewasm_hir::CollectionFunc::ArraySortIndices,
            pinewasm_hir::CollectionFunc::MapNew,
            pinewasm_hir::CollectionFunc::MapPut,
            pinewasm_hir::CollectionFunc::MapGet,
            pinewasm_hir::CollectionFunc::MapContains,
            pinewasm_hir::CollectionFunc::MapRemove,
            pinewasm_hir::CollectionFunc::MapSize,
            pinewasm_hir::CollectionFunc::MapClear,
            pinewasm_hir::CollectionFunc::MapKeys,
            pinewasm_hir::CollectionFunc::MapValues,
            pinewasm_hir::CollectionFunc::MapCopy,
            pinewasm_hir::CollectionFunc::MapPutAll,
            pinewasm_hir::CollectionFunc::MatrixNew,
            pinewasm_hir::CollectionFunc::MatrixGet,
            pinewasm_hir::CollectionFunc::MatrixSet,
            pinewasm_hir::CollectionFunc::MatrixRows,
            pinewasm_hir::CollectionFunc::MatrixColumns,
            pinewasm_hir::CollectionFunc::MatrixElementsCount,
            pinewasm_hir::CollectionFunc::MatrixFill,
            pinewasm_hir::CollectionFunc::MatrixCopy,
            pinewasm_hir::CollectionFunc::MatrixAddRow,
            pinewasm_hir::CollectionFunc::MatrixAddCol,
            pinewasm_hir::CollectionFunc::MatrixRemoveRow,
            pinewasm_hir::CollectionFunc::MatrixRemoveCol,
            pinewasm_hir::CollectionFunc::MatrixSwapRows,
            pinewasm_hir::CollectionFunc::MatrixSwapColumns,
            pinewasm_hir::CollectionFunc::MatrixReverse,
            pinewasm_hir::CollectionFunc::MatrixTranspose,
            pinewasm_hir::CollectionFunc::MatrixReshape,
            pinewasm_hir::CollectionFunc::MatrixSubmatrix,
            pinewasm_hir::CollectionFunc::MatrixConcat,
            pinewasm_hir::CollectionFunc::MatrixSum,
            pinewasm_hir::CollectionFunc::MatrixDiff,
            pinewasm_hir::CollectionFunc::MatrixMult,
            pinewasm_hir::CollectionFunc::MatrixDet,
            pinewasm_hir::CollectionFunc::MatrixInv,
            pinewasm_hir::CollectionFunc::MatrixTrace,
            pinewasm_hir::CollectionFunc::MatrixPow,
            pinewasm_hir::CollectionFunc::MatrixKron,
            pinewasm_hir::CollectionFunc::MatrixAvg,
            pinewasm_hir::CollectionFunc::MatrixMin,
            pinewasm_hir::CollectionFunc::MatrixMax,
            pinewasm_hir::CollectionFunc::MatrixMedian,
            pinewasm_hir::CollectionFunc::MatrixMode,
            pinewasm_hir::CollectionFunc::MatrixIsSquare,
            pinewasm_hir::CollectionFunc::MatrixIsIdentity,
            pinewasm_hir::CollectionFunc::MatrixIsDiagonal,
            pinewasm_hir::CollectionFunc::MatrixIsSymmetric,
            pinewasm_hir::CollectionFunc::MatrixIsAntisymmetric,
            pinewasm_hir::CollectionFunc::MatrixIsTriangular,
            pinewasm_hir::CollectionFunc::MatrixIsStochastic,
            pinewasm_hir::CollectionFunc::MatrixIsBinary,
            pinewasm_hir::CollectionFunc::MatrixIsZero,
            pinewasm_hir::CollectionFunc::MatrixIsAntidiagonal,
            pinewasm_hir::CollectionFunc::MatrixRank,
            pinewasm_hir::CollectionFunc::MatrixRow,
            pinewasm_hir::CollectionFunc::MatrixCol,
            pinewasm_hir::CollectionFunc::MatrixEigenvalues,
            pinewasm_hir::CollectionFunc::MatrixEigenvectors,
            pinewasm_hir::CollectionFunc::MatrixPinv,
            pinewasm_hir::CollectionFunc::MatrixSort,
        ];
        for argc in [0, 1, 127, u8::MAX] {
            for (min, max) in [(0, 0), (0, u8::MAX), (u8::MAX, u8::MAX), (u8::MAX, 0)] {
                for func in pure {
                    let issue = StructureIssue::PureArity {
                        func,
                        argc,
                        min,
                        max,
                    };
                    assert_eq!(issue.clone().message(), baseline_issue_message(issue));
                }
                for func in collections {
                    let issue = StructureIssue::CollectionArity {
                        func,
                        argc,
                        min,
                        max,
                    };
                    assert_eq!(issue.clone().message(), baseline_issue_message(issue));
                }
            }
        }
    }

    // Test-only copy of the orchestration body; only names are qualified for this test module.
    fn expected_validate_function(
        program: &CsilProgram,
        func: &crate::CsilFunction,
        errors: &mut Vec<ValidationError>,
    ) {
        let name = func.name.clone();
        let code_len = func.code.len();

        if code_len == 0 {
            errors.push(ValidationError::Structure {
                function: name,
                message: "function body is empty".to_string(),
            });
            return;
        }
        if func.source_map.len() != code_len {
            errors.push(ValidationError::Structure {
                function: name.clone(),
                message: format!(
                    "source map has {} entries for {} instructions",
                    func.source_map.len(),
                    code_len
                ),
            });
        }

        // Structural bounds for every instruction, reachable or not.
        let mut structural_ok = true;
        for (i, instr) in func.code.iter().enumerate() {
            let offset = instruction_offset(i);
            let mut bad = |message: String| {
                errors.push(ValidationError::AtInstruction {
                    function: name.clone(),
                    offset,
                    message,
                });
            };
            for issue in baseline_instruction_issues(program, instr, code_len) {
                bad(issue.message());
                structural_ok = false;
            }
        }
        // Over-declaration is rejected: hosts size buffers from this field, so an
        // adversarial program must not be able to request a huge allocation the
        // code never uses. Checked HERE, before the abstract interpretation
        // below, because that pass allocates one stack snapshot per pc and is
        // quadratic in code length — a program that is going to be rejected on a
        // field readable up front should not first be paid for.
        if func.max_stack > MAX_DECLARED_STACK {
            errors.push(ValidationError::Structure {
                function: name,
                message: format!(
                    "declared max_stack {} exceeds the limit of {MAX_DECLARED_STACK}",
                    func.max_stack
                ),
            });
            return;
        }

        if !structural_ok {
            return; // The stack simulation assumes in-range operands.
        }

        // Preflight stack *heights* before allocating a typed stack snapshot for
        // every instruction. The typed pass below is intentionally precise, but
        // retaining all of its vectors costs the sum of reachable stack heights.
        // A compact artifact can otherwise encode a long push-then-pop triangle
        // and force quadratic allocation before `max_stack` is rechecked.
        if let Err(error) = preflight_stack_heights(program, func, &name) {
            errors.push(error);
            return;
        }

        // Abstract interpretation over all paths: the typed worklist kernel
        // [`typed_stacks`], then the rendering of its outcome.
        let ta_arity = ta_arity_table(&program.ta_states);
        let tables = ProgramTables { program };
        let outcome = typed_stacks(&tables, &ta_arity, &func.code);
        let mut typed_errors = Vec::new();
        // `ReadSecurityExpr` visits in visit order: each one checks its `HtfExpr`
        // (a non-fatal error, reported once per visit, before any fatal one).
        for &pc in &outcome.htf_visits {
            if let CsilInstr::ReadSecurityExpr { expr, .. } = &func.code[pc] {
                if let Err(message) = validate_htf_expr(
                    expr,
                    count_htf_ta_nodes(expr),
                    count_htf_history_nodes(expr),
                    &program.inputs,
                ) {
                    typed_errors.push(ValidationError::AtInstruction {
                        function: name.clone(),
                        offset: instruction_offset(pc),
                        message,
                    });
                }
            }
        }
        let typed_result = match outcome.result {
            Ok(seen) => Ok(
                u32::try_from(seen).expect("the preflighted abstract stack ceiling fits in u32")
            ),
            Err(error) => {
                typed_errors.push(typed_error(&name, error, program.ta_states.len()));
                Err(())
            }
        };
        #[cfg(test)]
        typed_diff_tests::assert_typed_matches_baseline(
            program,
            func,
            &name,
            &typed_errors,
            typed_result,
        );
        errors.append(&mut typed_errors);
        let Ok(max_stack_seen) = typed_result else {
            return;
        };

        if max_stack_seen > func.max_stack {
            errors.push(ValidationError::Structure {
                function: name.clone(),
                message: format!(
                    "declared max_stack {} understates actual usage {}",
                    func.max_stack, max_stack_seen
                ),
            });
        }
        // NOTE: the MAX_DECLARED_STACK ceiling is checked before the abstract
        // interpretation above, not here — see the comment at that site.
    }

    fn program() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"Structure\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        let mut p = crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers");
        for f in &mut p.functions {
            f.budget_reference = None;
        }
        p.series_slots = vec![SlotDef {
            name: "sample".into(),
            persistent: false,
            varip: false,
            lane: None,
        }];
        p
    }

    fn install(p: &mut CsilProgram, code: Vec<CsilInstr>) {
        let span = p.functions[0].source_map[0];
        p.functions[0].source_map = vec![span; code.len()];
        p.functions[0].code = code;
        p.functions[0].max_stack = 1;
    }

    fn messages(p: &CsilProgram, instr: CsilInstr, len: usize) -> Vec<String> {
        instruction_issues(p, &instr, len)
            .into_iter()
            .map(StructureIssue::message)
            .collect()
    }

    #[test]
    fn fractional_transparency_is_valid_and_nonfinite_or_out_of_range_is_rejected() {
        let p = program();
        for t in [0.0, 0.1, 47.6, 100.0] {
            let color = CsilInstr::PushColor {
                r: 18,
                g: 52,
                b: 86,
                t: F64Bits::new(t),
            };
            assert!(instruction_issues(&p, &color, 1).is_empty(), "{t}");
        }
        for t in [-1.0, 100.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let color = CsilInstr::PushColor {
                r: 18,
                g: 52,
                b: 86,
                t: F64Bits::new(t),
            };
            assert_eq!(
                instruction_issues(&p, &color, 1),
                vec![StructureIssue::ColorTransparency { t: t.to_bits() }]
            );
        }
    }

    #[test]
    fn valid_scalar_and_invalid_unreachable_row() {
        let mut p = program();
        let code = vec![
            CsilInstr::ReadBuiltin {
                source: crate::BuiltinSeries::Close,
                offset: 0,
            },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::PlotF64 { plot_slot: 0 },
            CsilInstr::Return,
        ];
        for i in &code {
            assert!(instruction_issues(&p, i, code.len()).is_empty());
        }
        install(&mut p, code);
        assert_eq!(validate(&p), Ok(()));
        p.constants.clear();
        install(&mut p, vec![CsilInstr::Return, CsilInstr::PushStr(0)]);
        assert_eq!(
            validate(&p),
            Err(vec![ValidationError::AtInstruction {
                function: "main".into(),
                offset: 1,
                message: "string constant 0 out of range".into(),
            }])
        );
    }

    #[test]
    fn multi_issue_and_instruction_order_preserved() {
        let mut p = program();
        install(
            &mut p,
            vec![
                CsilInstr::WindowMean { slot: 1, length: 0 },
                CsilInstr::JumpIfSlotWritten { slot: 1, target: 3 },
                CsilInstr::Return,
            ],
        );
        assert_eq!(
            validate(&p),
            Err(vec![
                ValidationError::AtInstruction {
                    function: "main".into(),
                    offset: 0,
                    message: "series slot 1 out of range".into()
                },
                ValidationError::AtInstruction {
                    function: "main".into(),
                    offset: 0,
                    message: "window length 0 out of range (1..=100000)".into()
                },
                ValidationError::AtInstruction {
                    function: "main".into(),
                    offset: 1,
                    message: "series slot 1 out of range".into()
                },
                ValidationError::AtInstruction {
                    function: "main".into(),
                    offset: 1,
                    message: "jump target 3 out of range".into()
                },
            ])
        );
        // Guard precedence: an invalid slot suppresses the later offset arm.
        assert_eq!(
            messages(
                &p,
                CsilInstr::ReadUserSlot {
                    slot: 1,
                    offset: MAX_WINDOW_LENGTH + 1
                },
                3
            ),
            vec!["series slot 1 out of range"]
        );
        assert_eq!(
            messages(
                &p,
                CsilInstr::ReadUserSlot {
                    slot: 0,
                    offset: MAX_WINDOW_LENGTH + 1
                },
                3
            ),
            vec!["series slot offset 100001 exceeds the limit of 100000"]
        );
    }

    #[test]
    fn direct_bounds_and_registry_diagnostics() {
        let mut p = program();
        p.constants.clear();
        p.ta_states.clear();
        p.inputs.clear();
        p.visuals.clear();
        p.fills.clear();
        p.object_types.clear();
        let cases = vec![
            (
                CsilInstr::WindowExtremumDynamic {
                    slot: 1,
                    highest: true,
                },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::WindowStatisticDynamic {
                    slot: 1,
                    func: crate::TaFunc::Median,
                },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::JumpIfExact {
                    target: 1,
                    on: true,
                },
                "jump target 1 out of range",
            ),
            (CsilInstr::PushStr(0), "string constant 0 out of range"),
            (
                CsilInstr::PushF64(F64Bits::new(f64::NAN)),
                "float constants must be finite (use PUSH_NA)",
            ),
            (
                CsilInstr::PushColor {
                    r: 0,
                    g: 0,
                    b: 0,
                    t: crate::F64Bits::new(101.0),
                },
                "color transparency 101 out of range (0..=100)",
            ),
            (
                CsilInstr::ReadUserSlotDynamic { slot: 1 },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::StoreUserSlotCurrent { slot: 1 },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::WindowMeanDynamic { slot: 1 },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::EmaSimpleDynamic { slot: 1 },
                "series slot 1 out of range",
            ),
            (
                CsilInstr::CallPure {
                    func: PureFunc::MathSqrt,
                    argc: 0,
                },
                "CALL_PURE math.sqrt argc 0 out of range (1..=1)",
            ),
            (
                CsilInstr::CallCollection {
                    func: pinewasm_hir::CollectionFunc::ArrayCopy,
                    argc: 0,
                },
                "CALL_COLLECTION array.copy argc 0 out of range (1..=1)",
            ),
            (
                CsilInstr::CallPivotPointLevels { argc: 2 },
                "CALL_PIVOT_POINT_LEVELS argc 2, expected 3",
            ),
            (CsilInstr::CallTa { state: 0 }, "ta state 0 out of range"),
            (CsilInstr::ReadInput { input: 0 }, "input 0 out of range"),
            (
                CsilInstr::PlotF64 { plot_slot: 1 },
                "plot slot 1 out of range",
            ),
            (
                CsilInstr::PlotStyledF64 {
                    plot_slot: 1,
                    color_count: 0,
                },
                "plot slot 1 out of range",
            ),
            (
                CsilInstr::VisualColor {
                    visual_slot: 0,
                    color_count: 0,
                },
                "visual slot 0 out of range",
            ),
            (
                CsilInstr::FillColor {
                    fill_slot: 0,
                    color_count: 0,
                },
                "fill slot 0 out of range",
            ),
            (
                CsilInstr::NewObject {
                    type_id: 0,
                    field_count: 0,
                },
                "object type 0 out of range",
            ),
            (
                CsilInstr::CopyObject { type_id: 0 },
                "object type 0 out of range",
            ),
            (CsilInstr::Jump { target: 1 }, "jump target 1 out of range"),
            (
                CsilInstr::JumpIfFalse { target: 1 },
                "jump target 1 out of range",
            ),
        ];
        for (i, expected) in cases {
            assert_eq!(messages(&p, i, 1), vec![expected]);
        }
        assert!(instruction_issues(&p, &CsilInstr::PushF64(F64Bits::new(1.0)), 1).is_empty());
        assert!(instruction_issues(
            &p,
            &CsilInstr::CallPure {
                func: PureFunc::MathSqrt,
                argc: 1
            },
            1
        )
        .is_empty());
        assert!(instruction_issues(
            &p,
            &CsilInstr::CallCollection {
                func: pinewasm_hir::CollectionFunc::ArrayCopy,
                argc: 1
            },
            1
        )
        .is_empty());
    }

    #[test]
    fn metadata_dependent_guard_precedence_and_diagnostics() {
        let mut p = program();
        p.plots[0].color_roles.clear();
        assert_eq!(
            messages(
                &p,
                CsilInstr::PlotStyledF64 {
                    plot_slot: 0,
                    color_count: 1
                },
                1
            ),
            vec!["plot slot 0 declares 0 color channel(s), instruction has 1"]
        );
        p.visuals = vec![CsilVisual {
            title: None,
            style: pinewasm_hir::DisplayStyle::default(),
            color_roles: vec![],
        }];
        assert_eq!(
            messages(
                &p,
                CsilInstr::VisualColor {
                    visual_slot: 0,
                    color_count: 1
                },
                1
            ),
            vec!["visual slot 0 declares 0 color channel(s), instruction has 1"]
        );
        p.fills = vec![CsilFill {
            plot1: 1,
            plot2: 2,
            title: None,
            style: pinewasm_hir::DisplayStyle::default(),
            color_roles: vec![],
        }];
        assert_eq!(
            messages(
                &p,
                CsilInstr::FillColor {
                    fill_slot: 0,
                    color_count: 1
                },
                1
            ),
            vec!["fill slot 0 declares 0 color channel(s), instruction has 1"]
        );
        assert_eq!(
            messages(
                &p,
                CsilInstr::FillColor {
                    fill_slot: 0,
                    color_count: 0
                },
                1
            ),
            vec!["fill slot 0 references plot slot(s) 1/2 out of range"]
        );
        p.object_types = vec![pinewasm_hir::ObjectTypeDecl {
            name: "T".into(),
            field_count: 2,
            field_lanes: Vec::new(),
        }];
        assert_eq!(
            messages(
                &p,
                CsilInstr::NewObject {
                    type_id: 0,
                    field_count: 1
                },
                1
            ),
            vec!["NEW_OBJECT type 0 expects 2 field(s), got 1"]
        );
        assert!(instruction_issues(
            &p,
            &CsilInstr::NewObject {
                type_id: 0,
                field_count: 2
            },
            1
        )
        .is_empty());
    }
    // One explicit value for every current CsilInstr constructor, not a random subset.
    fn every_opcode() -> Vec<CsilInstr> {
        vec![
            CsilInstr::PushNa,
            CsilInstr::PushBool(false),
            CsilInstr::PushI64(0),
            CsilInstr::PushF64(crate::F64Bits::new(0.0)),
            CsilInstr::PushStr(0),
            CsilInstr::PushColor {
                r: 0,
                g: 0,
                b: 0,
                t: crate::F64Bits::new(0.0),
            },
            CsilInstr::Dup,
            CsilInstr::Pop,
            CsilInstr::ReadBuiltin {
                source: crate::BuiltinSeries::Close,
                offset: 0,
            },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::ReadBuiltinDynamic {
                source: crate::BuiltinSeries::Close,
            },
            CsilInstr::ReadUserSlotDynamic { slot: 0 },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadInput { input: 0 },
            CsilInstr::ReadSecurity {
                selector: None,
                source: crate::BuiltinSeries::Close,
                htf_seconds: 0,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            },
            CsilInstr::ReadSecurityExpr {
                expr: crate::HtfExpr::Source(crate::BuiltinSeries::Close),
                htf_seconds: 0,
                timeframe_expr: None,
                calendar_months: None,
                gaps_on: false,
                lookahead_on: false,
            },
            CsilInstr::ReadRequest {
                func: crate::RequestFunc::SecurityExpression,
                selector: String::new(),
            },
            CsilInstr::BuildFootprint {
                selector: String::new(),
            },
            CsilInstr::ReadSecurityLowerTf {
                selector: String::new(),
            },
            CsilInstr::ReadPivotLevels {
                system: crate::PivotSystem::Traditional,
                anchor_seconds: 0,
            },
            CsilInstr::CallPivotPointLevels { argc: 0 },
            CsilInstr::ReadTimeTf {
                close: false,
                tf_seconds: 0,
            },
            CsilInstr::FootprintRowByPrice,
            CsilInstr::WindowMean { slot: 0, length: 0 },
            CsilInstr::WindowMeanDynamic { slot: 0 },
            CsilInstr::WindowExtremumDynamic {
                slot: 0,
                highest: false,
            },
            CsilInstr::WindowStatisticDynamic {
                slot: 0,
                func: crate::TaFunc::Median,
            },
            CsilInstr::EmaSimpleDynamic { slot: 0 },
            CsilInstr::CallPure {
                func: PureFunc::MathSqrt,
                argc: 0,
            },
            CsilInstr::CallCollection {
                func: pinewasm_hir::CollectionFunc::ArrayCopy,
                argc: 0,
            },
            CsilInstr::CallTa { state: 0 },
            CsilInstr::IntAdd,
            CsilInstr::IntSub,
            CsilInstr::IntMul,
            CsilInstr::IntDiv,
            CsilInstr::PineIntDiv,
            CsilInstr::IntRem,
            CsilInstr::IntNeg,
            CsilInstr::F64Add,
            CsilInstr::F64Sub,
            CsilInstr::F64Mul,
            CsilInstr::F64Div,
            CsilInstr::F64Rem,
            CsilInstr::F64Neg,
            CsilInstr::IntToF64,
            CsilInstr::StrConcat,
            CsilInstr::Select,
            CsilInstr::CmpGt,
            CsilInstr::CmpGe,
            CsilInstr::CmpLt,
            CsilInstr::CmpLe,
            CsilInstr::CmpEq,
            CsilInstr::CmpNe,
            CsilInstr::BoolAnd,
            CsilInstr::BoolOr,
            CsilInstr::BoolNot,
            CsilInstr::Jump { target: 0 },
            CsilInstr::JumpIfFalse { target: 0 },
            CsilInstr::JumpIfExact {
                on: false,
                target: 0,
            },
            CsilInstr::JumpIfSlotWritten { slot: 0, target: 0 },
            CsilInstr::PlotF64 { plot_slot: 0 },
            CsilInstr::PlotStyledF64 {
                plot_slot: 0,
                color_count: 0,
            },
            CsilInstr::VisualColor {
                visual_slot: 0,
                color_count: 0,
            },
            CsilInstr::FillColor {
                fill_slot: 0,
                color_count: 0,
            },
            CsilInstr::RuntimeError,
            CsilInstr::NewObject {
                type_id: 0,
                field_count: 0,
            },
            CsilInstr::CopyObject { type_id: 0 },
            CsilInstr::GetField { field_idx: 0 },
            CsilInstr::SetField { field_idx: 0 },
            CsilInstr::RegisterDrawing {
                kind: pinewasm_hir::DrawKind::Line,
            },
            CsilInstr::ReadDrawingsAll {
                kind: pinewasm_hir::DrawKind::Line,
            },
            CsilInstr::DeleteDrawing {
                kind: pinewasm_hir::DrawKind::Line,
            },
            CsilInstr::SetCell { attrs: vec![] },
            CsilInstr::ClearCells,
            CsilInstr::MergeCells,
            CsilInstr::Log {
                level: pinewasm_hir::LogLevel::Info,
                argc: 0,
            },
            CsilInstr::Alert,
            CsilInstr::AlertCondition,
            CsilInstr::Order {
                kind: pinewasm_hir::OrderKind::Entry,
            },
            CsilInstr::Return,
        ]
    }

    #[test]
    fn primitives_match_pre_interface_for_every_opcode_and_boundary_family() {
        let p = program();
        let mut empty = p.clone();
        empty.constants.clear();
        empty.series_slots.clear();
        empty.plots.clear();
        empty.visuals.clear();
        empty.fills.clear();
        empty.ta_states.clear();
        empty.inputs.clear();
        empty.object_types.clear();
        let cases = every_opcode();
        assert_eq!(cases.len(), 80);
        for program in [&p, &empty] {
            for len in [0, 1, 3] {
                for instr in &cases {
                    assert_eq!(
                        instruction_issues(program, instr, len),
                        baseline_instruction_issues(program, instr, len),
                        "{instr:?}, length={len}"
                    );
                }
            }
            for v in [
                f64::NEG_INFINITY,
                -0.0,
                0.0,
                f64::MAX,
                f64::INFINITY,
                f64::NAN,
            ] {
                let instr = CsilInstr::PushF64(F64Bits::new(v));
                assert_eq!(
                    instruction_issues(program, &instr, 1),
                    baseline_instruction_issues(program, &instr, 1)
                );
            }
            for length in [0, 1, MAX_WINDOW_LENGTH, MAX_WINDOW_LENGTH + 1, u32::MAX] {
                let instr = CsilInstr::WindowMean { slot: 0, length };
                assert_eq!(
                    instruction_issues(program, &instr, 1),
                    baseline_instruction_issues(program, &instr, 1)
                );
            }
            for argc in [0, 1, 2, 3, 10, 11, u8::MAX] {
                for instr in [
                    CsilInstr::CallPure {
                        func: PureFunc::MathSqrt,
                        argc,
                    },
                    CsilInstr::CallPure {
                        func: PureFunc::MathMin,
                        argc,
                    },
                    CsilInstr::CallCollection {
                        func: pinewasm_hir::CollectionFunc::ArrayCopy,
                        argc,
                    },
                    CsilInstr::CallCollection {
                        func: pinewasm_hir::CollectionFunc::ArrayFrom,
                        argc,
                    },
                ] {
                    assert_eq!(
                        instruction_issues(program, &instr, 1),
                        baseline_instruction_issues(program, &instr, 1)
                    );
                }
            }
        }
    }

    struct PanicPrimitives;
    impl StructurePrimitives for PanicPrimitives {
        fn finite(&self, _: F64Bits) -> bool {
            panic!("scalar called finite")
        }
        fn color_transparency(&self, _: F64Bits) -> bool {
            panic!("scalar called color transparency")
        }
        fn window_length(&self, _: u32) -> bool {
            panic!("scalar called window length")
        }
        fn pure_arity(&self, _: PureFunc) -> (u8, u8) {
            panic!("scalar called pure arity")
        }
        fn collection_arity(&self, _: pinewasm_hir::CollectionFunc) -> (u8, u8) {
            panic!("scalar called collection arity")
        }
    }

    #[test]
    fn five_opcode_fragment_never_calls_primitives() {
        let p = program();
        for instr in [
            CsilInstr::ReadBuiltin {
                source: crate::BuiltinSeries::Close,
                offset: 0,
            },
            CsilInstr::StoreUserSlotCurrent { slot: 0 },
            CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
            CsilInstr::PlotF64 { plot_slot: 0 },
            CsilInstr::Return,
        ] {
            assert!(instruction_issues_with_primitives(&PanicPrimitives, &p, &instr, 5).is_empty());
        }
    }
    #[derive(Default)]
    struct RecordingSink(Vec<(usize, StructureIssue)>);
    impl StructureIssueSink for RecordingSink {
        fn emit(&mut self, offset: usize, issue: StructureIssue) {
            self.0.push((offset, issue));
        }
    }

    #[test]
    fn streaming_scan_matches_frozen_all_opcode_sequence_and_full_diagnostics() {
        let p = program();
        let mut empty_tables = p.clone();
        empty_tables.constants.clear();
        empty_tables.series_slots.clear();
        empty_tables.plots.clear();
        empty_tables.visuals.clear();
        empty_tables.fills.clear();
        empty_tables.ta_states.clear();
        empty_tables.inputs.clear();
        empty_tables.object_types.clear();
        let opcodes = every_opcode();
        assert_eq!(opcodes.len(), 80);
        for base in [&p, &empty_tables] {
            let mut bodies: Vec<Vec<CsilInstr>> =
                opcodes.iter().cloned().map(|i| vec![i]).collect();
            bodies.push(opcodes.clone());
            bodies.push(vec![]);
            for code in bodies {
                for malformed_map in [false, true] {
                    for declared in [0, 1, MAX_DECLARED_STACK + 1] {
                        let mut candidate = base.clone();
                        install(&mut candidate, code.clone());
                        candidate.functions[0].max_stack = declared;
                        if malformed_map {
                            candidate.functions[0].source_map.clear();
                        }
                        let func = &candidate.functions[0];
                        let expected: Vec<_> = func
                            .code
                            .iter()
                            .enumerate()
                            .flat_map(|(pc, instr)| {
                                baseline_instruction_issues(&candidate, instr, func.code.len())
                                    .into_iter()
                                    .map(move |issue| (pc, issue))
                            })
                            .collect();
                        let mut sink = RecordingSink::default();
                        let ok = structural_issues(&mut sink, &candidate, func);
                        assert_eq!(ok, expected.is_empty());
                        assert_eq!(sink.0, expected);
                        let mut actual = vec![ValidationError::NoFunctions];
                        let mut legacy = vec![ValidationError::NoFunctions];
                        super::super::validate_function(&candidate, func, &mut actual);
                        expected_validate_function(&candidate, func, &mut legacy);
                        assert_eq!(
                            actual, legacy,
                            "code={code:?}, map={malformed_map}, declared={declared}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn streaming_multiple_issues_and_unreachable_rows_keep_offsets_and_precedence() {
        let mut p = program();
        install(
            &mut p,
            vec![
                CsilInstr::Return,
                CsilInstr::ReadUserSlot {
                    slot: 1,
                    offset: 100001,
                },
                CsilInstr::WindowMean { slot: 1, length: 0 },
                CsilInstr::PlotF64 { plot_slot: 1 },
            ],
        );
        let mut sink = RecordingSink::default();
        assert!(!structural_issues(&mut sink, &p, &p.functions[0]));
        let expected: Vec<_> = p.functions[0]
            .code
            .iter()
            .enumerate()
            .flat_map(|(pc, instr)| {
                baseline_instruction_issues(&p, instr, p.functions[0].code.len())
                    .into_iter()
                    .map(move |issue| (pc, issue))
            })
            .collect();
        assert_eq!(sink.0, expected);
        assert_eq!(
            sink.0.iter().map(|(pc, _)| *pc).collect::<Vec<_>>(),
            vec![1, 2, 2, 3]
        );
    }

    #[test]
    fn admitted_scalar_body_does_not_call_sink_or_primitives() {
        struct PanicSink;
        impl StructureIssueSink for PanicSink {
            fn emit(&mut self, _: usize, _: StructureIssue) {
                panic!("valid row invoked sink");
            }
        }
        let mut p = program();
        install(
            &mut p,
            vec![
                CsilInstr::ReadBuiltin {
                    source: crate::BuiltinSeries::Close,
                    offset: 0,
                },
                CsilInstr::StoreUserSlotCurrent { slot: 0 },
                CsilInstr::ReadUserSlot { slot: 0, offset: 0 },
                CsilInstr::PlotF64 { plot_slot: 0 },
                CsilInstr::Return,
            ],
        );
        assert!(structural_issues_with_primitives(
            &PanicPrimitives,
            &mut PanicSink,
            &p,
            &p.functions[0]
        ));
    }
}
