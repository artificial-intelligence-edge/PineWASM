//! Location projection used by the interpreter's actual error constructor.

use crate::RuntimeErrorKind;
use pinewasm_span::Span;

/// Preserve the reporting casts while looking up the original, untruncated PC.
/// Source IDs and span payloads are copied verbatim, including synthetic spans.
/// This is the initial VM error location: `run_owned` may later replace its span
/// when a second run of the same program, used to recover the budget-failure
/// span, has the same committed visible prefix.
pub(crate) fn fault_location(
    bar: usize,
    pc: usize,
    source_map: &[Span],
) -> (u32, u32, Option<Span>) {
    let bar_index = bar as u32;
    let instruction_offset = pc as u32;
    let span = source_map.get(pc).copied();
    (bar_index, instruction_offset, span)
}

/// Whether `run_owned` should run the same program a second time to recover a
/// budget-failure source span.
pub(crate) fn should_replay_budget_reference(
    has_realtime: bool,
    has_budget_reference: bool,
    error_count: usize,
    error_kind: Option<RuntimeErrorKind>,
) -> bool {
    !has_realtime
        && has_budget_reference
        && error_count == 1
        && matches!(
            error_kind,
            Some(
                RuntimeErrorKind::InstructionLimitExceeded
                    | RuntimeErrorKind::RunInstructionLimitExceeded
            )
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetErrorSummary {
    pub(crate) error_count: usize,
    pub(crate) kind: Option<RuntimeErrorKind>,
    pub(crate) bar_index: Option<u32>,
    pub(crate) span: Option<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetSpanSelection {
    KeepOptimized,
    UseReference(Option<Span>),
}

/// Decide whether a second run of the same program may replace the current
/// error's source span. `visible_prefix_matches` is computed by `run_owned`
/// using the exact fields it already compares.
pub(crate) fn select_budget_reference_span(
    current: BudgetErrorSummary,
    reference: BudgetErrorSummary,
    visible_prefix_matches: Option<bool>,
) -> BudgetSpanSelection {
    if current.error_count != 1
        || reference.error_count != 1
        || current.bar_index != reference.bar_index
        || visible_prefix_matches != Some(true)
    {
        return BudgetSpanSelection::KeepOptimized;
    }
    match (current.kind, reference.kind) {
        (
            Some(RuntimeErrorKind::InstructionLimitExceeded),
            Some(RuntimeErrorKind::InstructionLimitExceeded),
        )
        | (
            Some(RuntimeErrorKind::RunInstructionLimitExceeded),
            Some(RuntimeErrorKind::RunInstructionLimitExceeded),
        ) => BudgetSpanSelection::UseReference(reference.span),
        _ => BudgetSpanSelection::KeepOptimized,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fault_location, select_budget_reference_span, should_replay_budget_reference,
        BudgetErrorSummary, BudgetSpanSelection,
    };
    use crate::{MarketData, RunOptions, RuntimeError, RuntimeErrorKind, Vm};
    use pinewasm_span::{SourceId, Span};

    fn spans() -> [Span; 3] {
        [
            Span::new(SourceId(19), 90, 97),
            Span::dummy(),
            // Imported payloads are copied, not normalized or re-validated.
            Span {
                source: SourceId(u32::MAX),
                start: 11,
                end: 3,
            },
        ]
    }

    #[test]
    fn matches_old_expression_at_indices_and_reporting_boundaries() {
        let rows = spans();
        let mut indices = vec![0, 1, 2, 3, 4, usize::MAX];
        if let Some(above_u32) = (u32::MAX as usize).checked_add(1) {
            indices.push(u32::MAX as usize);
            indices.push(above_u32);
        }
        for len in 0..=rows.len() {
            for &bar in &indices {
                for &pc in &indices {
                    assert_eq!(
                        fault_location(bar, pc, &rows[..len]),
                        (bar as u32, pc as u32, rows[..len].get(pc).copied())
                    );
                }
            }
        }
    }

    #[test]
    fn imported_dummy_and_nonmonotonic_payloads_are_not_rewritten() {
        let rows = spans();
        assert_eq!(fault_location(7, 0, &rows), (7, 0, Some(rows[0])));
        assert_eq!(fault_location(7, 1, &rows), (7, 1, Some(Span::dummy())));
        assert_eq!(fault_location(7, 2, &rows), (7, 2, Some(rows[2])));
        assert_eq!(fault_location(7, 3, &rows), (7, 3, None));
    }

    #[test]
    fn real_vm_fail_preserves_kind_message_and_original_index() {
        let parsed = pinewasm_parse::parse("indicator(\"fault\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        let mir = pinewasm_mir::lower(&analysis.hir);
        let program = pinewasm_csil::lower_mir(&mir).expect("focused fixture lowers");
        let data = MarketData {
            open: vec![1.0],
            high: vec![2.0],
            low: vec![0.0],
            close: vec![1.5],
            volume: vec![10.0],
            ..MarketData::default()
        };
        let mut vm = Vm::new(&program, &data, &RunOptions::default()).expect("fixture VM");
        let rows = spans();
        std::sync::Arc::make_mut(&mut vm.program).functions[0].source_map = rows.to_vec();
        let mut indices = vec![0, 1, 2, 3, usize::MAX];
        if let Some(above_u32) = (u32::MAX as usize).checked_add(1) {
            indices.push(above_u32);
        }
        for &bar in &indices {
            vm.bar = bar;
            for &pc in &indices {
                for kind in [
                    RuntimeErrorKind::InvalidProgram,
                    RuntimeErrorKind::InstructionLimitExceeded,
                ] {
                    let message = format!("fault at original PC {pc}: \u{3bb}");
                    assert_eq!(
                        vm.fail(kind, message.clone(), pc),
                        RuntimeError {
                            kind,
                            message,
                            bar_index: Some(bar as u32),
                            instruction_offset: Some(pc as u32),
                            span: rows.get(pc).copied(),
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn budget_reference_replay_gate_is_history_only_and_budget_only() {
        assert!(should_replay_budget_reference(
            false,
            true,
            1,
            Some(RuntimeErrorKind::InstructionLimitExceeded)
        ));
        assert!(should_replay_budget_reference(
            false,
            true,
            1,
            Some(RuntimeErrorKind::RunInstructionLimitExceeded)
        ));
        assert!(!should_replay_budget_reference(
            true,
            true,
            1,
            Some(RuntimeErrorKind::InstructionLimitExceeded)
        ));
        assert!(!should_replay_budget_reference(
            false,
            false,
            1,
            Some(RuntimeErrorKind::InstructionLimitExceeded)
        ));
        assert!(!should_replay_budget_reference(
            false,
            true,
            0,
            Some(RuntimeErrorKind::InstructionLimitExceeded)
        ));
        assert!(!should_replay_budget_reference(
            false,
            true,
            2,
            Some(RuntimeErrorKind::InstructionLimitExceeded)
        ));
        assert!(!should_replay_budget_reference(
            false,
            true,
            1,
            Some(RuntimeErrorKind::InvalidProgram)
        ));
        assert!(!should_replay_budget_reference(false, true, 1, None));
    }

    #[test]
    fn budget_reference_span_selector_requires_matching_visible_prefix() {
        let span = Span::new(SourceId(7), 11, 19);
        let budget = RuntimeErrorKind::InstructionLimitExceeded;
        let summary = |error_count, kind, bar_index, error_span| BudgetErrorSummary {
            error_count,
            kind,
            bar_index,
            span: error_span,
        };

        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::UseReference(Some(span))
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), None),
                Some(true),
            ),
            BudgetSpanSelection::UseReference(None)
        );
        assert_eq!(
            select_budget_reference_span(
                summary(
                    1,
                    Some(RuntimeErrorKind::RunInstructionLimitExceeded),
                    Some(4),
                    None
                ),
                summary(
                    1,
                    Some(RuntimeErrorKind::RunInstructionLimitExceeded),
                    Some(4),
                    Some(span)
                ),
                Some(true),
            ),
            BudgetSpanSelection::UseReference(Some(span))
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                Some(false),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                None,
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(0, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(2, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(0, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(2, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, Some(budget), Some(5), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(RuntimeErrorKind::InvalidProgram), Some(4), None),
                summary(
                    1,
                    Some(RuntimeErrorKind::InvalidProgram),
                    Some(4),
                    Some(span)
                ),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, None, Some(4), None),
                summary(1, Some(budget), Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
        assert_eq!(
            select_budget_reference_span(
                summary(1, Some(budget), Some(4), None),
                summary(1, None, Some(4), Some(span)),
                Some(true),
            ),
            BudgetSpanSelection::KeepOptimized
        );
    }
}
