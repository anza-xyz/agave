use {
    solana_clock::Slot,
    std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Instant,
    },
};

/// Shared set of slots whose replay votor cancelled, because they received a Skip certificate
/// without any NotarizeFallback-or-stronger certificate.
///
/// Writer (votor) inserts and removes slots; reader (replay) discards the banks of cancelled
/// slots and doesn't recreate them while they stay cancelled. Each slot maps to the time its
/// replay was first cancelled.
#[derive(Clone, Debug, Default)]
pub struct CancelledReplaySlots(Arc<Mutex<BTreeMap<Slot, Instant>>>);

impl CancelledReplaySlots {
    /// Cancels replay of `slot`. Keeps the original cancellation time if `slot` is already
    /// cancelled.
    pub fn insert(&self, slot: Slot) {
        self.0
            .lock()
            .unwrap()
            .entry(slot)
            .or_insert_with(Instant::now);
    }

    /// Resumes replay of `slot`. Returns whether `slot` was cancelled.
    pub fn remove(&self, slot: Slot) -> bool {
        self.0.lock().unwrap().remove(&slot).is_some()
    }

    /// Returns whether replay of `slot` is cancelled.
    pub fn contains(&self, slot: Slot) -> bool {
        self.0.lock().unwrap().contains_key(&slot)
    }

    /// Returns when replay of `slot` was first cancelled, if it is currently cancelled.
    pub fn cancelled_at(&self, slot: Slot) -> Option<Instant> {
        self.0.lock().unwrap().get(&slot).copied()
    }

    /// Forgets every cancelled slot at or below `root`.
    pub fn prune_through(&self, root: Slot) {
        let mut cancelled_slots = self.0.lock().unwrap();
        *cancelled_slots = cancelled_slots.split_off(&root.saturating_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_replay_slots_insert_remove_and_prune() {
        let cancelled_replay_slots = CancelledReplaySlots::default();
        let shared = cancelled_replay_slots.clone();
        assert!(!shared.contains(5));
        assert_eq!(shared.cancelled_at(5), None);

        cancelled_replay_slots.insert(5);
        let first_cancelled_at = shared.cancelled_at(5).unwrap();
        assert!(shared.contains(5));
        // Cancelling again keeps the first cancellation time.
        cancelled_replay_slots.insert(5);
        assert_eq!(shared.cancelled_at(5), Some(first_cancelled_at));

        assert!(cancelled_replay_slots.remove(5));
        assert!(!shared.contains(5));
        assert!(!cancelled_replay_slots.remove(5));

        for slot in [3, 4, 6] {
            cancelled_replay_slots.insert(slot);
        }
        cancelled_replay_slots.prune_through(4);
        assert!(!shared.contains(3));
        assert!(!shared.contains(4));
        assert!(shared.contains(6));
    }
}
