use {
    crate::ancestors::Ancestors,
    solana_clock::{BankId, Slot},
    std::{
        collections::{HashSet, btree_map::BTreeMap},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
    },
    thiserror::Error,
};

pub type ScanResult<T> = Result<T, ScanError>;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ScanError {
    #[error(
        "Node detected it replayed bad version of slot {slot:?} with id {bank_id:?}, thus the \
         scan on said slot was aborted"
    )]
    SlotRemoved { slot: Slot, bank_id: BankId },
    #[error("scan aborted: {0}")]
    Aborted(String),
}

#[derive(Debug, Default)]
pub(crate) struct ScanConfig {
    /// checked by the scan. When true, abort scan.
    pub(crate) abort: Option<Arc<AtomicBool>>,
}

impl ScanConfig {
    /// mark the scan as aborted
    pub(crate) fn abort(&self) {
        if let Some(abort) = self.abort.as_ref() {
            abort.store(true, Ordering::Relaxed)
        }
    }

    /// use existing 'abort' if available, otherwise allocate one
    pub(crate) fn recreate_with_abort(&self) -> Self {
        ScanConfig {
            abort: Some(self.abort.clone().unwrap_or_default()),
        }
    }

    /// true if scan should abort
    pub(crate) fn is_aborted(&self) -> bool {
        if let Some(abort) = self.abort.as_ref() {
            abort.load(Ordering::Relaxed)
        } else {
            false
        }
    }
}

/// Runtime state for tracking in-progress account scans.
#[derive(Debug, Default)]
pub struct ScanTracker {
    ongoing_scan_roots: RwLock<BTreeMap<Slot, u64>>,
    // Each scan has some latest slot `S` that is the tip of the fork the scan
    // is iterating over. The unique id of that slot `S` is recorded here (note we don't use
    // `S` as the id because there can be more than one version of a slot `S`). If a fork
    // is abandoned, all of the slots on that fork up to `S` will be removed via
    // `AccountsDb::remove_unrooted_slots()`. When the scan finishes, it'll realize that the
    // results of the scan may have been corrupted by `remove_unrooted_slots` and abort its results.
    //
    // `removed_bank_ids` tracks all the slot ids that were removed via `remove_unrooted_slots()` so any attempted scans
    // on any of these slots fails. This is safe to purge once the associated Bank is dropped and
    // scanning the fork with that Bank at the tip is no longer possible.
    pub removed_bank_ids: Mutex<HashSet<BankId>>,
    /// # scans active currently
    pub active_scans: AtomicUsize,
    /// # of slots between latest max and latest scan
    pub max_distance_to_min_scan_slot: AtomicU64,
}

impl ScanTracker {
    fn min_ongoing_scan_root_from_btree(ongoing_scan_roots: &BTreeMap<Slot, u64>) -> Option<Slot> {
        ongoing_scan_roots.keys().next().cloned()
    }

    pub fn min_ongoing_scan_root(&self) -> Option<Slot> {
        Self::min_ongoing_scan_root_from_btree(&self.ongoing_scan_roots.read().unwrap())
    }
}

/// Guard that protects account state during an accounts scan.
///
/// Pins `max_root` in `ongoing_scan_roots` on creation and unpins it on drop,
/// preventing clean from advancing past the pinned root while the scan is active.
#[derive(Debug)]
pub(crate) struct ScanGuard<'a> {
    scan_tracker: &'a ScanTracker,
    max_root: Slot,
    scan_bank_id: BankId,
}

impl<'a> ScanGuard<'a> {
    /// Begin a scan: checks the bank hasn't been removed and pins `max_root` in
    /// `ongoing_scan_roots`.
    ///
    /// Returns `None` if the bank has already been removed.
    ///
    /// `max_root_inclusive_fn` is called while holding the `ongoing_scan_roots` write lock.
    pub(crate) fn try_new(
        scan_tracker: &'a ScanTracker,
        scan_bank_id: BankId,
        max_root_inclusive_fn: impl FnOnce() -> Slot,
    ) -> Option<Self> {
        {
            let locked_removed_bank_ids = scan_tracker.removed_bank_ids.lock().unwrap();
            if locked_removed_bank_ids.contains(&scan_bank_id) {
                return None;
            }
        }

        let max_root_inclusive = {
            let mut w_ongoing_scan_roots = scan_tracker
                // This lock is also grabbed by clean_accounts(), so clean
                // has at most cleaned up to the current `max_root` (since
                // clean only happens *after* BankForks::set_root() which sets
                // the `max_root`)
                .ongoing_scan_roots
                .write()
                .unwrap();
            // `max_root()` grabs a lock while
            // the `ongoing_scan_roots` lock is held,
            // make sure inverse doesn't happen to avoid
            // deadlock
            let max_root_inclusive = max_root_inclusive_fn();
            if let Some(min_ongoing_scan_root) =
                ScanTracker::min_ongoing_scan_root_from_btree(&w_ongoing_scan_roots)
            {
                if min_ongoing_scan_root < max_root_inclusive {
                    let current = max_root_inclusive - min_ongoing_scan_root;
                    scan_tracker
                        .max_distance_to_min_scan_slot
                        .fetch_max(current, Ordering::Relaxed);
                }
            }
            *w_ongoing_scan_roots.entry(max_root_inclusive).or_default() += 1;
            max_root_inclusive
        };

        scan_tracker.active_scans.fetch_add(1, Ordering::Relaxed);
        Some(Self {
            scan_tracker,
            max_root: max_root_inclusive,
            scan_bank_id,
        })
    }

    /// The inclusive max root pinned by this scan guard.
    pub(crate) fn max_root(&self) -> Slot {
        self.max_root
    }

    /// Returns true if ancestors should be used, or false if the scan's bank
    /// is not descended from `max_root` (different fork or ancestor of
    /// `max_root`).
    ///
    /// For any bank `B` descended from the current `max_root`, it must be true
    /// that `B.ancestors.contains(max_root)`, regardless of squash behavior.
    /// (Proof: at startup max_root is the greatest root from the snapshot, and
    /// on each `set_root(R_new)` where `R_new > R`, every surviving descendant
    /// of `R_new` was also a descendant of `R` and therefore has `R_new` in its
    /// ancestors.)
    ///
    /// If `max_root` is **not** in `ancestors`, the bank is either:
    /// 1. on a different fork, or
    /// 2. an ancestor of `max_root`.
    ///
    /// In both cases the provided ancestors may reference slots that have
    /// already been cleaned, so we fall back to an empty ancestor set and rely
    /// only on roots (bounded by `max_root`).
    ///
    /// ```text
    ///             slot 0
    ///               |
    ///             slot 1
    ///           /        \
    ///      slot 2         |
    ///         |       slot 3 (max root)
    ///     slot 4 (scan)
    /// ```
    ///
    /// By the time the scan on slot 4 is called, slot 2 may already have been
    /// cleaned by a clean on slot 3, but slot 4 may not have been cleaned.
    /// The state in slot 2 would have been purged and is not saved in any roots.
    /// In this case, a scan on slot 4 wouldn't accurately reflect the state
    /// when bank 4 was frozen, so we default to a scan on the latest roots by
    /// removing all `ancestors`.
    ///
    /// After calling this, there are two cases:
    ///
    /// 1) **Ancestors is empty** (this method returned `false`): the scan behaves
    ///    like a scan on a rooted bank. `ongoing_scan_roots` protects the roots
    ///    needed by the scan, and passing `max_root` to the scan ensures newer
    ///    roots don't appear in the results.
    ///
    /// 2) **Ancestors is non-empty** (this method returned `true`): the fork
    ///    structure must look something like:
    ///
    /// ```text
    ///            slot 0
    ///              |
    ///        slot 1 (max_root)
    ///        /            \
    ///   slot 2              |
    ///      |            slot 3 (potential newer max root)
    ///    slot 4
    ///      |
    ///   slot 5 (scan)
    /// ```
    ///
    ///    Consider both types of ancestors, `ancestor <= max_root` and
    ///    `ancestor > max_root`, where `max_root == 1` as illustrated above.
    ///
    ///    a) The set of `ancestors <= max_root` are all rooted, which means their
    ///       state is protected by the same guarantees as case 1.
    ///
    ///    b) The `ancestors > max_root` have at least one reference discoverable
    ///       through the chain of `Bank::BankRc::parent` starting from the calling
    ///       bank. For instance bank 5's parent reference keeps bank 4 alive, which
    ///       prevents `Bank::drop()` from running and cleaning up bank 4.
    ///       Furthermore, no cleans can happen past the saved `max_root == 1`, so a
    ///       potential newer max root at slot 3 will not clean up any of the
    ///       ancestors > 1, and slot 4 will not be cleaned in the middle of the
    ///       scan either. (NOTE: similar reasoning is employed for the `assert!()`
    ///       justification in `AccountsDb::retry_to_get_account_accessor`.)
    pub(crate) fn should_use_ancestors(&self, ancestors: &Ancestors) -> bool {
        ancestors.contains_key(&self.max_root)
    }

    /// Finalize the scan: returns whether the bank was removed during the scan.
    /// The `Drop` impl handles unpinning regardless of whether this is called.
    pub(crate) fn was_scan_corrupted(self) -> bool {
        self.scan_tracker
            .removed_bank_ids
            .lock()
            .unwrap()
            .contains(&self.scan_bank_id)
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.scan_tracker
            .active_scans
            .fetch_sub(1, Ordering::Relaxed);
        let mut ongoing_scan_roots = self.scan_tracker.ongoing_scan_roots.write().unwrap();
        let count = ongoing_scan_roots.get_mut(&self.max_root).unwrap();
        *count -= 1;
        if *count == 0 {
            ongoing_scan_roots.remove(&self.max_root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_config() {
        let config = ScanConfig::default();
        assert!(config.abort.is_none()); // not allocated
        assert!(!config.is_aborted());
        config.abort(); // has no effect
        assert!(!config.is_aborted());

        let config = config.recreate_with_abort();
        assert!(config.abort.is_some());
        assert!(!config.is_aborted());
        config.abort();
        assert!(config.is_aborted());

        let config = config.recreate_with_abort();
        assert!(config.is_aborted());
    }

    #[test]
    fn test_scan_guard_pins_and_unpins_root() {
        let tracker = ScanTracker::default();

        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 0);
        assert!(tracker.min_ongoing_scan_root().is_none());

        let guard = ScanGuard::try_new(&tracker, 0, || 42).unwrap();
        assert_eq!(guard.max_root(), 42);
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 1);
        assert_eq!(tracker.min_ongoing_scan_root(), Some(42));

        assert!(!guard.was_scan_corrupted());
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 0);
        assert!(tracker.min_ongoing_scan_root().is_none());
    }

    #[test]
    fn test_scan_guard_refcounts_same_root() {
        let tracker = ScanTracker::default();

        let guard1 = ScanGuard::try_new(&tracker, 0, || 10).unwrap();
        let guard2 = ScanGuard::try_new(&tracker, 0, || 10).unwrap();
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 2);
        assert_eq!(
            *tracker.ongoing_scan_roots.read().unwrap().get(&10).unwrap(),
            2
        );

        drop(guard1);
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 1);
        assert_eq!(tracker.min_ongoing_scan_root(), Some(10));

        drop(guard2);
        assert!(tracker.min_ongoing_scan_root().is_none());
    }

    #[test]
    fn test_scan_guard_multiple_different_roots() {
        let tracker = ScanTracker::default();

        let guard1 = ScanGuard::try_new(&tracker, 0, || 5).unwrap();
        let guard2 = ScanGuard::try_new(&tracker, 0, || 15).unwrap();
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 2);
        assert_eq!(tracker.min_ongoing_scan_root(), Some(5));

        drop(guard1);
        assert_eq!(tracker.min_ongoing_scan_root(), Some(15));

        drop(guard2);
        assert!(tracker.min_ongoing_scan_root().is_none());
    }

    #[test]
    fn test_scan_guard_rejected_for_removed_bank() {
        let tracker = ScanTracker::default();
        tracker.removed_bank_ids.lock().unwrap().insert(7);

        let result = ScanGuard::try_new(&tracker, 7, || 100);
        assert!(result.is_none());
        // should not have incremented active_scans
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_scan_guard_corrupted_when_bank_removed_during_scan() {
        let tracker = ScanTracker::default();
        let guard = ScanGuard::try_new(&tracker, 5, || 50).unwrap();

        // simulate bank removal mid-scan
        tracker.removed_bank_ids.lock().unwrap().insert(5);

        assert!(guard.was_scan_corrupted());
        // guard should still have cleaned up
        assert_eq!(tracker.active_scans.load(Ordering::Relaxed), 0);
        assert!(tracker.min_ongoing_scan_root().is_none());
    }

    #[test]
    fn test_scan_guard_should_use_ancestors_when_max_root_in_ancestors() {
        let tracker = ScanTracker::default();
        let mut ancestors = Ancestors::default();
        ancestors.insert(42);

        let guard = ScanGuard::try_new(&tracker, 0, || 42).unwrap();
        assert!(guard.should_use_ancestors(&ancestors));
    }

    #[test]
    fn test_scan_guard_skip_ancestors_when_max_root_not_in_ancestors() {
        let tracker = ScanTracker::default();
        let mut ancestors = Ancestors::default();
        ancestors.insert(10);

        // max_root_inclusive = 42, which is NOT in ancestors
        let guard = ScanGuard::try_new(&tracker, 0, || 42).unwrap();
        assert!(!guard.should_use_ancestors(&ancestors));
    }
}
