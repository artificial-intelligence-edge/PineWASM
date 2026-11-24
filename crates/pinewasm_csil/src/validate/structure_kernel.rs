//! Formatting-free structural predicates called by production validation.
//! No diagnostics, instruction rewrites, or successful stack interpretation.
use super::MAX_WINDOW_LENGTH;

pub(crate) fn index_in_range(index: u32, count: usize) -> bool {
    (index as usize) < count
}

pub(crate) fn window_offset_admitted(offset: u32) -> bool {
    offset <= MAX_WINDOW_LENGTH
}

// CSIL offsets are zero-based: length u32::MAX + 1 is admitted when usize
// can represent it. On 32-bit targets every representable length fits.
#[allow(
    clippy::checked_conversions,
    reason = "Keep primitive bounds explicit in the production structural kernel"
)]
pub(crate) fn instruction_count_admitted(len: usize) -> bool {
    if len == 0 {
        true
    } else {
        len - 1 <= u32::MAX as usize
    }
}

#[cfg(test)]
mod tests {
    use super::super::{validate, ValidationError};
    use super::*;
    use crate::{CsilInstr, CsilProgram};

    fn program() -> CsilProgram {
        let parsed =
            pinewasm_parse::parse("//@version=6\nindicator(\"Structure\")\nx = close\nplot(x)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        let mut program = crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers");
        // Mutations test the production structural guards directly.
        for function in &mut program.functions {
            function.budget_reference = None;
        }
        if program.series_slots.is_empty() {
            program.series_slots.push(crate::SlotDef {
                name: "probe".into(),
                persistent: false,
                varip: false,
                lane: None,
            });
        }
        program
    }

    #[test]
    fn primitive_index_boundaries() {
        assert!(!index_in_range(0, 0));
        assert!(index_in_range(0, 1));
        assert!(!index_in_range(1, 1));
        assert!(index_in_range(1, 2));
        assert!(!index_in_range(u32::MAX, u32::MAX as usize));
    }

    #[test]
    fn primitive_window_boundaries() {
        assert!(window_offset_admitted(0));
        assert!(window_offset_admitted(MAX_WINDOW_LENGTH));
        assert!(!window_offset_admitted(MAX_WINDOW_LENGTH + 1));
        assert!(!window_offset_admitted(u32::MAX));
    }

    #[test]
    fn primitive_instruction_count_boundaries() {
        assert!(instruction_count_admitted(0));
        assert!(instruction_count_admitted(1));
        assert!(instruction_count_admitted(u32::MAX as usize));
        #[cfg(target_pointer_width = "64")]
        {
            let max = u32::MAX as usize;
            assert!(instruction_count_admitted(max + 1));
            assert!(!instruction_count_admitted(max + 2));
            assert!(!instruction_count_admitted(usize::MAX));
        }
        #[cfg(target_pointer_width = "32")]
        {
            // u32::MAX + 1 is not a usize value on this target.
            assert!(instruction_count_admitted(usize::MAX));
        }
    }

    #[test]
    fn production_slot_guard_precedes_offset_guard() {
        let mut p = program();
        let count = u32::try_from(p.series_slots.len()).unwrap();
        let pos = p.functions[0]
            .code
            .iter()
            .position(|i| {
                matches!(
                    i,
                    CsilInstr::ReadUserSlot { .. } | CsilInstr::ReadBuiltin { .. }
                )
            })
            .expect("read slot");
        p.functions[0].code[pos] = CsilInstr::ReadUserSlot {
            slot: count,
            offset: MAX_WINDOW_LENGTH + 1,
        };
        let errors = validate(&p).unwrap_err();
        assert_eq!(
            errors,
            vec![ValidationError::AtInstruction {
                function: p.functions[0].name.clone(),
                offset: u32::try_from(pos).unwrap(),
                message: format!("series slot {count} out of range"),
            }]
        );
        p.functions[0].code[pos] = CsilInstr::ReadUserSlot {
            slot: 0,
            offset: MAX_WINDOW_LENGTH + 1,
        };
        assert_eq!(
            validate(&p).unwrap_err(),
            vec![ValidationError::AtInstruction {
                function: p.functions[0].name.clone(),
                offset: u32::try_from(pos).unwrap(),
                message: format!(
                    "series slot offset {} exceeds the limit of {MAX_WINDOW_LENGTH}",
                    MAX_WINDOW_LENGTH + 1
                ),
            }]
        );
    }

    #[test]
    fn production_plot_guard_and_zero_table() {
        let mut p = program();
        let pos = p.functions[0]
            .code
            .iter()
            .position(|i| matches!(i, CsilInstr::PlotStyledF64 { .. }))
            .expect("plot");
        let count = u32::try_from(p.plots.len()).unwrap();
        p.functions[0].code[pos] = CsilInstr::PlotStyledF64 {
            plot_slot: count,
            color_count: 1,
        };
        assert_eq!(
            validate(&p).unwrap_err(),
            vec![ValidationError::AtInstruction {
                function: p.functions[0].name.clone(),
                offset: u32::try_from(pos).unwrap(),
                message: format!("plot slot {count} out of range"),
            }]
        );
        p.plots.clear();
        p.functions[0].code[pos] = CsilInstr::PlotStyledF64 {
            plot_slot: 0,
            color_count: 1,
        };
        assert_eq!(
            validate(&p).unwrap_err(),
            vec![ValidationError::AtInstruction {
                function: p.functions[0].name.clone(),
                offset: u32::try_from(pos).unwrap(),
                message: "plot slot 0 out of range".into(),
            }]
        );
    }
}
