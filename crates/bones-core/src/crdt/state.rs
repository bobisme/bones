//! Epoch+Phase state CRDT for work item lifecycle.
//!
//! The lifecycle state (Open/Doing/Done/Archived) uses a non-standard
//! (epoch, phase) CRDT that handles concurrent close/reopen correctly:
//! if agent A closes an item while agent B reopens it, the higher epoch wins.
//!
//! # Phase Ranking
//!
//! Phases have a total ordering:
//!   Open(0) < Doing(1) < Done(2) < Archived(3)
//!
//! # Merge Rules
//!
//! Given two `EpochPhaseState` values `a` and `b`:
//!   1. If `a.epoch != b.epoch`: the one with higher epoch wins entirely.
//!   2. If `a.epoch == b.epoch`: the one with higher phase rank wins.
//!
//! This satisfies the semilattice laws (commutative, associative, idempotent).
//!
//! # Reopen Semantics
//!
//! To reopen an item, increment the epoch and set phase to Open.
//! This guarantees the reopen wins against any concurrent operations
//! in the previous epoch.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Phase enum
// ---------------------------------------------------------------------------

/// Work item lifecycle phase with total ordering by rank.
///
/// The discriminant values define the rank for merge comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Phase {
    Open = 0,
    Doing = 1,
    Done = 2,
    Archived = 3,
}

impl Phase {
    /// Return the numeric rank of this phase.
    #[must_use]
    pub const fn rank(self) -> u8 {
        self as u8
    }

    /// Return the phase name as a string slice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }

    /// All phases in rank order.
    pub const ALL: [Phase; 4] = [Self::Open, Self::Doing, Self::Done, Self::Archived];
}

impl PartialOrd for Phase {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Phase {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Phase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "doing" => Ok(Self::Doing),
            "done" => Ok(Self::Done),
            "archived" => Ok(Self::Archived),
            _ => Err(format!("unknown phase: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// EpochPhaseState
// ---------------------------------------------------------------------------

/// CRDT state combining an epoch counter with a lifecycle phase.
///
/// The epoch increases on reopen operations, ensuring that reopens
/// always win against concurrent operations in earlier epochs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EpochPhaseState {
    /// Monotonically increasing epoch counter. Incremented on reopen.
    pub epoch: u64,
    /// Current lifecycle phase within this epoch.
    pub phase: Phase,
}

impl EpochPhaseState {
    /// Create a new state at epoch 0, phase Open.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: 0,
            phase: Phase::Open,
        }
    }

    /// Create a state with specific epoch and phase.
    #[must_use]
    pub fn with(epoch: u64, phase: Phase) -> Self {
        Self { epoch, phase }
    }

    /// Advance to a new phase within the current epoch.
    ///
    /// Returns `Err` if the target phase has a lower rank than the current
    /// phase (within the same epoch, phases only move forward).
    pub fn advance(&mut self, target: Phase) -> Result<(), StateError> {
        if target <= self.phase {
            return Err(StateError::InvalidTransition {
                from: self.phase,
                to: target,
                epoch: self.epoch,
            });
        }
        self.phase = target;
        Ok(())
    }

    /// Reopen the item: increment epoch and set phase to Open.
    ///
    /// This is valid from any phase. The epoch increment ensures
    /// this reopen wins against concurrent operations in the old epoch.
    pub fn reopen(&mut self) {
        self.epoch += 1;
        self.phase = Phase::Open;
    }

    /// Merge two states, producing the semilattice join (least upper bound).
    ///
    /// Rules:
    /// 1. Higher epoch wins entirely.
    /// 2. Same epoch: higher phase rank wins.
    pub fn merge(&mut self, other: &Self) {
        match self.epoch.cmp(&other.epoch) {
            std::cmp::Ordering::Less => {
                // Other has higher epoch — take it entirely
                self.epoch = other.epoch;
                self.phase = other.phase;
            }
            std::cmp::Ordering::Equal => {
                // Same epoch — take higher phase
                if other.phase > self.phase {
                    self.phase = other.phase;
                }
            }
            std::cmp::Ordering::Greater => {
                // We have higher epoch — keep ours
            }
        }
    }
}

impl Default for EpochPhaseState {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EpochPhaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch={} phase={}", self.epoch, self.phase)
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error for invalid state transitions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("invalid transition from {from} to {to} in epoch {epoch}")]
    InvalidTransition {
        from: Phase,
        to: Phase,
        epoch: u64,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // === Phase ordering ===

    #[test]
    fn phase_ranking() {
        assert!(Phase::Open < Phase::Doing);
        assert!(Phase::Doing < Phase::Done);
        assert!(Phase::Done < Phase::Archived);
    }

    #[test]
    fn phase_rank_values() {
        assert_eq!(Phase::Open.rank(), 0);
        assert_eq!(Phase::Doing.rank(), 1);
        assert_eq!(Phase::Done.rank(), 2);
        assert_eq!(Phase::Archived.rank(), 3);
    }

    #[test]
    fn phase_display_and_parse() {
        for phase in Phase::ALL {
            let s = phase.to_string();
            let parsed: Phase = s.parse().unwrap();
            assert_eq!(phase, parsed);
        }
    }

    // === EpochPhaseState basics ===

    #[test]
    fn new_state_is_epoch_0_open() {
        let s = EpochPhaseState::new();
        assert_eq!(s.epoch, 0);
        assert_eq!(s.phase, Phase::Open);
    }

    #[test]
    fn advance_forward() {
        let mut s = EpochPhaseState::new();
        s.advance(Phase::Doing).unwrap();
        assert_eq!(s.phase, Phase::Doing);
        s.advance(Phase::Done).unwrap();
        assert_eq!(s.phase, Phase::Done);
        s.advance(Phase::Archived).unwrap();
        assert_eq!(s.phase, Phase::Archived);
    }

    #[test]
    fn advance_backward_fails() {
        let mut s = EpochPhaseState::with(0, Phase::Done);
        let err = s.advance(Phase::Doing).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
    }

    #[test]
    fn advance_same_phase_fails() {
        let mut s = EpochPhaseState::with(0, Phase::Doing);
        let err = s.advance(Phase::Doing).unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
    }

    // === Reopen ===

    #[test]
    fn reopen_increments_epoch() {
        let mut s = EpochPhaseState::with(0, Phase::Done);
        s.reopen();
        assert_eq!(s.epoch, 1);
        assert_eq!(s.phase, Phase::Open);
    }

    #[test]
    fn reopen_from_archived() {
        let mut s = EpochPhaseState::with(0, Phase::Archived);
        s.reopen();
        assert_eq!(s.epoch, 1);
        assert_eq!(s.phase, Phase::Open);
    }

    #[test]
    fn multiple_reopens_monotonic_epochs() {
        let mut s = EpochPhaseState::new();
        for expected_epoch in 1..=5 {
            s.advance(Phase::Done).unwrap_or(()); // may fail if already past Done
            s.reopen();
            assert_eq!(s.epoch, expected_epoch);
            assert_eq!(s.phase, Phase::Open);
        }
    }

    // === Merge: same epoch ===

    #[test]
    fn merge_same_epoch_higher_phase_wins() {
        let mut a = EpochPhaseState::with(0, Phase::Open);
        let b = EpochPhaseState::with(0, Phase::Done);
        a.merge(&b);
        assert_eq!(a.phase, Phase::Done);
        assert_eq!(a.epoch, 0);
    }

    #[test]
    fn merge_same_epoch_lower_phase_no_change() {
        let mut a = EpochPhaseState::with(0, Phase::Done);
        let b = EpochPhaseState::with(0, Phase::Open);
        a.merge(&b);
        assert_eq!(a.phase, Phase::Done);
    }

    #[test]
    fn merge_same_epoch_same_phase_idempotent() {
        let mut a = EpochPhaseState::with(0, Phase::Doing);
        let b = EpochPhaseState::with(0, Phase::Doing);
        a.merge(&b);
        assert_eq!(a.phase, Phase::Doing);
        assert_eq!(a.epoch, 0);
    }

    // === Merge: different epochs ===

    #[test]
    fn merge_higher_epoch_wins() {
        let mut a = EpochPhaseState::with(1, Phase::Open);
        let b = EpochPhaseState::with(2, Phase::Doing);
        a.merge(&b);
        assert_eq!(a.epoch, 2);
        assert_eq!(a.phase, Phase::Doing);
    }

    #[test]
    fn merge_lower_epoch_no_change() {
        let mut a = EpochPhaseState::with(3, Phase::Doing);
        let b = EpochPhaseState::with(1, Phase::Archived);
        a.merge(&b);
        assert_eq!(a.epoch, 3);
        assert_eq!(a.phase, Phase::Doing);
    }

    // === Concurrent close/reopen ===

    #[test]
    fn concurrent_close_and_reopen_reopen_wins() {
        // Agent A closes: epoch 0, Done
        let close = EpochPhaseState::with(0, Phase::Done);
        // Agent B reopens: epoch 1, Open
        let reopen = EpochPhaseState::with(1, Phase::Open);

        // Merge order 1: close into reopen
        let mut m1 = close.clone();
        m1.merge(&reopen);
        assert_eq!(m1.epoch, 1);
        assert_eq!(m1.phase, Phase::Open);

        // Merge order 2: reopen into close
        let mut m2 = reopen.clone();
        m2.merge(&close);
        assert_eq!(m2.epoch, 1);
        assert_eq!(m2.phase, Phase::Open);

        // Both orderings produce same result
        assert_eq!(m1, m2);
    }

    // === Semilattice properties ===

    #[test]
    fn semilattice_commutative() {
        let cases = vec![
            (EpochPhaseState::with(0, Phase::Open), EpochPhaseState::with(0, Phase::Done)),
            (EpochPhaseState::with(1, Phase::Doing), EpochPhaseState::with(0, Phase::Archived)),
            (EpochPhaseState::with(2, Phase::Open), EpochPhaseState::with(2, Phase::Doing)),
        ];
        for (a, b) in cases {
            let mut ab = a.clone();
            ab.merge(&b);
            let mut ba = b.clone();
            ba.merge(&a);
            assert_eq!(ab, ba, "commutative failed for {a:?} and {b:?}");
        }
    }

    #[test]
    fn semilattice_associative() {
        let a = EpochPhaseState::with(1, Phase::Open);
        let b = EpochPhaseState::with(0, Phase::Done);
        let c = EpochPhaseState::with(1, Phase::Doing);

        // (a merge b) merge c
        let mut left = a.clone();
        left.merge(&b);
        left.merge(&c);

        // a merge (b merge c)
        let mut bc = b.clone();
        bc.merge(&c);
        let mut right = a.clone();
        right.merge(&bc);

        assert_eq!(left, right);
    }

    #[test]
    fn semilattice_idempotent() {
        let a = EpochPhaseState::with(2, Phase::Doing);
        let mut m = a.clone();
        m.merge(&a);
        assert_eq!(m, a);
    }

    // === Edge cases ===

    #[test]
    fn merge_default_with_default() {
        let mut a = EpochPhaseState::default();
        let b = EpochPhaseState::default();
        a.merge(&b);
        assert_eq!(a, EpochPhaseState::new());
    }

    #[test]
    fn display() {
        let s = EpochPhaseState::with(3, Phase::Done);
        assert_eq!(s.to_string(), "epoch=3 phase=done");
    }

    #[test]
    fn serde_roundtrip() {
        let s = EpochPhaseState::with(5, Phase::Archived);
        let json = serde_json::to_string(&s).unwrap();
        let deserialized: EpochPhaseState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, deserialized);
    }

    #[test]
    fn phase_serde_roundtrip() {
        for phase in Phase::ALL {
            let json = serde_json::to_string(&phase).unwrap();
            let deserialized: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(phase, deserialized);
        }
    }
}
