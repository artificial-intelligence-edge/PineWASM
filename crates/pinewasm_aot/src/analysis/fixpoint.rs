//! The round driver of the collection-element fixpoint
//! ([`super::infer_collection_elems`]), generic over the round body so that
//! Charon/Aeneas translate the driver alone (the `AotCfg` extraction).
//!
//! Protocol: run ordinary rounds until one reports no change, or until
//! `max_rounds` rounds have run; then run exactly ONE `checking` round, which
//! evaluates the heap gate on the (post-)fixpoint state. A malformed state
//! stops the driver immediately.

/// How one round ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundEnd {
    /// No lattice cell changed: the state is a fixpoint of the round.
    Stable,
    /// Some cell grew.
    Changed,
    /// The state is malformed (entry stacks of different heights).
    Malformed,
}

/// One pass of a monotone analysis over the whole CFG.
pub(crate) trait FixpointRound {
    fn round(&mut self, checking: bool) -> RoundEnd;
}

/// What the driver did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriverOutcome {
    /// An ordinary round reported [`RoundEnd::Stable`] before the cap.
    pub converged: bool,
    /// Some round reported [`RoundEnd::Malformed`].
    pub malformed: bool,
    /// Ordinary (non-checking) rounds run.
    pub rounds: usize,
}

pub(crate) fn run_fixpoint<R: FixpointRound>(body: &mut R, max_rounds: usize) -> DriverOutcome {
    let mut rounds = 0;
    let mut checking = false;
    let mut converged = true;
    let mut malformed = false;
    let mut running = true;
    while running {
        match body.round(checking) {
            RoundEnd::Malformed => {
                malformed = true;
                running = false;
            }
            RoundEnd::Stable => {
                if checking {
                    running = false;
                } else {
                    rounds += 1;
                    checking = true;
                }
            }
            RoundEnd::Changed => {
                if checking {
                    running = false;
                } else {
                    rounds += 1;
                    if rounds >= max_rounds {
                        converged = false;
                        checking = true;
                    }
                }
            }
        }
    }
    DriverOutcome {
        converged,
        malformed,
        rounds,
    }
}
