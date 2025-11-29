use {
    crate::snapshot_package::{
        are_snapshot_packages_the_same_kind, cmp_snapshot_packages_by_priority, SnapshotPackage,
    },
    agave_snapshots::{SnapshotArchiveKind, SnapshotKind},
    log::*,
    std::cmp::Ordering::Greater,
};

/// Snapshot packages that are pending for archival
#[derive(Debug, Default)]
pub struct PendingSnapshotPackages {
    full: Option<SnapshotPackage>,
    incremental: Option<SnapshotPackage>,
    fastboot: Option<SnapshotPackage>,
}

impl PendingSnapshotPackages {
    /// Adds `snapshot_package` as a pending snapshot package
    ///
    /// This will overwrite currently-pending in-kind packages.
    ///
    /// Note: This function will panic if `snapshot_package` is *older*
    /// than any currently-pending in-kind packages.
    pub fn push(&mut self, snapshot_package: SnapshotPackage) {
        let (pending_package, kind_str) = match snapshot_package.snapshot_kind {
            SnapshotKind::Archive(SnapshotArchiveKind::Full) => (&mut self.full, "full"),
            SnapshotKind::Archive(SnapshotArchiveKind::Incremental(_)) => {
                (&mut self.incremental, "incremental")
            }
            SnapshotKind::Fastboot => (&mut self.fastboot, "fastboot"),
        };

        if let Some(pending_snapshot_package) = pending_package.as_ref() {
            // snapshots are monotonically increasing; only overwrite *old* packages
            assert!(
                are_snapshot_packages_the_same_kind(&snapshot_package, pending_snapshot_package),
                "mismatched snapshot kinds: pending: {pending_snapshot_package:?}, new: \
                 {snapshot_package:?}",
            );
            assert_eq!(
                cmp_snapshot_packages_by_priority(&snapshot_package, pending_snapshot_package),
                Greater,
                "{kind_str} snapshot package must be newer than pending package, old: \
                 {pending_snapshot_package:?}, new: {snapshot_package:?}",
            );
            info!(
                "overwrote pending {kind_str} snapshot package, old slot: {}, new slot: {}",
                pending_snapshot_package.slot, snapshot_package.slot,
            );
        }

        *pending_package = Some(snapshot_package);
    }

    /// Returns the next pending snapshot package to handle
    pub fn pop(&mut self) -> Option<SnapshotPackage> {
        let pending_full = self.full.take();
        let pending_incremental = self.incremental.take();
        let pending_archive_snapshot = match (pending_full, pending_incremental) {
            (Some(pending_full), pending_incremental) => {
                // If there is a pending incremental snapshot package, check its slot.
                // If its slot is greater than the full snapshot package's,
                // re-enqueue it, otherwise drop it.
                // Note that it is *not supported* to handle incremental snapshots with
                // slots *older* than the latest full snapshot.  This is why we do not
                // re-enqueue every incremental snapshot.
                if let Some(pending_incremental) = pending_incremental {
                    let SnapshotKind::Archive(SnapshotArchiveKind::Incremental(base_slot)) =
                        &pending_incremental.snapshot_kind
                    else {
                        panic!(
                            "the pending incremental snapshot package must be of kind \
                             IncrementalSnapshot, but instead was {pending_incremental:?}",
                        );
                    };
                    if pending_incremental.slot > pending_full.slot
                        && *base_slot >= pending_full.slot
                    {
                        self.incremental = Some(pending_incremental);
                    }
                }

                assert!(pending_full.snapshot_kind.is_full_snapshot());
                Some(pending_full)
            }
            (None, Some(pending_incremental)) => {
                assert!(pending_incremental.snapshot_kind.is_incremental_snapshot());
                Some(pending_incremental)
            }
            (None, None) => None,
        };

        let pending_fastboot = self.fastboot.take();

        match (pending_archive_snapshot, pending_fastboot) {
            (Some(archive_snapshot), Some(pending_fastboot)) => {
                // Re-enqueue the fastboot snapshot package if its slot is greater
                if pending_fastboot.slot > archive_snapshot.slot {
                    self.fastboot = Some(pending_fastboot);
                }
                Some(archive_snapshot)
            }
            (Some(archive_snapshot), None) => Some(archive_snapshot),
            (None, Some(fastboot_snapshot)) => {
                assert!(
                    fastboot_snapshot.snapshot_kind == SnapshotKind::Fastboot,
                    "the pending fastboot snapshot package must be of kind Fastboot, but instead \
                     was {fastboot_snapshot:?}",
                );
                Some(fastboot_snapshot)
            }
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_clock::Slot};

    fn new(snapshot_kind: SnapshotKind, slot: Slot) -> SnapshotPackage {
        SnapshotPackage {
            snapshot_kind,
            slot,
            ..SnapshotPackage::default_for_tests()
        }
    }
    fn new_full(slot: Slot) -> SnapshotPackage {
        new(SnapshotKind::Archive(SnapshotArchiveKind::Full), slot)
    }
    fn new_incr(slot: Slot, base: Slot) -> SnapshotPackage {
        new(
            SnapshotKind::Archive(SnapshotArchiveKind::Incremental(base)),
            slot,
        )
    }

    fn new_fastboot(slot: Slot) -> SnapshotPackage {
        new(SnapshotKind::Fastboot, slot)
    }

    #[test]
    fn test_default() {
        let pending_snapshot_packages = PendingSnapshotPackages::default();
        assert!(pending_snapshot_packages.full.is_none());
        assert!(pending_snapshot_packages.incremental.is_none());
    }

    #[test]
    fn test_push() {
        let mut pending_snapshot_packages = PendingSnapshotPackages::default();

        // ensure we can push full snapshot packages
        let slot = 100;
        pending_snapshot_packages.push(new_full(slot));
        assert_eq!(pending_snapshot_packages.full.as_ref().unwrap().slot, slot);
        assert!(pending_snapshot_packages.incremental.is_none());
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure we can overwrite full snapshot packages
        let slot = slot + 100;
        pending_snapshot_packages.push(new_full(slot));
        assert_eq!(pending_snapshot_packages.full.as_ref().unwrap().slot, slot);
        assert!(pending_snapshot_packages.incremental.is_none());
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure we can push incremental packages
        let full_slot = slot;
        let slot = full_slot + 10;
        pending_snapshot_packages.push(new_incr(slot, full_slot));
        assert_eq!(
            pending_snapshot_packages.full.as_ref().unwrap().slot,
            full_slot,
        );
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            slot,
        );
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure we can overwrite incremental packages
        let slot = slot + 10;
        pending_snapshot_packages.push(new_incr(slot, full_slot));
        assert_eq!(
            pending_snapshot_packages.full.as_ref().unwrap().slot,
            full_slot,
        );
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            slot,
        );
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure pushing a full package doesn't affect the incremental package
        // (we already tested above that pushing an incremental doesn't affect the full)
        let incremental_slot = slot;
        let slot = full_slot + 100;
        pending_snapshot_packages.push(new_full(slot));
        assert_eq!(pending_snapshot_packages.full.as_ref().unwrap().slot, slot);
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            incremental_slot,
        );
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure we can overwrite incremental packages with incremental packages
        // with a new full slot
        let full_slot = slot;
        let slot = slot + 10;
        pending_snapshot_packages.push(new_incr(slot, full_slot));
        assert_eq!(
            pending_snapshot_packages.full.as_ref().unwrap().slot,
            full_slot,
        );
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            slot,
        );
        assert!(pending_snapshot_packages.fastboot.is_none());

        // ensure we can push fastboot packages
        let incremental_slot = slot;
        let slot = slot + 50;
        let fastboot_package = SnapshotPackage {
            snapshot_kind: SnapshotKind::Fastboot,
            slot,
            ..SnapshotPackage::default_for_tests()
        };
        pending_snapshot_packages.push(fastboot_package);
        assert_eq!(
            pending_snapshot_packages.fastboot.as_ref().unwrap().slot,
            slot,
        );
        assert_eq!(
            pending_snapshot_packages.full.as_ref().unwrap().slot,
            full_slot,
        );
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            incremental_slot,
        );

        // ensure we can overwrite fastboot packages
        let slot = slot + 10;
        let fastboot_package = SnapshotPackage {
            snapshot_kind: SnapshotKind::Fastboot,
            slot,
            ..SnapshotPackage::default_for_tests()
        };
        pending_snapshot_packages.push(fastboot_package);
        assert_eq!(
            pending_snapshot_packages.fastboot.as_ref().unwrap().slot,
            slot,
        );
        assert_eq!(
            pending_snapshot_packages.full.as_ref().unwrap().slot,
            full_slot,
        );
        assert_eq!(
            pending_snapshot_packages.incremental.as_ref().unwrap().slot,
            incremental_slot,
        );
    }

    #[test]
    #[should_panic(expected = "full snapshot package must be newer than pending package")]
    fn test_push_older_full() {
        let slot = 100;
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: Some(new_full(slot)),
            incremental: None,
            fastboot: None,
        };

        // pushing an older full should panic
        pending_snapshot_packages.push(new_full(slot - 1));
    }

    #[test]
    #[should_panic(expected = "incremental snapshot package must be newer than pending package")]
    fn test_push_older_incremental() {
        let base = 100;
        let slot = base + 20;
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: None,
            incremental: Some(new_incr(slot, base)),
            fastboot: None,
        };

        // pushing an older incremental should panic
        pending_snapshot_packages.push(new_incr(slot - 1, base));
    }

    #[test]
    #[should_panic(expected = "fastboot snapshot package must be newer than pending package")]
    fn test_push_older_fastboot() {
        let slot = 100;
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: None,
            incremental: None,
            fastboot: Some(new_fastboot(slot)),
        };

        // pushing an older incremental should panic
        pending_snapshot_packages.push(new_fastboot(slot - 1));
    }

    #[test]
    fn test_pop() {
        let mut pending_snapshot_packages = PendingSnapshotPackages::default();

        let oldest_slot = 100;
        let middle_slot = 200;
        let newest_slot = 300;

        let slots = vec![
            None,
            Some(oldest_slot),
            Some(middle_slot),
            Some(newest_slot),
        ];
        let mut scenarios = Vec::new();

        // Generate all combinations of full, incremental, and fastboot slots
        for &full in &slots {
            for &incr in &slots {
                for &fastboot in &slots {
                    scenarios.push((full, incr, fastboot));
                }
            }
        }

        for (full, incr, fastboot) in scenarios {
            pending_snapshot_packages.full = full.map(new_full);
            pending_snapshot_packages.incremental = incr.map(|slot| new_incr(slot, slot));
            pending_snapshot_packages.fastboot = fastboot.map(new_fastboot);

            // Pop full snapshot if it exists
            if let Some(full_slot) = full {
                assert_eq!(pending_snapshot_packages.pop().unwrap().slot, full_slot);
            }

            // Pop incremental snapshot if it exists and is newer than the full snapshot
            if let Some(incr_slot) = incr {
                if full.is_none() || incr_slot > full.unwrap() {
                    assert_eq!(pending_snapshot_packages.pop().unwrap().slot, incr_slot);
                }
            }

            // Pop fastboot snapshot if it exists and is newer than both full and incremental
            if let Some(fastboot_slot) = fastboot {
                if fastboot_slot > full.unwrap_or(0) && fastboot_slot > incr.unwrap_or(0) {
                    assert_eq!(pending_snapshot_packages.pop().unwrap().slot, fastboot_slot);
                }
            }

            // Ensure no more pending packages
            assert!(pending_snapshot_packages.pop().is_none());
        }
    }

    #[test]
    fn test_pop_incremental() {
        let mut pending_snapshot_packages = PendingSnapshotPackages::default();

        // ensure pop returns incremental when there's only an incremental
        let base = 100;
        let slot = base + 10;
        pending_snapshot_packages.incremental = Some(new_incr(slot, base));
        let snapshot_package = pending_snapshot_packages.pop().unwrap();
        assert!(snapshot_package.snapshot_kind.is_incremental_snapshot());
        assert_eq!(snapshot_package.slot, slot);
    }

    #[test]
    fn test_pop_fastboot() {
        let mut pending_snapshot_packages = PendingSnapshotPackages::default();

        // ensure pop returns fastboot when there's only a fastboot
        let slot = 100;
        pending_snapshot_packages.fastboot = Some(new_fastboot(slot));
        let snapshot_package = pending_snapshot_packages.pop().unwrap();
        assert!(snapshot_package.snapshot_kind == SnapshotKind::Fastboot);
        assert_eq!(snapshot_package.slot, slot);
    }

    #[test]
    #[should_panic]
    fn test_pop_invalid_pending_full() {
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: Some(new_incr(110, 100)), // <-- invalid! `full` is IncrementalSnapshot
            incremental: None,
            fastboot: None,
        };
        pending_snapshot_packages.pop();
    }

    #[test]
    #[should_panic]
    fn test_pop_invalid_pending_incremental() {
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: None,
            incremental: Some(new_full(100)), // <-- invalid! `incremental` is FullSnapshot
            fastboot: None,
        };
        pending_snapshot_packages.pop();
    }

    #[test]
    #[should_panic]
    fn test_pop_invalid_pending_fastboot() {
        let mut pending_snapshot_packages = PendingSnapshotPackages {
            full: None,
            incremental: None,
            fastboot: Some(new_full(100)), // <-- invalid! `incremental` is FullSnapshot
        };
        pending_snapshot_packages.pop();
    }
}
