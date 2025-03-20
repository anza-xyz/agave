use {
    crate::{
        accounts_background_service::{AbsRequestSender, SnapshotRequest, SnapshotRequestKind},
        bank::{epoch_accounts_hash_utils, Bank, SquashTiming},
        bank_forks::SetRootError,
        snapshot_config::SnapshotConfig,
    },
    log::*,
    solana_measure::measure::Measure,
    solana_sdk::clock::Slot,
    std::{
        cmp,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Instant,
    },
};

#[derive(Default)]
pub struct SnapshotController {
    abs_request_sender: AbsRequestSender,
    snapshot_config: Option<SnapshotConfig>,
    latest_abs_request_slot: AtomicU64,
}

impl SnapshotController {
    pub fn new(
        abs_request_sender: AbsRequestSender,
        snapshot_config: Option<SnapshotConfig>,
        root_slot: Slot,
    ) -> Self {
        Self {
            abs_request_sender,
            snapshot_config,
            latest_abs_request_slot: AtomicU64::new(root_slot),
        }
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
        if let Some(abs_request_interval) = self.abs_request_interval() {
            if self.abs_request_sender.is_snapshot_creation_enabled() {
                if let Some(bank) = banks.iter().find(|bank| {
                    bank.slot() > self.latest_abs_request_slot()
                        && bank.block_height() % abs_request_interval == 0
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
                        if let Err(e) =
                            self.abs_request_sender
                                .send_snapshot_request(SnapshotRequest {
                                    snapshot_root_bank: Arc::clone(bank),
                                    status_cache_slot_deltas,
                                    request_kind: SnapshotRequestKind::Snapshot,
                                    enqueued: Instant::now(),
                                })
                        {
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
        }

        Ok((is_root_bank_squashed, squash_timing, total_snapshot_ms))
    }

    /// Returns the interval, in slots, for sending an ABS request
    ///
    /// Returns None if ABS requests should not be sent
    fn abs_request_interval(&self) -> Option<Slot> {
        self.snapshot_config.as_ref().and_then(|snapshot_config| {
            snapshot_config.should_generate_snapshots().then(||
                // N.B. This assumes if a snapshot is disabled that its interval will be Slot::MAX
                cmp::min(
                    snapshot_config.full_snapshot_archive_interval_slots,
                    snapshot_config.incremental_snapshot_archive_interval_slots,
                ))
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
            if let Err(e) = self
                .abs_request_sender
                .send_snapshot_request(SnapshotRequest {
                    snapshot_root_bank: Arc::clone(eah_bank),
                    status_cache_slot_deltas: Vec::default(),
                    request_kind: SnapshotRequestKind::EpochAccountsHash,
                    enqueued: Instant::now(),
                })
            {
                return Err(SetRootError::SendEpochAccountHashError(eah_bank.slot(), e));
            };
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
