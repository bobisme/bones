use tracing::instrument;
use crate::crdt::*;
use std::hash::Hash;

pub trait Merge {
    fn merge(&mut self, other: Self);
}

impl Merge for Timestamp {
    #[instrument(skip(self))]
    fn merge(&mut self, other: Self) {
        if other > *self {
            *self = other;
        }
    }
}

impl<T> Merge for Lww<T> {
    fn merge(&mut self, other: Self) {
        if other.timestamp > self.timestamp {
            *self = other;
        }
    }
}

impl<T: Eq + Hash + Clone> Merge for GSet<T> {
    fn merge(&mut self, other: Self) {
        for element in other.elements {
            self.elements.insert(element);
        }
    }
}

impl<T: Eq + Hash + Clone> Merge for OrSet<T> {
    fn merge(&mut self, other: Self) {
        // Stub implementation - naive merge
        // Real implementation handles concurrent adds/removes properly
        self.elements.extend(other.elements);
        self.tombstone.extend(other.tombstone);
    }
}

impl Merge for EpochPhase {
    fn merge(&mut self, other: Self) {
        if other.epoch > self.epoch {
            *self = other;
        } else if other.epoch == self.epoch {
            if other.phase > self.phase {
                self.phase = other.phase;
            }
        }
    }
}
