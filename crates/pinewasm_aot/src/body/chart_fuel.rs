//! The fuel-window kernel for every native module.
//!
//! A chart module and a standalone WebAssembly module charge a run of
//! straight-line instructions with one guarded prelude instead of one check
//! per instruction. An instruction may cost more than one unit, may stop the
//! run with a status, and may be the block's terminator, whose unit closes the
//! window while its branch follows it. None of that changes what a run
//! reports:
//!
//! - The prelude subtracts the window's summed cost from both counters only
//!   when both cover it. Each instruction's charge then succeeds in the
//!   original order too, so the fast arm skips no failure. When either
//!   counter is short, the other arm is the original per-instruction path and
//!   stops at the same instruction with the same status.
//! - An instruction that stops the run returns from it, and nothing reads a
//!   counter after that: both are locals of `bar_step` (a standalone module's
//!   per-instruction path keeps them in private globals while it runs), and
//!   no status reports them. The error triple, the committed effects and the
//!   bars executed are the original's.
//!
//! An instruction whose emitted code reads or writes a counter (a `host.vm`
//! site, a runtime or in-op extra charge, a dynamic dispatch cost) is never
//! grouped. The emitter computes [`ChartFacts`], asks [`chart_group_len`], and
//! refuses the module if a grouped instruction touches a counter anyway.
//!
//! The kernel is written for Charon/Aeneas: free functions, explicit loops,
//! no closures or iterator adaptors.

/// The fewest charged instructions worth a grouped prelude. The prelude costs
/// about one per-instruction check, so two checks are already a saving.
pub(crate) const CHART_GROUP_MIN: u32 = 2;

/// The largest precharge one window may carry, and so the largest cost of
/// any one of its instructions. It keeps the prelude's `i64.const` the exact
/// unsigned sum and the kernel's own addition far from overflow.
pub(crate) const CHART_GROUP_COST_MAX: u64 = 1 << 32;

/// What the emitter knows about one chart instruction, as far as grouping
/// cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChartFacts {
    /// What the original path charges before the instruction:
    /// `analysis::instr_cost`, or zero for a budget-free carry instruction.
    pub cost: u64,
    /// The block's terminator (`Jump*`, `Return`). A window may precharge its
    /// unit as the last element; the branch itself is emitted after the
    /// window.
    pub terminator: bool,
    /// The instruction's emitted code reads or writes a budget counter.
    pub meters: bool,
}

/// Whether the instruction can join a window at all.
pub(crate) fn chart_group_eligible(f: &ChartFacts) -> bool {
    !f.meters && f.cost <= CHART_GROUP_COST_MAX
}

/// The window at the start of `window` (the facts of the instructions from
/// the group's first pc up to the end of its block): its length and the
/// summed cost its prelude precharges, or `None` when it would charge fewer
/// than [`CHART_GROUP_MIN`] instructions. The window stops before the first
/// ineligible instruction, before the sum would pass
/// [`CHART_GROUP_COST_MAX`], and right after a terminator.
pub(crate) fn chart_group_len(window: &[ChartFacts]) -> Option<(usize, u64)> {
    let mut k = 0;
    let mut total: u64 = 0;
    let mut charged: u32 = 0;
    while k < window.len() {
        let f = &window[k];
        if !chart_group_eligible(f) {
            break;
        }
        // `total` and `f.cost` are both at most CHART_GROUP_COST_MAX, so the
        // sum cannot wrap.
        let next = total + f.cost;
        if next > CHART_GROUP_COST_MAX {
            break;
        }
        total = next;
        if f.cost > 0 {
            charged += 1;
        }
        k += 1;
        if f.terminator {
            break;
        }
    }
    if charged >= CHART_GROUP_MIN {
        Some((k, total))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(cost: u64) -> ChartFacts {
        ChartFacts {
            cost,
            terminator: false,
            meters: false,
        }
    }

    fn term() -> ChartFacts {
        ChartFacts {
            cost: 1,
            terminator: true,
            meters: false,
        }
    }

    fn meter() -> ChartFacts {
        ChartFacts {
            cost: 1,
            terminator: false,
            meters: true,
        }
    }

    #[test]
    fn a_window_sums_weighted_costs() {
        assert_eq!(chart_group_len(&[op(1), op(5), op(1)]), Some((3, 7)));
    }

    #[test]
    fn one_charged_instruction_is_not_worth_a_prelude() {
        assert_eq!(chart_group_len(&[op(1)]), None);
        assert_eq!(chart_group_len(&[op(0), op(3), op(0)]), None);
        assert_eq!(chart_group_len(&[]), None);
    }

    #[test]
    fn budget_free_instructions_ride_along_at_no_cost() {
        assert_eq!(chart_group_len(&[op(0), op(1), op(0), op(1)]), Some((4, 2)));
    }

    #[test]
    fn a_metering_instruction_ends_the_window() {
        assert_eq!(
            chart_group_len(&[op(1), op(1), meter(), op(1)]),
            Some((2, 2))
        );
        assert_eq!(chart_group_len(&[meter(), op(1), op(1)]), None);
    }

    #[test]
    fn a_terminator_closes_the_window_with_its_unit() {
        assert_eq!(chart_group_len(&[op(1), term()]), Some((2, 2)));
        assert_eq!(chart_group_len(&[op(1), term(), op(1)]), Some((2, 2)));
        assert_eq!(chart_group_len(&[term(), op(1), op(1)]), None);
    }

    #[test]
    fn the_precharge_never_passes_the_cap() {
        let big = op(CHART_GROUP_COST_MAX);
        assert_eq!(chart_group_len(&[big, op(1)]), None);
        assert_eq!(chart_group_len(&[op(1), big]), None);
        assert_eq!(
            chart_group_len(&[op(1), op(CHART_GROUP_COST_MAX - 1), op(1)]),
            Some((2, CHART_GROUP_COST_MAX))
        );
        assert_eq!(
            chart_group_len(&[op(CHART_GROUP_COST_MAX + 1), op(1)]),
            None
        );
    }
}
