use {
    crate::{
        accounts_background_service::{
            SnapshotRequest, SnapshotRequestKind, SnapshotRequestSender,
        },
        bank::{Bank, SquashTiming},
    },
    agave_snapshots::{snapshot_config::SnapshotConfig, SnapshotInterval},
    log::*,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        time::Instant,
    },
};

struct SnapshotGenerationIntervals {
    full_snapshot_interval: SnapshotInterval,
    incremental_snapshot_interval: SnapshotInterval,
}

pub struct SnapshotController {
    abs_request_sender: SnapshotRequestSender,
    snapshot_config: SnapshotConfig,
    latest_abs_request_slot: AtomicU64,
    request_fastboot_snapshot: AtomicBool,
    latest_bank_snapshot_slot: AtomicU64,
}

impl SnapshotController {
    pub fn new(
        abs_request_sender: SnapshotRequestSender,
        snapshot_config: SnapshotConfig,
        root_slot: Slot,
    ) -> Self {
        Self {
            abs_request_sender,
            snapshot_config,
            latest_abs_request_slot: AtomicU64::new(root_slot),
            request_fastboot_snapshot: AtomicBool::new(false),
            latest_bank_snapshot_slot: AtomicU64::new(root_slot),
        }
    }

    pub fn snapshot_config(&self) -> &SnapshotConfig {
        &self.snapshot_config
    }

    pub fn request_sender(&self) -> &SnapshotRequestSender {
        &self.abs_request_sender
    }

    fn latest_abs_request_slot(&self) -> Slot {
        self.latest_abs_request_slot.load(Ordering::Relaxed)
    }

    fn set_latest_abs_request_slot(&self, slot: Slot) {
        self.latest_abs_request_slot.store(slot, Ordering::Relaxed);
    }

    /// Request that a fastboot snapshot is requested at the root bank next time
    /// handle_new_roots() is called
    pub fn request_fastboot_snapshot(&self) {
        self.request_fastboot_snapshot
            .store(true, Ordering::Relaxed);
    }

    pub fn latest_bank_snapshot_slot(&self) -> Slot {
        self.latest_bank_snapshot_slot.load(Ordering::Relaxed)
    }

    pub fn set_latest_bank_snapshot_slot(&self, slot: Slot) {
        self.latest_bank_snapshot_slot
            .store(slot, Ordering::Relaxed);
    }

    pub fn handle_new_roots(&self, root: Slot, banks: &[&Arc<Bank>]) -> (bool, SquashTiming, u64) {
        let mut is_root_bank_squashed = false;
        let mut squash_timing = SquashTiming::default();
        let mut total_snapshot_ms = 0;
        let request_fastboot_snapshot = self
            .request_fastboot_snapshot
            .swap(false, Ordering::Relaxed);

        let SnapshotGenerationIntervals {
            full_snapshot_interval,
            incremental_snapshot_interval,
        } = self.snapshot_generation_intervals();

        if let Some((bank, request_kind)) = banks.iter().find_map(|bank| {
            let should_request_full_snapshot =
                if let SnapshotInterval::Slots(snapshot_interval) = full_snapshot_interval {
                    bank.block_height() % snapshot_interval == 0
                } else {
                    false
                };
            let should_request_incremental_snapshot =
                if let SnapshotInterval::Slots(snapshot_interval) = incremental_snapshot_interval {
                    bank.block_height() % snapshot_interval == 0
                } else {
                    false
                };

            if bank.slot() <= self.latest_abs_request_slot() {
                None
            } else if should_request_full_snapshot {
                Some((bank, SnapshotRequestKind::FullSnapshot))
            } else if should_request_incremental_snapshot {
                Some((bank, SnapshotRequestKind::IncrementalSnapshot))
            } else if request_fastboot_snapshot {
                Some((bank, SnapshotRequestKind::FastbootSnapshot))
            } else {
                None
            }
        }) {
            let bank_slot = bank.slot();
            self.set_latest_abs_request_slot(bank_slot);
            squash_timing += bank.squash();

            is_root_bank_squashed = bank_slot == root;

            let mut snapshot_time = Measure::start("squash::snapshot_time");
            // Save off the status cache because these may get pruned if another
            // `set_root()` is called before the snapshots package can be generated
            let status_cache_slot_deltas = bank.status_cache.read().unwrap().root_slot_deltas();
            if let Err(e) = self.abs_request_sender.send(SnapshotRequest {
                snapshot_root_bank: Arc::clone(bank),
                status_cache_slot_deltas,
                request_kind,
                enqueued: Instant::now(),
            }) {
                warn!("Error sending snapshot request for bank: {bank_slot}, err: {e:?}");
            }
            snapshot_time.stop();
            total_snapshot_ms += snapshot_time.as_ms();
        }

        (is_root_bank_squashed, squash_timing, total_snapshot_ms)
    }

    /// Returns the intervals, in slots, for sending snapshot requests
    fn snapshot_generation_intervals(&self) -> SnapshotGenerationIntervals {
        if self.snapshot_config.should_generate_snapshots() {
            SnapshotGenerationIntervals {
                full_snapshot_interval: self.snapshot_config.full_snapshot_archive_interval,
                incremental_snapshot_interval: self
                    .snapshot_config
                    .incremental_snapshot_archive_interval,
            }
        } else {
            SnapshotGenerationIntervals {
                full_snapshot_interval: SnapshotInterval::Disabled,
                incremental_snapshot_interval: SnapshotInterval::Disabled,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::accounts_background_service::SnapshotRequestKind,
        agave_snapshots::snapshot_config::SnapshotConfig, crossbeam_channel::unbounded,
        solana_genesis_config::create_genesis_config, solana_pubkey::Pubkey, std::sync::Arc,
        test_case::test_case,
    };

    fn create_banks(num_banks: u64) -> Vec<Arc<Bank>> {
        let mut banks = vec![];
        let (genesis_config, _) = create_genesis_config(1_000_000);
        let mut parent_bank = Arc::new(Bank::new_for_tests(&genesis_config));
        banks.push(parent_bank.clone());

        for _ in 1..=num_banks {
            let new_bank = Arc::new(Bank::new_from_parent(
                parent_bank.clone(),
                &Pubkey::default(),
                parent_bank.slot() + 1,
            ));
            parent_bank = new_bank;
            banks.push(parent_bank.clone());
        }

        banks
    }
    #[test_case(SnapshotInterval::Disabled, SnapshotInterval::Disabled,
        50, None, None; "Snapshots Disabled")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(5.try_into().unwrap()),
        4, None, None; "No snapshot triggered")]
    #[test_case(SnapshotInterval::Slots(5.try_into().unwrap()), SnapshotInterval::Disabled,
        8, Some(5), Some(SnapshotRequestKind::FullSnapshot); "Full without Incremental")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(10.try_into().unwrap()),
        10, Some(10), Some(SnapshotRequestKind::FullSnapshot); "Full and Incremental on same slot, full is selected")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(5.try_into().unwrap()),
        15, Some(15), Some(SnapshotRequestKind::IncrementalSnapshot); "Newer incremental picked over older Full")]
    #[test_case(SnapshotInterval::Disabled, SnapshotInterval::Slots(5.try_into().unwrap()),
        5, Some(5), Some(SnapshotRequestKind::IncrementalSnapshot); "Incremental without Full")]
    fn test_handle_new_roots(
        full_snapshot_archive_interval: SnapshotInterval,
        incremental_snapshot_archive_interval: SnapshotInterval,
        num_banks: u64,
        expected_snapshot_slot: Option<u64>,
        expected_snapshot_type: Option<SnapshotRequestKind>,
    ) {
        let banks = create_banks(num_banks);
        let banks = banks.iter().rev().collect::<Vec<_>>();

        let snapshot_config = SnapshotConfig {
            full_snapshot_archive_interval,
            incremental_snapshot_archive_interval,
            ..Default::default()
        };

        let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
        let snapshot_controller =
            SnapshotController::new(snapshot_request_sender, snapshot_config, 0);

        let (root_bank_squashed, _, _) = snapshot_controller.handle_new_roots(num_banks, &banks);

        // Verify that the root bank was squashed if and only if a snapshot was requested at that slot
        if expected_snapshot_slot == Some(num_banks) {
            assert!(root_bank_squashed);
        } else {
            assert!(!root_bank_squashed);
        }

        // Verify the latest_abs_request_slot is updated
        assert_eq!(
            snapshot_controller.latest_abs_request_slot(),
            expected_snapshot_slot.unwrap_or(0)
        );

        // Pull the snapshot request from the channel
        let sent_request = snapshot_request_receiver.try_recv();

        if let Some(expected_snapshot_type) = expected_snapshot_type {
            assert!(
                sent_request.is_ok(),
                "Expected a snapshot request to be sent"
            );
            let sent_request = sent_request.unwrap();
            assert_eq!(sent_request.request_kind, expected_snapshot_type);
            // Verify that the bank was squashed up to the snapshot slot
            assert_eq!(
                sent_request.snapshot_root_bank.slot(),
                expected_snapshot_slot.unwrap()
            );
        } else {
            assert!(
                sent_request.is_err(),
                "Expected no snapshot request to be sent"
            );
        }
    }

    #[test_case(SnapshotInterval::Disabled, SnapshotInterval::Disabled,
        50, SnapshotRequestKind::FastbootSnapshot; "Fastboot triggered when snapshots are disabled")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(5.try_into().unwrap()),
        4, SnapshotRequestKind::FastbootSnapshot; "Fastboot triggered when no other snapshot conditions are met")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Disabled,
        10, SnapshotRequestKind::FullSnapshot; "Full snapshot overrides fastboot on the same slot")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(5.try_into().unwrap()),
        5, SnapshotRequestKind::IncrementalSnapshot; "Incremental snapshot overrides fastboot on the same slot")]
    #[test_case(SnapshotInterval::Slots(10.try_into().unwrap()), SnapshotInterval::Slots(5.try_into().unwrap()),
        14, SnapshotRequestKind::FastbootSnapshot; "Fastboot triggered on a newer slot, overriding full and incremental")]
    fn test_fastboot_snapshot(
        full_snapshot_archive_interval: SnapshotInterval,
        incremental_snapshot_archive_interval: SnapshotInterval,
        num_banks: u64,
        expected_snapshot_type: SnapshotRequestKind,
    ) {
        let banks = create_banks(num_banks);
        let banks = banks.iter().rev().collect::<Vec<_>>();

        let snapshot_config = SnapshotConfig {
            full_snapshot_archive_interval,
            incremental_snapshot_archive_interval,
            ..Default::default()
        };

        let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
        let snapshot_controller =
            SnapshotController::new(snapshot_request_sender, snapshot_config, 0);

        snapshot_controller.request_fastboot_snapshot();

        let (root_bank_squashed, _, _) = snapshot_controller.handle_new_roots(num_banks, &banks);

        // Root bank should always be squashed when fastboot snapshot is requested
        assert!(root_bank_squashed);

        // Verify the latest_abs_request_slot is updated
        assert_eq!(snapshot_controller.latest_abs_request_slot(), num_banks,);

        // Pull the snapshot request from the channel
        let sent_request = snapshot_request_receiver.try_recv();

        assert!(
            sent_request.is_ok(),
            "Expected a snapshot request to be sent"
        );
        let sent_request = sent_request.unwrap();
        assert_eq!(sent_request.request_kind, expected_snapshot_type);
        // Verify that the bank was squashed up to the snapshot slot
        assert_eq!(sent_request.snapshot_root_bank.slot(), num_banks);
    }
}
