use {
    crate::{
        accounts_background_service::{
            SnapshotRequest, SnapshotRequestKind, SnapshotRequestSender,
        },
        bank::{epoch_accounts_hash_utils, Bank, SquashTiming},
        bank_forks::SetRootError,
        snapshot_config::SnapshotConfig,
    },
    log::*,
    solana_measure::measure::Measure,
    solana_sdk::clock::Slot,
    std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Instant,
    },
};

struct SnapshotGenerationIntervals {
    full_snapshot_interval: Slot,
    incremental_snapshot_interval: Slot,
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
        let (mut is_root_bank_squashed, mut squash_timing) =
            self.send_eah_request_if_needed(root, banks)?;
        let mut total_snapshot_ms = 0;

        // After checking for EAH requests, also check for regular snapshot requests.
        //
        // This is needed when a snapshot request occurs in a slot after an EAH request, and is
        // part of the same set of `banks` in a single `set_root()` invocation.  While (very)
        // unlikely for a validator with default snapshot intervals (and accounts hash verifier
        // intervals), it *is* possible, and there are tests to exercise this possibility.
        if let Some(SnapshotGenerationIntervals {
            full_snapshot_interval,
            incremental_snapshot_interval,
        }) = self.snapshot_generation_intervals()
        {
            if let Some((bank, request_kind)) = banks.iter().find_map(|bank| {
                if bank.slot() <= self.latest_abs_request_slot() {
                    None
                } else if bank.block_height() % full_snapshot_interval == 0 {
                    Some((bank, SnapshotRequestKind::FullSnapshot))
                } else if bank.block_height() % incremental_snapshot_interval == 0 {
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
                if bank.is_startup_verification_complete() {
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
                full_snapshot_interval: self.snapshot_config.full_snapshot_archive_interval_slots,
                incremental_snapshot_interval: self
                    .snapshot_config
                    .incremental_snapshot_archive_interval_slots,
            })
    }

    /// Sends an EpochAccountsHash request if one of the `banks` crosses the EAH boundary.
    /// Returns if the bank at slot `root` was squashed, and its timings.
    ///
    /// Panics if more than one bank in `banks` should send an EAH request.
    pub fn send_eah_request_if_needed(
        &self,
        root: Slot,
        banks: &[&Arc<Bank>],
    ) -> Result<(bool, SquashTiming), SetRootError> {
        let mut is_root_bank_squashed = false;
        let mut squash_timing = SquashTiming::default();

        // Go through all the banks and see if we should send an EAH request.
        // Only one EAH bank is allowed to send an EAH request.
        // NOTE: Instead of filter-collect-assert, `.find()` could be used instead.
        // Once sufficient testing guarantees only one bank will ever request an EAH,
        // change to `.find()`.
        let eah_banks: Vec<_> = banks
            .iter()
            .filter(|bank| self.should_request_epoch_accounts_hash(bank))
            .collect();
        assert!(
            eah_banks.len() <= 1,
            "At most one bank should request an epoch accounts hash calculation! num banks: {}, bank slots: {:?}",
            eah_banks.len(),
            eah_banks.iter().map(|bank| bank.slot()).collect::<Vec<_>>(),
        );
        if let Some(&&eah_bank) = eah_banks.first() {
            debug!(
                "sending epoch accounts hash request, slot: {}",
                eah_bank.slot(),
            );

            self.set_latest_abs_request_slot(eah_bank.slot());
            squash_timing += eah_bank.squash();
            is_root_bank_squashed = eah_bank.slot() == root;

            eah_bank
                .rc
                .accounts
                .accounts_db
                .epoch_accounts_hash_manager
                .set_in_flight(eah_bank.slot());

            if let Err(err) = self.abs_request_sender.send(SnapshotRequest {
                snapshot_root_bank: Arc::clone(eah_bank),
                status_cache_slot_deltas: Vec::default(),
                request_kind: SnapshotRequestKind::EpochAccountsHash,
                enqueued: Instant::now(),
            }) {
                return Err(SetRootError::SendEpochAccountHashError(
                    eah_bank.slot(),
                    err,
                ));
            }
        }

        Ok((is_root_bank_squashed, squash_timing))
    }

    /// Determine if this bank should request an epoch accounts hash
    #[must_use]
    fn should_request_epoch_accounts_hash(&self, bank: &Bank) -> bool {
        if !epoch_accounts_hash_utils::is_enabled_this_epoch(bank) {
            return false;
        }

        let start_slot = epoch_accounts_hash_utils::calculation_start(bank);
        bank.slot() > self.latest_abs_request_slot()
            && bank.parent_slot() < start_slot
            && bank.slot() >= start_slot
    }
}
