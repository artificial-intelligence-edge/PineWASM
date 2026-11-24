//! Integer kernels behind [`crate::CsilProgram::history_depth_with_ta_inputs`]
//! and [`crate::CsilProgram::history_ring`].
//!
//! Free functions over plain integers and slices, written in an extractable
//! style. The methods walk `CsilInstr` (which carries `f64`) and look TA
//! overrides up in a `HashMap`; they only *resolve* each lookback observer
//! into a [`DepthObserver`] (with [`ta_override_u32`], a checked cast), and
//! the depth itself is computed here by [`history_depth_scan`] over that
//! pre-resolved slice.

use crate::MAX_WINDOW_LENGTH;

/// One instruction that can reach backwards, with its TA override already
/// looked up and converted to `u32`. The slot number is not needed once the
/// override is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DepthObserver {
    /// `ReadUserSlot { offset }` with its resolved `ta.change` length
    /// override (always `None` for a zero offset, which is never overridden).
    ReadUserSlot { offset: u32, change: Option<u32> },
    /// `WindowMean { length }` with its resolved `ta.sma` length override.
    WindowMean { length: u32, sma: Option<u32> },
    /// A runtime-length mean, extremum or statistic can reach the supported
    /// maximum, [`MAX_DYNAMIC_WINDOW_DEPTH`] cells back.
    DynamicWindow,
}

/// Convert a TA length override to `u32` with a checked cast: a value in
/// `0..=u32::MAX` is kept, anything else becomes `u32::MAX`.
///
/// Callers pass values that passed `resolve_ta_input`'s bounds
/// (`1..=MAX_WINDOW_LENGTH`), where this is the identity. Outside them a plain
/// `value as u32` would truncate (for example `2^32 + 1` would become `1`),
/// which could plan a ring *smaller* than the override's lookback; saturating
/// to `u32::MAX` instead over-approximates the depth, so ring sizing fails
/// (`ring_size` returns `None`) rather than under-allocating.
#[inline]
pub(crate) fn ta_override_u32(value: i64) -> u32 {
    if value < 0 || value > u32::MAX as i64 {
        u32::MAX
    } else {
        value as u32
    }
}

/// Fold one `ReadUserSlot { offset }` into the running depth. A zero offset
/// reads the current bar and is never overridden; otherwise a resolved
/// `ta.change` length override replaces the literal offset.
#[inline]
pub(crate) fn read_user_slot_depth(deepest: u32, offset: u32, change_override: Option<u32>) -> u32 {
    let offset = if offset == 0 {
        0
    } else {
        match change_override {
            Some(value) => value,
            None => offset,
        }
    };
    deepest.max(offset)
}

/// Fold one `WindowMean { length }` into the running depth: a window of
/// `length` cells ending at the current bar reaches `length - 1` back. A
/// resolved `ta.sma` length override replaces the literal length.
#[inline]
pub(crate) fn window_mean_depth(deepest: u32, length: u32, sma_override: Option<u32>) -> u32 {
    let length = match sma_override {
        Some(value) => value,
        None => length,
    };
    deepest.max(length.saturating_sub(1))
}

/// Deepest history cell a runtime-length window can read: a window of
/// [`MAX_WINDOW_LENGTH`] cells ending at the current bar reaches one less
/// than that back. The ring is sized from this and the VM's runtime length
/// guards admit `1..=MAX_WINDOW_LENGTH`, so the two cannot drift apart.
pub const MAX_DYNAMIC_WINDOW_DEPTH: u32 = MAX_WINDOW_LENGTH - 1;

/// Fold one pre-resolved observer into the running depth.
#[inline]
pub(crate) fn observer_depth(deepest: u32, observer: DepthObserver) -> u32 {
    match observer {
        DepthObserver::ReadUserSlot { offset, change } => {
            read_user_slot_depth(deepest, offset, change)
        }
        DepthObserver::WindowMean { length, sma } => window_mean_depth(deepest, length, sma),
        DepthObserver::DynamicWindow => deepest.max(MAX_DYNAMIC_WINDOW_DEPTH),
    }
}

/// The shallowest history any program keeps: one bar. The bar preamble
/// copies a `var` slot forward from the previous bar's cell, and re-running
/// the last committed bar in realtime needs that cell distinct from the
/// current one, so the ring never shrinks below two cells.
pub const MIN_HISTORY_DEPTH: u32 = 1;

/// Where the history-depth scan starts. `max_bars_back` bounds only dynamic
/// slot reads (`ReadUserSlotDynamic`), so it counts only when the program
/// has one; otherwise nothing can observe it and the floor is
/// [`MIN_HISTORY_DEPTH`].
#[inline]
pub(crate) fn history_depth_base(max_bars_back: u32, dynamic_slot_read: bool) -> u32 {
    if dynamic_slot_read && max_bars_back > MIN_HISTORY_DEPTH {
        max_bars_back
    } else {
        MIN_HISTORY_DEPTH
    }
}

/// The history-depth scan: start from `base` ([`history_depth_base`]) and
/// fold every pre-resolved observer into the running maximum.
pub(crate) fn history_depth_scan(base: u32, observers: &[DepthObserver]) -> u32 {
    let mut deepest = base;
    let mut i = 0;
    while i < observers.len() {
        deepest = observer_depth(deepest, observers[i]);
        i += 1;
    }
    deepest
}

/// `n.next_power_of_two()` for `u32`, written out so it is extracted code
/// rather than a trusted model of the standard library: the smallest power
/// of two `>= n` (`1` for `0`). Above `2^31` no `u32` power of two exists and,
/// exactly like std (`u32::MAX + 1` under inherited overflow checks), the
/// final doubling overflows: a panic with debug assertions, `0` without.
#[inline]
pub(crate) fn next_power_of_two(n: u32) -> u32 {
    let mut power: u32 = 1;
    while power < n {
        if power > u32::MAX / 2 {
            // `power == 2^31 < n`: overflow exactly as std does.
            return power * 2;
        }
        power *= 2;
    }
    power
}

/// The ring size the AOT host path uses (via [`crate::CsilProgram::history_ring`]):
/// `(depth + 1).next_power_of_two()` with a saturating add and an unchecked
/// [`next_power_of_two`], which overflows (debug panic, release `0`) for
/// `depth > 2^31 - 1`.
#[inline]
pub(crate) fn history_ring_aot(depth: u32) -> u32 {
    next_power_of_two(depth.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::{
        history_depth_base, history_depth_scan, history_ring_aot, next_power_of_two,
        read_user_slot_depth, ta_override_u32, window_mean_depth, DepthObserver,
        MAX_DYNAMIC_WINDOW_DEPTH, MIN_HISTORY_DEPTH,
    };
    use crate::{CsilInstr, CsilProgram};
    use pinewasm_hir::TaInputTarget;
    use std::collections::HashMap;

    fn read_reference(deepest: u32, offset: u32, change: Option<i64>) -> u32 {
        let offset = if offset == 0 {
            0
        } else {
            change.map_or(offset, |value| value as u32)
        };
        deepest.max(offset)
    }

    fn window_reference(deepest: u32, length: u32, sma: Option<i64>) -> u32 {
        let length = sma.map_or(length, |value| value as u32);
        deepest.max(length.saturating_sub(1))
    }

    const U32_EDGES: [u32; 9] = [0, 1, 2, 3, 99, 500, 1 << 31, u32::MAX - 1, u32::MAX];
    const I64_EDGES: [Option<i64>; 9] = [
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(5000),
        Some(u32::MAX as i64),
        Some(u32::MAX as i64 + 1),
        Some(-1),
        Some(i64::MAX),
    ];

    #[test]
    fn depth_kernels_match_reference() {
        for &deepest in &U32_EDGES {
            for &literal in &U32_EDGES {
                for &value in &I64_EDGES {
                    let resolved = value.map(|value| value as u32);
                    assert_eq!(
                        read_user_slot_depth(deepest, literal, resolved),
                        read_reference(deepest, literal, value),
                        "read deepest={deepest} offset={literal} value={value:?}"
                    );
                    assert_eq!(
                        window_mean_depth(deepest, literal, resolved),
                        window_reference(deepest, literal, value),
                        "window deepest={deepest} length={literal} value={value:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ta_override_u32_is_a_checked_cast() {
        for value in [0, 1, 2, 5000, 100_000, u32::MAX as i64 - 1, u32::MAX as i64] {
            assert_eq!(ta_override_u32(value), value as u32, "value={value}");
            assert_eq!(ta_override_u32(value), u32::try_from(value).unwrap());
        }
        for value in [
            -1,
            i64::MIN,
            u32::MAX as i64 + 1,
            u32::MAX as i64 + 2,
            (1 << 32) + 5,
            i64::MAX,
        ] {
            assert!(u32::try_from(value).is_err());
            assert_eq!(ta_override_u32(value), u32::MAX, "value={value}");
        }
        // A plain truncating cast would plan 1 here; the checked one is never
        // less than the (unrepresentable) request.
        assert_eq!((((1_i64 << 32) + 1) as u32), 1);
        assert_eq!(ta_override_u32((1 << 32) + 1), u32::MAX);
    }

    #[test]
    fn next_power_of_two_matches_std_in_range() {
        for n in (0..=4096u32).chain([
            (1 << 30) - 1,
            1 << 30,
            (1 << 30) + 1,
            (1 << 31) - 1,
            1 << 31,
        ]) {
            assert_eq!(next_power_of_two(n), n.next_power_of_two(), "n={n}");
            assert_eq!(Some(next_power_of_two(n)), n.checked_next_power_of_two());
        }
    }

    #[test]
    fn next_power_of_two_overflows_like_std() {
        for n in [(1u32 << 31) + 1, u32::MAX - 1, u32::MAX] {
            let ours = std::panic::catch_unwind(|| next_power_of_two(n));
            let std = std::panic::catch_unwind(|| n.next_power_of_two());
            assert_eq!(ours.is_err(), std.is_err(), "n={n}");
            if let (Ok(ours), Ok(std)) = (ours, std) {
                assert_eq!(ours, std, "n={n}");
            }
        }
    }

    #[test]
    fn history_ring_aot_matches_reference_in_range() {
        for depth in (0..=4096).chain([(1 << 30) - 1, 1 << 30, (1 << 31) - 1]) {
            assert_eq!(
                history_ring_aot(depth),
                depth.saturating_add(1).next_power_of_two(),
                "depth={depth}"
            );
        }
        assert_eq!(history_ring_aot(0), 1);
        assert_eq!(history_ring_aot((1 << 31) - 1), 1 << 31);
    }

    /// Plain re-implementation of `history_depth_with_ta_inputs` (`HashMap`
    /// lookups in the walk, truncating `as u32`), used by the differential
    /// test. Its starting depth is the specification of the base: `max_bars_back`
    /// when some function reads a slot at a dynamic offset, never below one
    /// bar.
    fn history_depth_reference(program: &CsilProgram, values: &HashMap<TaInputTarget, i64>) -> u32 {
        let dynamic = program.functions.iter().any(|function| {
            function
                .code
                .iter()
                .any(|instr| matches!(instr, CsilInstr::ReadUserSlotDynamic { .. }))
        });
        let mut deepest = if dynamic {
            program.metadata.max_bars_back.max(1)
        } else {
            1
        };
        for function in &program.functions {
            for instr in &function.code {
                match instr {
                    CsilInstr::ReadUserSlot { slot, offset } => {
                        let change = if *offset == 0 {
                            None
                        } else {
                            values
                                .get(&TaInputTarget::Change { slot: *slot })
                                .map(|value| *value as u32)
                        };
                        deepest = read_user_slot_depth(deepest, *offset, change);
                    }
                    CsilInstr::WindowMean { slot, length } => {
                        let sma = values
                            .get(&TaInputTarget::Sma { slot: *slot })
                            .map(|value| *value as u32);
                        deepest = window_mean_depth(deepest, *length, sma);
                    }
                    _ => {}
                }
            }
        }
        deepest
    }

    /// Deterministic xorshift64* so the sweep needs no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn small_program() -> CsilProgram {
        let source = "//@version=6\n\
indicator(\"History kernel\", max_bars_back=2)\n\
n = input.int(2)\n\
x = close\n\
plot(x[2])\n\
plot(ta.sma(x, n))\n\
plot(ta.change(x, n))\n";
        let parsed = pinewasm_parse::parse(source);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(
            analysis.diagnostics.is_empty(),
            "{:?}",
            analysis.diagnostics
        );
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lower")
    }

    fn random_value(rng: &mut Rng, in_range: bool) -> i64 {
        if in_range {
            1 + rng.below(crate::MAX_WINDOW_LENGTH as u64) as i64
        } else {
            match rng.below(6) {
                0 => -(rng.below(1 << 40) as i64) - 1,
                1 => u32::MAX as i64 + 1 + rng.below(1 << 33) as i64,
                2 => i64::MAX,
                3 => i64::MIN,
                4 => rng.below(u32::MAX as u64 + 1) as i64,
                _ => 1 + rng.below(crate::MAX_WINDOW_LENGTH as u64) as i64,
            }
        }
    }

    /// The `HashMap` walk vs the pre-resolved slice kernel on the real
    /// method: identical for every override inside `resolve_ta_input`'s
    /// bounds (and for every in-`u32` value); outside `u32` the checked cast
    /// only ever raises the depth, to at least `min(value, u32::MAX)`.
    #[test]
    fn history_depth_slice_kernel_matches_reference() {
        let base = small_program();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..4000 {
            let mut program = base.clone();
            program.metadata.max_bars_back = rng.below(64) as u32;
            let n_slots = 1 + rng.below(6) as u32;
            let n_functions = 1 + rng.below(3) as usize;
            for f in 0..n_functions {
                let len = rng.below(24) as usize;
                let code: Vec<CsilInstr> = (0..len)
                    .map(|_| match rng.below(4) {
                        0 => CsilInstr::ReadUserSlot {
                            slot: rng.below(n_slots as u64) as u32,
                            offset: if rng.below(4) == 0 {
                                0
                            } else {
                                rng.below(600) as u32
                            },
                        },
                        1 => CsilInstr::WindowMean {
                            slot: rng.below(n_slots as u64) as u32,
                            length: rng.below(600) as u32,
                        },
                        2 => CsilInstr::ReadUserSlotDynamic {
                            slot: rng.below(n_slots as u64) as u32,
                        },
                        _ => CsilInstr::Pop,
                    })
                    .collect();
                if f < program.functions.len() {
                    program.functions[f].code = code;
                } else {
                    let mut function = program.functions[0].clone();
                    function.code = code;
                    program.functions.push(function);
                }
            }
            let in_range = round % 2 == 0;
            let mut values = HashMap::new();
            for slot in 0..n_slots {
                if rng.below(2) == 0 {
                    values.insert(
                        TaInputTarget::Change { slot },
                        random_value(&mut rng, in_range),
                    );
                }
                if rng.below(2) == 0 {
                    values.insert(
                        TaInputTarget::Sma { slot },
                        random_value(&mut rng, in_range),
                    );
                }
            }
            let new = program.history_depth_with_ta_inputs(&values);
            let old = history_depth_reference(&program, &values);
            let all_u32 = values.values().all(|v| u32::try_from(*v).is_ok());
            if all_u32 {
                assert_eq!(new, old, "round={round}");
            } else {
                // Conservative: never below the checked reading of any override.
                let checked: HashMap<_, _> = values
                    .iter()
                    .map(|(k, v)| (*k, i64::from(ta_override_u32(*v))))
                    .collect();
                assert_eq!(
                    new,
                    history_depth_reference(&program, &checked),
                    "round={round}"
                );
            }
            assert_eq!(
                program.history_depth(),
                history_depth_reference(&program, &HashMap::new())
            );
        }
    }

    #[test]
    fn history_depth_base_counts_max_bars_back_only_for_dynamic_reads() {
        for mbb in [0, 1, 2, 500, 5000, u32::MAX] {
            assert_eq!(
                history_depth_base(mbb, false),
                MIN_HISTORY_DEPTH,
                "mbb={mbb}"
            );
            assert_eq!(
                history_depth_base(mbb, true),
                mbb.max(MIN_HISTORY_DEPTH),
                "mbb={mbb}"
            );
        }
    }

    /// A program that never reads history keeps a two-cell ring whatever its
    /// `max_bars_back`; one dynamic read restores the declared buffer.
    #[test]
    fn max_bars_back_sizes_the_ring_only_with_a_dynamic_read() {
        let compile = |source: &str| {
            let parsed = pinewasm_parse::parse(source);
            assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
            let analysis = pinewasm_semantics::analyze(&parsed.program);
            assert!(
                analysis.diagnostics.is_empty(),
                "{:?}",
                analysis.diagnostics
            );
            crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lower")
        };
        let plain =
            compile("//@version=6\nindicator(\"P\", max_bars_back=3000)\nx = close * 2\nplot(x)\n");
        assert_eq!(plain.metadata.max_bars_back, 3000);
        assert_eq!(plain.history_depth(), MIN_HISTORY_DEPTH);
        assert_eq!(plain.history_ring(), 2);
        let literal = compile(
            "//@version=6\nindicator(\"L\", max_bars_back=3000)\nx = close * 2\nplot(x[7])\n",
        );
        assert_eq!(literal.history_depth(), 7);
        let dynamic = compile(
            "//@version=6\nindicator(\"D\", max_bars_back=3000)\nx = close * 2\nplot(x[bar_index % 3])\n",
        );
        assert_eq!(dynamic.history_depth(), 3000);
        assert_eq!(dynamic.history_ring(), 4096);
    }

    #[test]
    fn history_depth_scan_empty_and_order() {
        assert_eq!(history_depth_scan(7, &[]), 7);
        let obs = [
            DepthObserver::ReadUserSlot {
                offset: 5,
                change: None,
            },
            DepthObserver::WindowMean {
                length: 10,
                sma: None,
            },
            DepthObserver::ReadUserSlot {
                offset: 0,
                change: None,
            },
        ];
        assert_eq!(history_depth_scan(0, &obs), 9);
        let mut rev = obs;
        rev.reverse();
        assert_eq!(history_depth_scan(0, &rev), 9);
        assert_eq!(
            history_depth_scan(0, &[DepthObserver::DynamicWindow]),
            MAX_DYNAMIC_WINDOW_DEPTH
        );
        assert_eq!(MAX_DYNAMIC_WINDOW_DEPTH, 99_999);
    }
}
