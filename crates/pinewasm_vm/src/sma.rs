//! Bounded observation history for slot-based SMA instructions.
//!
//! Bar history and SMA history have different clocks: an `na` source consumes
//! a bar but no SMA observation. Keep the latter separately so a long gap
//! cannot evict a valid value from the ordinary user-slot ring.

use crate::vm_window::{self, MeanCell, MeanOut, MeanView};
use pinewasm_csil::{CsilInstr, CsilProgram, MAX_WINDOW_LENGTH};
use pinewasm_hir::TaInputTarget;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct States(Vec<Option<State>>);

impl Clone for States {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }

    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

struct State {
    cells: Vec<f64>,
    observations: usize,
    bar_start_observations: usize,
    bar: Option<usize>,
    overwritten: f64,
}

impl Clone for State {
    fn clone(&self) -> Self {
        Self {
            cells: self.cells.clone(),
            observations: self.observations,
            bar_start_observations: self.bar_start_observations,
            bar: self.bar,
            overwritten: self.overwritten,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        // A realtime restore reuses the admitted ring rather than briefly
        // allocating a third full-capacity copy beside the snapshot.
        self.cells.clone_from(&source.cells);
        self.observations = source.observations;
        self.bar_start_observations = source.bar_start_observations;
        self.bar = source.bar;
        self.overwritten = source.overwritten;
    }
}

impl MeanView<f64> for f64 {
    fn view(&self) -> MeanCell<f64> {
        MeanCell::Num(*self)
    }
}

fn capacities(program: &CsilProgram, values: &HashMap<TaInputTarget, i64>) -> Vec<usize> {
    let mut capacities = vec![0; program.series_slots.len()];
    for instruction in program.functions.iter().flat_map(|function| &function.code) {
        let (slot, length) = match instruction {
            CsilInstr::WindowMean { slot, length } => (
                *slot,
                values
                    .get(&TaInputTarget::Sma { slot: *slot })
                    .map_or(*length, |value| *value as u32),
            ),
            CsilInstr::WindowMeanDynamic { slot } => (*slot, MAX_WINDOW_LENGTH),
            _ => continue,
        };
        let capacity = (length as usize).next_power_of_two();
        capacities[slot as usize] = capacities[slot as usize].max(capacity);
    }
    if capacities.iter().all(|capacity| *capacity == 0) {
        capacities.clear();
    }
    capacities
}

impl States {
    pub(crate) fn new(program: &CsilProgram) -> Self {
        Self(
            capacities(program, &HashMap::new())
                .into_iter()
                .map(|capacity| {
                    (capacity != 0).then(|| State {
                        cells: vec![0.0; capacity],
                        observations: 0,
                        bar_start_observations: 0,
                        bar: None,
                        overwritten: 0.0,
                    })
                })
                .collect(),
        )
    }

    /// Includes the active state and its realtime rollback copy. All cell
    /// storage is admitted before executing any source instruction.
    pub(crate) fn allocation_bytes(
        program: &CsilProgram,
        values: &HashMap<TaInputTarget, i64>,
    ) -> Option<usize> {
        let capacities = capacities(program, values);
        let bytes = capacities
            .len()
            .checked_mul(std::mem::size_of::<Option<State>>())?;
        capacities
            .into_iter()
            .try_fold(bytes, |sum, capacity| {
                sum.checked_add(capacity.checked_mul(std::mem::size_of::<f64>())?)
            })?
            .checked_mul(2)
    }

    /// One bar contributes at most one observation. Re-evaluating a call
    /// site on that bar replaces its current observation; it does not turn
    /// a loop or a realtime recalculation into extra historical bars.
    pub(crate) fn mean(
        &mut self,
        slot: u32,
        bar: usize,
        source: Option<f64>,
        length: u32,
    ) -> Option<f64> {
        let state = self.0.get_mut(slot as usize)?.as_mut()?;
        let mask = state.cells.len() - 1;
        if state.bar == Some(bar) {
            if state.observations != state.bar_start_observations {
                state.cells[state.bar_start_observations & mask] = state.overwritten;
            }
            state.observations = state.bar_start_observations;
        } else {
            state.bar = Some(bar);
            state.bar_start_observations = state.observations;
        }
        if let Some(source) = source {
            let index = state.observations & mask;
            state.overwritten = state.cells[index];
            state.cells[index] = source;
            state.observations += 1;
        }
        let observation = state.observations.checked_sub(1)?;
        // The existing chronological arithmetic kernel now sees a dense
        // observation ring. Its implementation and extracted model remain
        // unchanged; only the clock and the storage supplied to it differ.
        match vm_window::window_mean(&state.cells, observation, length, mask) {
            MeanOut::Mean(mean) => Some(mean),
            MeanOut::Na => None,
            MeanOut::Mismatch(index) => {
                unreachable!("numeric SMA observation ring has a non-numeric cell at {index}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{State, States};

    fn states(capacity: usize) -> States {
        States(vec![Some(State {
            cells: vec![0.0; capacity],
            observations: 0,
            bar_start_observations: 0,
            bar: None,
            overwritten: 0.0,
        })])
    }

    #[test]
    fn replacing_a_full_ring_observation_with_na_restores_its_oldest_cell() {
        let mut states = states(2);
        assert_eq!(states.mean(0, 0, Some(10.0), 2), None);
        assert_eq!(states.mean(0, 1, Some(20.0), 2), Some(15.0));
        assert_eq!(states.mean(0, 2, Some(30.0), 2), Some(25.0));
        assert_eq!(states.mean(0, 2, None, 2), Some(15.0));
        assert_eq!(states.mean(0, 2, Some(40.0), 2), Some(30.0));
        assert_eq!(states.mean(0, 3, Some(50.0), 2), Some(45.0));
    }

    #[test]
    fn rollback_restores_observations_after_a_long_missing_run() {
        let mut states = states(2);
        assert_eq!(states.mean(0, 0, Some(10.0), 2), None);
        for bar in 1..10 {
            assert_eq!(states.mean(0, bar, None, 2), None);
        }
        assert_eq!(states.mean(0, 10, Some(20.0), 2), Some(15.0));
        let snapshot = states.clone();
        let cells = states.0[0].as_ref().unwrap().cells.as_ptr();
        assert_eq!(states.mean(0, 11, Some(100.0), 2), Some(60.0));
        states.clone_from(&snapshot);
        assert_eq!(states.0[0].as_ref().unwrap().cells.as_ptr(), cells);
        assert_eq!(states.mean(0, 11, None, 2), Some(15.0));
        states.clone_from(&snapshot);
        assert_eq!(states.mean(0, 11, Some(40.0), 2), Some(30.0));
    }
}
