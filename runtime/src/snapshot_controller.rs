use {
    crate::{
        accounts_background_service::{
            SnapshotRequest, SnapshotRequestKind, SnapshotRequestSender,
        },
        bank::{Bank, SquashTiming},
        bank_forks::SetRootError,
        snapshot_config::SnapshotConfig,
        snapshot_utils::SnapshotInterval,
    },
    log::*,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    std::{
        sync::{
            atomic::{AtomicU64, Ordering},
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

    pub fn handle_new_roots(
        &self,
        root: Slot,
        banks: &[&Arc<Bank>],
    ) -> Result<(bool, SquashTiming, u64), SetRootError> {
        let mut is_root_bank_squashed = false;
        let mut squash_timing = SquashTiming::default();
        let mut total_snapshot_ms = 0;

        if let Some(SnapshotGenerationIntervals {
            full_snapshot_interval,
            incremental_snapshot_interval,
        }) = self.snapshot_generation_intervals()
        {
            if let Some((bank, request_kind)) = banks.iter().find_map(|bank| {
                let should_request_full_snapshot =
                    if let SnapshotInterval::Slots(snapshot_interval) = full_snapshot_interval {
                        bank.block_height() % snapshot_interval == 0
                    } else {
                        false
                    };
                let should_request_incremental_snapshot =
                    if let SnapshotInterval::Slots(snapshot_interval) =
                        incremental_snapshot_interval
                    {
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
                } else {
                    None
                }
            }) {
                let bank_slot = bank.slot();
                self.set_latest_abs_request_slot(bank_slot);
                squash_timing += bank.squash();

                is_root_bank_squashed = bank_slot == root;

                let mut snapshot_time = Measure::start("squash::snapshot_time");
                if bank.has_initial_accounts_hash_verification_completed() {
                    // Save off the status cache because these may get pruned if another
                    // `set_root()` is called before the snapshots package can be generated
                    let status_cache_slot_deltas =
                        bank.status_cache.read().unwrap().root_slot_deltas();
                    if let Err(e) = self.abs_request_sender.send(SnapshotRequest {
                        snapshot_root_bank: Arc::clone(bank),
                        status_cache_slot_deltas,
                        request_kind,
                        enqueued: Instant::now(),
                    }) {
                        warn!(
                            "Error sending snapshot request for bank: {}, err: {:?}",
                            bank_slot, e
                        );
                    }
                } else {
                    info!("Not sending snapshot request for bank: {}, startup verification is incomplete", bank_slot);
                }
                snapshot_time.stop();
                total_snapshot_ms += snapshot_time.as_ms();
            }
        }

        Ok((is_root_bank_squashed, squash_timing, total_snapshot_ms))
    }

    /// Returns the intervals, in slots, for sending snapshot requests
    ///
    /// Returns None if snapshot generation is disabled and snapshot requests
    /// should not be sent
    fn snapshot_generation_intervals(&self) -> Option<SnapshotGenerationIntervals> {
        self.snapshot_config
            .should_generate_snapshots()
            .then_some(SnapshotGenerationIntervals {
                full_snapshot_interval: self.snapshot_config.full_snapshot_archive_interval,
                incremental_snapshot_interval: self
                    .snapshot_config
                    .incremental_snapshot_archive_interval,
            })
    }
}
