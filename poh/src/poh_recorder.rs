//! The `poh_recorder` module provides an object for synchronizing with Proof of History.
//! It synchronizes PoH, bank's register_tick and the ledger
//!
//! PohRecorder will send ticks or entries to a WorkingBank, if the current range of ticks is
//! within the specified WorkingBank range.
//!
//! For Ticks:
//! * new tick_height must be > WorkingBank::min_tick_height && new tick_height must be <= WorkingBank::max_tick_height
//!
//! For Entries:
//! * recorded entry must be >= WorkingBank::min_tick_height && entry must be < WorkingBank::max_tick_height
//!
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        poh_controller::PohController, poh_service::PohService, record_channels::record_channels,
        transaction_recorder::TransactionRecorder,
    },
    arc_swap::ArcSwap,
    crossbeam_channel::{unbounded, Receiver, SendError, Sender, TrySendError},
    log::*,
    solana_clock::{BankId, Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_entry::{
        entry::Entry,
        poh::{Poh, PohEntry},
    },
    solana_hash::Hash,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_measure::measure_us,
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    solana_transaction::versioned::VersionedTransaction,
    std::{
        cmp,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex, RwLock,
        },
        time::Instant,
    },
    thiserror::Error,
};

pub const GRACE_TICKS_FACTOR: u64 = 2;
pub const MAX_GRACE_SLOTS: u64 = 2;

#[derive(Error, Debug, Clone)]
pub enum PohRecorderError {
    #[error("max height reached")]
    MaxHeightReached,

    #[error("min height not reached")]
    MinHeightNotReached,

    #[error("send WorkingBankEntry error")]
    SendError(#[from] SendError<WorkingBankEntry>),

    #[error("channel full")]
    ChannelFull,

    #[error("channel disconnected")]
    ChannelDisconnected,
}

pub(crate) type Result<T> = std::result::Result<T, PohRecorderError>;

pub type WorkingBankEntry = (Arc<Bank>, (Entry, u64));

#[derive(Debug)]
pub struct RecordSummary {
    pub remaining_hashes_in_slot: u64,
}

pub struct Record {
    pub mixins: Vec<Hash>,
    pub transaction_batches: Vec<Vec<VersionedTransaction>>,
    pub bank_id: BankId,
}

impl Record {
    pub fn new(
        mixins: Vec<Hash>,
        transaction_batches: Vec<Vec<VersionedTransaction>>,
        bank_id: BankId,
    ) -> Self {
        Self {
            mixins,
            transaction_batches,
            bank_id,
        }
    }
}

pub struct WorkingBank {
    pub bank: BankWithScheduler,
    pub start: Arc<Instant>,
    pub min_tick_height: u64,
    pub max_tick_height: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PohLeaderStatus {
    NotReached,
    Reached { poh_slot: Slot, parent_slot: Slot },
}

struct PohRecorderMetrics {
    flush_cache_tick_us: u64,
    flush_cache_no_tick_us: u64,
    record_us: u64,
    record_lock_contention_us: u64,
    report_metrics_us: u64,
    send_entry_us: u64,
    tick_lock_contention_us: u64,
    ticks_from_record: u64,
    total_sleep_us: u64,
    last_metric: Instant,
}

impl Default for PohRecorderMetrics {
    fn default() -> Self {
        Self {
            flush_cache_tick_us: 0,
            flush_cache_no_tick_us: 0,
            record_us: 0,
            record_lock_contention_us: 0,
            report_metrics_us: 0,
            send_entry_us: 0,
            tick_lock_contention_us: 0,
            ticks_from_record: 0,
            total_sleep_us: 0,
            last_metric: Instant::now(),
        }
    }
}

impl PohRecorderMetrics {
    fn report(&mut self, bank_slot: Slot) {
        if self.last_metric.elapsed().as_millis() > 1000 {
            datapoint_info!(
                "poh_recorder",
                ("slot", bank_slot, i64),
                ("flush_cache_tick_us", self.flush_cache_tick_us, i64),
                ("flush_cache_no_tick_us", self.flush_cache_no_tick_us, i64),
                ("record_us", self.record_us, i64),
                (
                    "record_lock_contention_us",
                    self.record_lock_contention_us,
                    i64
                ),
                ("report_metrics_us", self.report_metrics_us, i64),
                ("send_entry_us", self.send_entry_us, i64),
                ("tick_lock_contention", self.tick_lock_contention_us, i64),
                ("ticks_from_record", self.ticks_from_record, i64),
                ("total_sleep_us", self.total_sleep_us, i64),
            );

            *self = Self::default();
        }
    }
}

pub struct PohRecorder {
    pub(crate) poh: Arc<Mutex<Poh>>,
    clear_bank_signal: Option<Sender<bool>>,
    start_bank: Arc<Bank>, // parent slot
    start_bank_active_descendants: Vec<Slot>,
    start_tick_height: u64, // first tick_height this recorder will observe
    tick_cache: Vec<(Entry, u64)>, // cache of entry and its tick_height
    /// This stores the current working bank + scheduler and other metadata,
    /// if they exist.
    /// This field MUST be kept consistent with the `shared_leader_state` field.
    working_bank: Option<WorkingBank>,
    shared_leader_state: SharedLeaderState,
    working_bank_sender: Sender<WorkingBankEntry>,
    leader_last_tick_height: u64, // zero if none
    grace_ticks: u64,
    blockstore: Arc<Blockstore>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    ticks_per_slot: u64,
    metrics: PohRecorderMetrics,
    delay_leader_block_for_pending_fork: bool,
    last_reported_slot_for_pending_fork: Arc<Mutex<Slot>>,
    pub is_exited: Arc<AtomicBool>,

    // Allocation to hold PohEntrys recorded into PoHStream.
    entries: Vec<PohEntry>,

    // Alpenglow related migration things
    pub is_alpenglow_enabled: bool,

    /// When alpenglow is enabled there will be no ticks apart from a final one
    /// to complete the block. This tick will not be verified, and we use this
    /// flag to unset hashes_per_tick
    alpenglow_enabled: bool,
}

impl PohRecorder {
    /// A recorder to synchronize PoH with the following data structures
    /// * bank - the LastId's queue is updated on `tick` and `record` events
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tick_height: u64,
        last_entry_hash: Hash,
        start_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
        ticks_per_slot: u64,
        blockstore: Arc<Blockstore>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        poh_config: &PohConfig,
        is_exited: Arc<AtomicBool>,
    ) -> (Self, Receiver<WorkingBankEntry>) {
        let delay_leader_block_for_pending_fork = false;
        Self::new_with_clear_signal(
            tick_height,
            last_entry_hash,
            start_bank,
            next_leader_slot,
            ticks_per_slot,
            delay_leader_block_for_pending_fork,
            blockstore,
            None,
            leader_schedule_cache,
            poh_config,
            is_exited,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_clear_signal(
        tick_height: u64,
        last_entry_hash: Hash,
        start_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
        ticks_per_slot: u64,
        delay_leader_block_for_pending_fork: bool,
        blockstore: Arc<Blockstore>,
        clear_bank_signal: Option<Sender<bool>>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        poh_config: &PohConfig,
        is_exited: Arc<AtomicBool>,
    ) -> (Self, Receiver<WorkingBankEntry>) {
        let tick_number = 0;
        let poh = Arc::new(Mutex::new(Poh::new_with_slot_info(
            last_entry_hash,
            poh_config.hashes_per_tick,
            tick_number,
        )));

        let (working_bank_sender, working_bank_receiver) = unbounded();
        let (leader_first_tick_height, leader_last_tick_height, grace_ticks) =
            Self::compute_leader_slot_tick_heights(next_leader_slot, ticks_per_slot);
        (
            Self {
                poh,
                tick_cache: vec![],
                working_bank: None,
                shared_leader_state: SharedLeaderState::new(
                    tick_height,
                    leader_first_tick_height,
                    next_leader_slot,
                ),
                working_bank_sender,
                clear_bank_signal,
                start_bank,
                start_bank_active_descendants: vec![],
                start_tick_height: tick_height + 1,
                leader_last_tick_height,
                grace_ticks,
                blockstore,
                leader_schedule_cache: leader_schedule_cache.clone(),
                ticks_per_slot,
                metrics: PohRecorderMetrics::default(),
                delay_leader_block_for_pending_fork,
                last_reported_slot_for_pending_fork: Arc::default(),
                is_exited,
                entries: Vec::with_capacity(64),
                is_alpenglow_enabled: false,
                alpenglow_enabled: false,
            },
            working_bank_receiver,
        )
    }

    // synchronize PoH with a bank
    pub fn reset(&mut self, reset_bank: Arc<Bank>, next_leader_slot: Option<(Slot, Slot)>) {
        self.clear_bank(false);
        let tick_height = self.reset_poh(reset_bank, true);

        let (leader_first_tick_height, leader_last_tick_height, grace_ticks) =
            Self::compute_leader_slot_tick_heights(next_leader_slot, self.ticks_per_slot);
        self.grace_ticks = grace_ticks;

        // Above call to `clear_bank` did not set the shared state,
        // nor did `reset_poh` update the tick_height.
        // Do the atomic swap of state here to reflect the reset.
        self.shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            tick_height,
            leader_first_tick_height,
            next_leader_slot,
        )));

        self.leader_last_tick_height = leader_last_tick_height;
    }

    // Returns the index of `transactions.first()` in the slot, if being tracked by WorkingBank
    pub fn record(
        &mut self,
        bank_id: BankId,
        mixins: Vec<Hash>,
        transaction_batches: Vec<Vec<VersionedTransaction>>,
    ) -> Result<RecordSummary> {
        // Entries without transactions are used to track real-time passing in the ledger and
        // cannot be generated by `record()`
        assert!(
            mixins.len() == transaction_batches.len(),
            "mismatched mixin and transaction batch lengths"
        );
        assert!(
            !transaction_batches.iter().any(|batch| batch.is_empty()),
            "No transactions provided"
        );

        if let Some(working_bank) = self.working_bank.as_ref() {
            let ((), report_metrics_us) =
                measure_us!(self.metrics.report(working_bank.bank.slot()));
            self.metrics.report_metrics_us += report_metrics_us;
        }

        loop {
            let (flush_cache_res, flush_cache_us) = measure_us!(self.flush_cache(false));
            self.metrics.flush_cache_no_tick_us += flush_cache_us;
            flush_cache_res?;

            let tick_height = self.tick_height(); // cannot change until next loop iteration.
            let working_bank = self
                .working_bank
                .as_mut()
                .ok_or(PohRecorderError::MaxHeightReached)?;
            if bank_id != working_bank.bank.bank_id() {
                return Err(PohRecorderError::MaxHeightReached);
            }

            let (mut poh_lock, poh_lock_us) = measure_us!(self.poh.lock().unwrap());
            self.metrics.record_lock_contention_us += poh_lock_us;

            let (mixed_in, record_mixin_us) =
                measure_us!(poh_lock.record_batches(&mixins, &mut self.entries));
            self.metrics.record_us += record_mixin_us;
            let remaining_hashes_in_slot =
                poh_lock.remaining_hashes_in_slot(working_bank.bank.ticks_per_slot());

            drop(poh_lock);

            if mixed_in {
                debug_assert_eq!(self.entries.len(), mixins.len());
                for (entry, transactions) in self.entries.drain(..).zip(transaction_batches) {
                    let (send_entry_res, send_batches_us) =
                        measure_us!(self.working_bank_sender.send((
                            working_bank.bank.clone(),
                            (
                                Entry {
                                    num_hashes: entry.num_hashes,
                                    hash: entry.hash,
                                    transactions,
                                },
                                tick_height, // `record_batches` guarantees that mixins are **not** split across ticks.
                            ),
                        )));
                    self.metrics.send_entry_us += send_batches_us;
                    send_entry_res?;
                }

                return Ok(RecordSummary {
                    remaining_hashes_in_slot,
                });
            }

            // record() might fail if the next PoH hash needs to be a tick.  But that's ok, tick()
            // and re-record()
            self.metrics.ticks_from_record += 1;
            self.tick();
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn tick(&mut self) {
        let (poh_entry, tick_lock_contention_us) = measure_us!({
            let mut poh_l = self.poh.lock().unwrap();
            poh_l.tick()
        });
        self.metrics.tick_lock_contention_us += tick_lock_contention_us;

        if let Some(poh_entry) = poh_entry {
            self.shared_leader_state.increment_tick_height();
            trace!("tick_height {}", self.tick_height());

            if self
                .shared_leader_state
                .load()
                .leader_first_tick_height
                .is_none()
            {
                return;
            }

            self.tick_cache.push((
                Entry {
                    num_hashes: poh_entry.num_hashes,
                    hash: poh_entry.hash,
                    transactions: vec![],
                },
                self.tick_height(),
            ));

            let (_flush_res, flush_cache_and_tick_us) = measure_us!(self.flush_cache(true));
            self.metrics.flush_cache_tick_us += flush_cache_and_tick_us;
        }
    }

    pub fn set_bank(&mut self, bank: BankWithScheduler) {
        assert!(self.working_bank.is_none());
        let working_bank = WorkingBank {
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.max_tick_height(),
            bank,
            start: Arc::new(Instant::now()),
        };
        trace!("new working bank");
        assert_eq!(working_bank.bank.ticks_per_slot(), self.ticks_per_slot());
        let mut tick_height = self.tick_height();
        if let Some(hashes_per_tick) = *working_bank.bank.hashes_per_tick() {
            if self.poh.lock().unwrap().hashes_per_tick() != hashes_per_tick {
                // We must clear/reset poh when changing hashes per tick because it's
                // possible there are ticks in the cache created with the old hashes per
                // tick value that would get flushed later. This would corrupt the leader's
                // block and it would be disregarded by the network.
                info!(
                    "resetting poh due to hashes per tick change detected at {}",
                    working_bank.bank.slot()
                );
                tick_height = self.reset_poh(working_bank.bank.clone(), false);
            }
        }

        let leader_state = self.shared_leader_state.load();
        let leader_first_tick_height = leader_state.leader_first_tick_height();
        let next_leader_slot = leader_state.next_leader_slot_range();
        drop(leader_state);
        self.shared_leader_state.store(Arc::new(LeaderState::new(
            Some(working_bank.bank.clone_without_scheduler()),
            tick_height,
            leader_first_tick_height,
            next_leader_slot,
        )));
        self.working_bank = Some(working_bank);

        // TODO: adjust the working_bank.start time based on number of ticks
        // that have already elapsed based on current tick height.
        let _ = self.flush_cache(false);
    }

    /// Clears the working bank.
    /// Updates [`Self::shared_leader_state`] if `set_shared_state` is true.
    /// Otherwise the caller is responsible for setting the state before
    /// releasing the lock.
    fn clear_bank(&mut self, set_shared_state: bool) {
        if let Some(WorkingBank { bank, start, .. }) = self.working_bank.take() {
            let next_leader_slot = self.leader_schedule_cache.next_leader_slot(
                bank.collector_id(),
                bank.slot(),
                &bank,
                Some(&self.blockstore),
                GRACE_TICKS_FACTOR * MAX_GRACE_SLOTS,
            );
            assert_eq!(self.ticks_per_slot, bank.ticks_per_slot());
            let (leader_first_tick_height, leader_last_tick_height, grace_ticks) =
                Self::compute_leader_slot_tick_heights(next_leader_slot, self.ticks_per_slot);
            self.grace_ticks = grace_ticks;
            self.leader_last_tick_height = leader_last_tick_height;

            // Only update if `set_shared_state` is true.
            // If `false` it is the caller's responsibility to set the shared state.
            if set_shared_state {
                self.shared_leader_state.store(Arc::new(LeaderState::new(
                    None,
                    self.tick_height(),
                    leader_first_tick_height,
                    next_leader_slot,
                )));
            }

            datapoint_info!(
                "leader-slot-start-to-cleared-elapsed-ms",
                ("slot", bank.slot(), i64),
                ("elapsed", start.elapsed().as_millis(), i64),
            );
        }

        if let Some(ref signal) = self.clear_bank_signal {
            match signal.try_send(true) {
                Ok(_) => {}
                Err(TrySendError::Full(_)) => {
                    trace!("replay wake up signal channel is full.")
                }
                Err(TrySendError::Disconnected(_)) => {
                    trace!("replay wake up signal channel is disconnected.")
                }
            }
        }
    }

    /// Returns tick_height - does not update the internal state for tick_height.
    #[must_use]
    fn reset_poh(&mut self, reset_bank: Arc<Bank>, reset_start_bank: bool) -> u64 {
        let blockhash = reset_bank.last_blockhash();
        let hashes_per_tick = if self.alpenglow_enabled {
            None
        } else {
            *reset_bank.hashes_per_tick()
        };
        let poh_hash = {
            let mut poh = self.poh.lock().unwrap();
            poh.reset(blockhash, hashes_per_tick);
            poh.hash
        };
        info!(
            "reset poh from: {},{},{} to: {},{}",
            poh_hash,
            self.tick_height(),
            self.start_slot(),
            blockhash,
            reset_bank.slot()
        );

        self.tick_cache = vec![];
        if reset_start_bank {
            self.start_bank = reset_bank;
            self.start_bank_active_descendants = vec![];
        }

        let tick_height = (self.start_slot() + 1) * self.ticks_per_slot;
        self.start_tick_height = tick_height + 1;

        tick_height
    }

    // Flush cache will delay flushing the cache for a bank until it past the WorkingBank::min_tick_height
    // On a record flush will flush the cache at the WorkingBank::min_tick_height, since a record
    // occurs after the min_tick_height was generated
    fn flush_cache(&mut self, tick: bool) -> Result<()> {
        // check_tick_height is called before flush cache, so it cannot overrun the bank
        // so a bank that is so late that it's slot fully generated before it starts recording
        // will fail instead of broadcasting any ticks
        let working_bank = self
            .working_bank
            .as_ref()
            .ok_or(PohRecorderError::MaxHeightReached)?;
        if self.tick_height() < working_bank.min_tick_height {
            return Err(PohRecorderError::MinHeightNotReached);
        }
        if tick && self.tick_height() == working_bank.min_tick_height {
            return Err(PohRecorderError::MinHeightNotReached);
        }

        let entry_count = self
            .tick_cache
            .iter()
            .take_while(|x| x.1 <= working_bank.max_tick_height)
            .count();
        let mut send_result: std::result::Result<(), SendError<WorkingBankEntry>> = Ok(());

        if entry_count > 0 {
            trace!(
                "flush_cache: bank_slot: {} tick_height: {} max: {} sending: {}",
                working_bank.bank.slot(),
                working_bank.bank.tick_height(),
                working_bank.max_tick_height,
                entry_count,
            );

            for tick in &self.tick_cache[..entry_count] {
                working_bank.bank.register_tick(&tick.0.hash);
                send_result = self
                    .working_bank_sender
                    .send((working_bank.bank.clone(), tick.clone()));
                if send_result.is_err() {
                    break;
                }
            }
        }
        if self.tick_height() >= working_bank.max_tick_height {
            info!(
                "poh_record: max_tick_height {} reached, clearing working_bank {}",
                working_bank.max_tick_height,
                working_bank.bank.slot()
            );
            self.start_bank = working_bank.bank.clone();
            let working_slot = self.start_slot();
            self.start_tick_height = working_slot * self.ticks_per_slot + 1;
            self.clear_bank(true);
        }
        if send_result.is_err() {
            info!("WorkingBank::sender disconnected {send_result:?}");
            // revert the cache, but clear the working bank
            self.clear_bank(true);
        } else {
            // commit the flush
            let _ = self.tick_cache.drain(..entry_count);
        }

        Ok(())
    }

    pub fn would_be_leader(&self, within_next_n_ticks: u64) -> bool {
        self.has_bank()
            || self
                .leader_first_tick_height()
                .is_some_and(|leader_first_tick_height| {
                    let tick_height = self.tick_height();
                    tick_height + within_next_n_ticks >= leader_first_tick_height
                        && tick_height <= self.leader_last_tick_height
                })
    }

    // Return the slot for a given tick height
    fn slot_for_tick_height(&self, tick_height: u64) -> Slot {
        // We need to subtract by one here because, assuming ticks per slot is 64,
        // tick heights [1..64] correspond to slot 0. The last tick height of a slot
        // is always a multiple of 64.
        tick_height.saturating_sub(1) / self.ticks_per_slot
    }

    /// Return the slot that PoH is currently ticking through.
    fn current_poh_slot(&self) -> Slot {
        // The tick_height field is initialized to the last tick of the start
        // bank and generally indicates what tick height has already been
        // reached so use the next tick height to determine which slot poh is
        // ticking through.
        let next_tick_height = self.tick_height().saturating_add(1);
        self.slot_for_tick_height(next_tick_height)
    }

    pub fn leader_after_n_slots(&self, slots: u64) -> Option<Pubkey> {
        self.leader_schedule_cache
            .slot_leader_at(self.current_poh_slot() + slots, None)
    }

    /// Return the leader and slot pair after `slots_in_the_future` slots.
    pub fn leader_and_slot_after_n_slots(
        &self,
        slots_in_the_future: u64,
    ) -> Option<(Pubkey, Slot)> {
        let target_slot = self.current_poh_slot().checked_add(slots_in_the_future)?;
        self.leader_schedule_cache
            .slot_leader_at(target_slot, None)
            .map(|leader| (leader, target_slot))
    }

    pub fn shared_leader_state(&self) -> SharedLeaderState {
        self.shared_leader_state.clone()
    }

    pub fn bank(&self) -> Option<Arc<Bank>> {
        self.working_bank.as_ref().map(|w| w.bank.clone())
    }

    pub fn has_bank(&self) -> bool {
        self.working_bank.is_some()
    }

    pub fn tick_height(&self) -> u64 {
        self.shared_leader_state.load().tick_height()
    }

    fn leader_first_tick_height(&self) -> Option<u64> {
        self.shared_leader_state.load().leader_first_tick_height()
    }

    pub fn ticks_per_slot(&self) -> u64 {
        self.ticks_per_slot
    }

    pub fn start_slot(&self) -> Slot {
        self.start_bank.slot()
    }

    /// Returns if the leader slot has been reached along with the current poh
    /// slot and the parent slot (could be a few slots ago if any previous
    /// leaders needed to be skipped).
    pub fn reached_leader_slot(&self, my_pubkey: &Pubkey) -> PohLeaderStatus {
        trace!(
            "tick_height {}, start_tick_height {}, leader_first_tick_height {:?}, grace_ticks {}, \
             has_bank {}",
            self.tick_height(),
            self.start_tick_height,
            self.leader_first_tick_height(),
            self.grace_ticks,
            self.has_bank()
        );

        let current_poh_slot = self.current_poh_slot();
        let Some(leader_first_tick_height) = self.leader_first_tick_height() else {
            // No next leader slot, so no leader slot has been reached.
            return PohLeaderStatus::NotReached;
        };

        if !self.reached_leader_tick(my_pubkey, leader_first_tick_height) {
            // PoH hasn't ticked far enough yet.
            return PohLeaderStatus::NotReached;
        }

        if self
            .blockstore
            .has_existing_shreds_for_slot(current_poh_slot)
        {
            // We already have existing shreds for this slot. This can happen when this block was previously
            // created and added to BankForks, however a recent PoH reset caused this bank to be removed
            // as it was not part of the rooted fork. If this slot is not the first slot for this leader,
            // and the first slot was previously ticked over, the check in `leader_schedule_cache::next_leader_slot`
            // will not suffice, as it only checks if there are shreds for the first slot.
            return PohLeaderStatus::NotReached;
        }

        let poh_slot = current_poh_slot;
        let parent_slot = self.start_slot();
        PohLeaderStatus::Reached {
            poh_slot,
            parent_slot,
        }
    }

    fn reached_leader_tick(&self, my_pubkey: &Pubkey, leader_first_tick_height: u64) -> bool {
        if self.start_tick_height == leader_first_tick_height {
            // PoH was reset to run immediately.
            return true;
        }

        let ideal_target_tick_height = leader_first_tick_height.saturating_sub(1);
        if self.tick_height() < ideal_target_tick_height {
            // We haven't ticked to our leader slot yet.
            return false;
        }

        if self.tick_height() >= ideal_target_tick_height.saturating_add(self.grace_ticks) {
            // We have finished waiting for grace ticks.
            return true;
        }

        // We're in the grace tick zone. Check if we can skip grace ticks.
        let next_leader_slot = self.current_poh_slot();
        self.can_skip_grace_ticks(my_pubkey, next_leader_slot)
    }

    fn can_skip_grace_ticks(&self, my_pubkey: &Pubkey, next_leader_slot: Slot) -> bool {
        if self.start_slot_was_mine(my_pubkey) {
            // Building off my own block. No need to wait.
            return true;
        }

        if self.start_slot_was_mine_or_previous_leader(next_leader_slot) {
            // Planning to build off block produced by the leader previous to
            // me. Check if they've completed all of their slots.
            return self.building_off_previous_leader_last_block(my_pubkey, next_leader_slot);
        }

        if !self.is_new_reset_bank_pending(next_leader_slot) {
            // No pending blocks from previous leader have been observed. No
            // need to wait.
            return true;
        }

        self.report_pending_fork_was_detected(next_leader_slot);
        if !self.delay_leader_block_for_pending_fork {
            // Not configured to wait for pending blocks from previous leader.
            return true;
        }

        // Wait for grace ticks
        false
    }

    fn start_slot_was_mine_or_previous_leader(&self, next_leader_slot: Slot) -> bool {
        (next_leader_slot.saturating_sub(NUM_CONSECUTIVE_LEADER_SLOTS)..next_leader_slot).any(
            |slot| {
                // Check if the last slot PoH reset to was any of the
                // previous leader's slots.
                // If so, PoH is currently building on the previous leader's blocks
                // If not, PoH is building on a different fork
                slot == self.start_slot()
            },
        )
    }

    // Check if the last slot PoH reset onto was the previous leader's last slot.
    fn building_off_previous_leader_last_block(
        &self,
        my_pubkey: &Pubkey,
        next_leader_slot: Slot,
    ) -> bool {
        // Walk backwards from the slot before our next leader slot.
        for slot in
            (next_leader_slot.saturating_sub(NUM_CONSECUTIVE_LEADER_SLOTS)..next_leader_slot).rev()
        {
            // Identify which leader is responsible for building this slot.
            let leader_for_slot = self.leader_schedule_cache.slot_leader_at(slot, None);
            let Some(leader_for_slot) = leader_for_slot else {
                // No leader for this slot, skip
                continue;
            };

            // If the leader for this slot is not me, then it's the previous
            // leader's last slot.
            if leader_for_slot != *my_pubkey {
                // Check if the last slot PoH reset onto was the previous leader's last slot.
                return slot == self.start_slot();
            }
        }
        false
    }

    fn start_slot_was_mine(&self, my_pubkey: &Pubkey) -> bool {
        self.start_bank.collector_id() == my_pubkey
    }

    // Active descendants of the last reset bank that are smaller than the
    // next leader slot could soon become the new reset bank.
    fn is_new_reset_bank_pending(&self, next_leader_slot: Slot) -> bool {
        self.start_bank_active_descendants
            .iter()
            .any(|pending_slot| *pending_slot < next_leader_slot)
    }

    // Report metrics when poh recorder detects a pending fork that could
    // soon lead to poh reset.
    fn report_pending_fork_was_detected(&self, next_leader_slot: Slot) {
        // Only report once per next leader slot to avoid spamming metrics. It's
        // enough to know that a leader decided to delay or not once per slot
        let mut last_slot = self.last_reported_slot_for_pending_fork.lock().unwrap();
        if *last_slot == next_leader_slot {
            return;
        }
        *last_slot = next_leader_slot;

        datapoint_info!(
            "poh_recorder-detected_pending_fork",
            ("next_leader_slot", next_leader_slot, i64),
            (
                "did_delay_leader_slot",
                self.delay_leader_block_for_pending_fork,
                bool
            ),
        );
    }

    // returns (leader_first_tick_height, leader_last_tick_height, grace_ticks) given the next
    //  slot this recorder will lead
    fn compute_leader_slot_tick_heights(
        next_leader_slot: Option<(Slot, Slot)>,
        ticks_per_slot: u64,
    ) -> (Option<u64>, u64, u64) {
        next_leader_slot
            .map(|(first_slot, last_slot)| {
                let leader_first_tick_height = first_slot * ticks_per_slot + 1;
                let last_tick_height = (last_slot + 1) * ticks_per_slot;
                let num_slots = last_slot - first_slot + 1;
                let grace_ticks = cmp::min(
                    ticks_per_slot * MAX_GRACE_SLOTS,
                    ticks_per_slot * num_slots / GRACE_TICKS_FACTOR,
                );
                (
                    Some(leader_first_tick_height),
                    last_tick_height,
                    grace_ticks,
                )
            })
            .unwrap_or((
                None,
                0,
                cmp::min(
                    ticks_per_slot * MAX_GRACE_SLOTS,
                    ticks_per_slot * NUM_CONSECUTIVE_LEADER_SLOTS / GRACE_TICKS_FACTOR,
                ),
            ))
    }

    // update the list of active descendants of the start bank to make a better
    // decision about whether to use grace ticks
    pub fn update_start_bank_active_descendants(&mut self, active_descendants: &[Slot]) {
        self.start_bank_active_descendants = active_descendants.to_vec();
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_bank_for_test(&mut self, bank: Arc<Bank>) {
        self.set_bank(BankWithScheduler::new_without_scheduler(bank))
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear_bank_for_test(&mut self) {
        self.clear_bank(true);
    }

    pub fn tick_alpenglow(&mut self, slot_max_tick_height: u64) {
        let (poh_entry, tick_lock_contention_us) = measure_us!({
            let mut poh_l = self.poh.lock().unwrap();
            poh_l.tick()
        });
        self.metrics.tick_lock_contention_us += tick_lock_contention_us;

        if let Some(poh_entry) = poh_entry {
            self.shared_leader_state
                .0
                .load()
                .tick_height
                .store(slot_max_tick_height, Ordering::Release);

            // Should be empty in most cases, but reset just to be safe
            self.tick_cache = vec![];
            self.tick_cache.push((
                Entry {
                    num_hashes: poh_entry.num_hashes,
                    hash: poh_entry.hash,
                    transactions: vec![],
                },
                self.shared_leader_state
                    .0
                    .load()
                    .tick_height
                    .load(Ordering::Acquire),
            ));

            let (_flush_res, flush_cache_and_tick_us) = measure_us!(self.flush_cache(true));
            self.metrics.flush_cache_tick_us += flush_cache_and_tick_us;
        }
    }

    pub fn enable_alpenglow(&mut self) {
        info!("Enabling Alpenglow, migrating poh to low power mode");
        self.alpenglow_enabled = true;
        self.tick_cache = vec![];
        {
            let mut poh = self.poh.lock().unwrap();
            let hashes_per_tick = None;
            let current_hash = poh.hash;
            poh.reset(current_hash, hashes_per_tick);
        }
    }
}

#[allow(clippy::type_complexity)]
fn do_create_test_recorder(
    bank: Arc<Bank>,
    blockstore: Arc<Blockstore>,
    poh_config: Option<PohConfig>,
    leader_schedule_cache: Option<Arc<LeaderScheduleCache>>,
    track_transaction_indexes: bool,
) -> (
    Arc<AtomicBool>,
    Arc<RwLock<PohRecorder>>,
    PohController,
    TransactionRecorder,
    PohService,
    Receiver<WorkingBankEntry>,
) {
    let leader_schedule_cache = match leader_schedule_cache {
        Some(provided_cache) => provided_cache,
        None => Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
    };
    let exit = Arc::new(AtomicBool::new(false));
    let poh_config = poh_config.unwrap_or_default();
    let (poh_recorder, entry_receiver) = PohRecorder::new(
        bank.tick_height(),
        bank.last_blockhash(),
        bank.clone(),
        Some((4, 4)),
        bank.ticks_per_slot(),
        blockstore,
        &leader_schedule_cache,
        &poh_config,
        exit.clone(),
    );
    let ticks_per_slot = bank.ticks_per_slot();

    let (record_sender, record_receiver) = record_channels(track_transaction_indexes);
    let transaction_recorder = TransactionRecorder::new(record_sender);
    let poh_recorder = Arc::new(RwLock::new(poh_recorder));
    let (mut poh_controller, poh_service_message_receiver) = PohController::new();
    let poh_service = PohService::new(
        poh_recorder.clone(),
        &poh_config,
        exit.clone(),
        ticks_per_slot,
        crate::poh_service::DEFAULT_PINNED_CPU_CORE,
        crate::poh_service::DEFAULT_HASHES_PER_BATCH,
        record_receiver,
        poh_service_message_receiver,
    );

    poh_controller
        .set_bank_sync(BankWithScheduler::new_without_scheduler(bank))
        .unwrap();

    (
        exit,
        poh_recorder,
        poh_controller,
        transaction_recorder,
        poh_service,
        entry_receiver,
    )
}

#[allow(clippy::type_complexity)]
pub fn create_test_recorder(
    bank: Arc<Bank>,
    blockstore: Arc<Blockstore>,
    poh_config: Option<PohConfig>,
    leader_schedule_cache: Option<Arc<LeaderScheduleCache>>,
) -> (
    Arc<AtomicBool>,
    Arc<RwLock<PohRecorder>>,
    PohController,
    TransactionRecorder,
    PohService,
    Receiver<WorkingBankEntry>,
) {
    do_create_test_recorder(bank, blockstore, poh_config, leader_schedule_cache, false)
}

/// A shareable leader status that can be used to
/// determine the current leader status of the
/// `PohRecorder`.
#[derive(Clone)]
pub struct SharedLeaderState(Arc<ArcSwap<LeaderState>>);

impl SharedLeaderState {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(
        tick_height: u64,
        leader_first_tick_height: Option<u64>,
        next_leader_slot_range: Option<(Slot, Slot)>,
    ) -> Self {
        let inner = LeaderState {
            working_bank: None,
            tick_height: AtomicU64::new(tick_height),
            leader_first_tick_height,
            next_leader_slot_range,
        };
        Self(Arc::new(ArcSwap::from_pointee(inner)))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<LeaderState>> {
        self.0.load()
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn store(&mut self, state: Arc<LeaderState>) {
        self.0.store(state)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn increment_tick_height(&self) {
        let inner = self.0.load();
        inner.tick_height.fetch_add(1, Ordering::Release);
    }
}

pub struct LeaderState {
    working_bank: Option<Arc<Bank>>,
    tick_height: AtomicU64,
    leader_first_tick_height: Option<u64>,
    next_leader_slot_range: Option<(Slot, Slot)>,
}

impl LeaderState {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(
        working_bank: Option<Arc<Bank>>,
        tick_height: u64,
        leader_first_tick_height: Option<u64>,
        next_leader_slot_range: Option<(u64, u64)>,
    ) -> Self {
        Self {
            working_bank,
            tick_height: AtomicU64::new(tick_height),
            leader_first_tick_height,
            next_leader_slot_range,
        }
    }

    pub fn working_bank(&self) -> Option<&Arc<Bank>> {
        self.working_bank.as_ref()
    }

    pub fn tick_height(&self) -> u64 {
        self.tick_height.load(Ordering::Acquire)
    }

    pub fn leader_first_tick_height(&self) -> Option<u64> {
        self.leader_first_tick_height
    }

    /// Returns [first_slot, last_slot] inclusive range for the next
    /// leader slots.
    pub fn next_leader_slot_range(&self) -> Option<(Slot, Slot)> {
        self.next_leader_slot_range
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::bounded,
        solana_clock::DEFAULT_TICKS_PER_SLOT,
        solana_ledger::{
            blockstore::Blockstore,
            blockstore_meta::SlotMeta,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            get_tmp_ledger_path_auto_delete,
        },
        solana_perf::test_tx::test_tx,
        solana_sha256_hasher::hash,
    };

    #[test]
    fn test_poh_recorder_no_zero_tick() {
        let prev_hash = Hash::default();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank,
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 1);
        assert_eq!(poh_recorder.tick_cache[0].1, 1);
        assert_eq!(poh_recorder.tick_height(), 1);
    }

    #[test]
    fn test_poh_recorder_tick_height_is_last_tick() {
        let prev_hash = Hash::default();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank,
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 2);
        assert_eq!(poh_recorder.tick_cache[1].1, 2);
        assert_eq!(poh_recorder.tick_height(), 2);
    }

    #[test]
    fn test_poh_recorder_reset_clears_cache() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            Hash::default(),
            bank0.clone(),
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 1);
        poh_recorder.reset(bank0, Some((4, 4)));
        assert_eq!(poh_recorder.tick_cache.len(), 0);
    }

    #[test]
    fn test_poh_recorder_clear() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        poh_recorder.set_bank_for_test(bank);
        assert!(poh_recorder.working_bank.is_some());
        poh_recorder.clear_bank(true);
        assert!(poh_recorder.working_bank.is_none());
    }

    #[test]
    fn test_poh_recorder_tick_sent_after_min() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank0.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank0.clone(),
            Some((4, 4)),
            bank0.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank0)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        bank0.fill_bank_with_ticks_for_tests();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));

        // Set a working bank
        poh_recorder.set_bank_for_test(bank1.clone());

        // Tick until poh_recorder.tick_height == working bank's min_tick_height
        let num_new_ticks = bank1.tick_height() - poh_recorder.tick_height();
        println!("{} {}", bank1.tick_height(), poh_recorder.tick_height());
        assert!(num_new_ticks > 0);
        for _ in 0..num_new_ticks {
            poh_recorder.tick();
        }

        // Check that poh_recorder.tick_height == working bank's min_tick_height
        let min_tick_height = poh_recorder.working_bank.as_ref().unwrap().min_tick_height;
        assert_eq!(min_tick_height, bank1.tick_height());
        assert_eq!(poh_recorder.tick_height(), min_tick_height);

        //poh_recorder.tick height == working bank's min_tick_height,
        // so no ticks should have been flushed yet
        assert_eq!(poh_recorder.tick_cache.last().unwrap().1, num_new_ticks);
        assert!(entry_receiver.try_recv().is_err());

        // all ticks are sent after height > min
        let tick_height_before = poh_recorder.tick_height();
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_height(), tick_height_before + 1);
        assert_eq!(poh_recorder.tick_cache.len(), 0);
        let mut num_entries = 0;
        while let Ok((wbank, (_entry, _tick_height))) = entry_receiver.try_recv() {
            assert_eq!(wbank.slot(), bank1.slot());
            num_entries += 1;
        }

        // All the cached ticks, plus the new tick above should have been flushed
        assert_eq!(num_entries, num_new_ticks + 1);
    }

    #[test]
    fn test_poh_recorder_tick_sent_upto_and_including_max() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        // Tick further than the bank's max height
        for _ in 0..bank.max_tick_height() + 1 {
            poh_recorder.tick();
        }
        assert_eq!(
            poh_recorder.tick_cache.last().unwrap().1,
            bank.max_tick_height() + 1
        );
        assert_eq!(poh_recorder.tick_height(), bank.max_tick_height() + 1);

        poh_recorder.set_bank_for_test(bank.clone());
        poh_recorder.tick();

        assert_eq!(poh_recorder.tick_height(), bank.max_tick_height() + 2);
        assert!(poh_recorder.working_bank.is_none());
        let mut num_entries = 0;
        while entry_receiver.try_recv().is_ok() {
            num_entries += 1;
        }

        // Should only flush up to bank's max tick height, despite the tick cache
        // having many more entries
        assert_eq!(num_entries, bank.max_tick_height());
    }

    #[test]
    fn test_poh_recorder_record_to_early() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank0.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank0.clone(),
            Some((4, 4)),
            bank0.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank0)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        bank0.fill_bank_with_ticks_for_tests();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        poh_recorder.set_bank_for_test(bank1.clone());
        // Let poh_recorder tick up to bank1.tick_height() - 1
        for _ in 0..bank1.tick_height() - 1 {
            poh_recorder.tick()
        }
        let tx = test_tx();
        let h1 = hash(b"hello world!");

        // We haven't yet reached the minimum tick height for the working bank,
        // so record should fail
        assert_matches!(
            poh_recorder.record(bank1.slot(), vec![h1], vec![vec![tx.into()]]),
            Err(PohRecorderError::MinHeightNotReached)
        );
        assert!(entry_receiver.try_recv().is_err());
    }

    #[test]
    fn test_poh_recorder_record_bad_slot() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        poh_recorder.set_bank_for_test(bank.clone());
        let tx = test_tx();
        let h1 = hash(b"hello world!");

        // Fulfills min height criteria for a successful record
        assert_eq!(
            poh_recorder.tick_height(),
            poh_recorder.working_bank.as_ref().unwrap().min_tick_height
        );

        // However we hand over a bad slot so record fails
        let bad_slot = bank.slot() + 1;
        assert_matches!(
            poh_recorder.record(bad_slot, vec![h1], vec![vec![tx.into()]]),
            Err(PohRecorderError::MaxHeightReached)
        );
    }

    #[test]
    fn test_poh_recorder_record_at_min_passes() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank0.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank0.clone(),
            Some((4, 4)),
            bank0.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank0)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        bank0.fill_bank_with_ticks_for_tests();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        poh_recorder.set_bank_for_test(bank1.clone());

        // Record up to exactly min tick height
        let min_tick_height = poh_recorder.working_bank.as_ref().unwrap().min_tick_height;
        while poh_recorder.tick_height() < min_tick_height {
            poh_recorder.tick();
        }

        assert_eq!(poh_recorder.tick_cache.len() as u64, min_tick_height);

        // Check record succeeds on boundary condition where
        // poh_recorder.tick height == poh_recorder.working_bank.min_tick_height
        assert_eq!(poh_recorder.tick_height(), min_tick_height);
        let tx = test_tx();
        let h1 = hash(b"hello world!");
        assert!(poh_recorder
            .record(bank1.slot(), vec![h1], vec![vec![tx.into()]])
            .is_ok());
        assert_eq!(poh_recorder.tick_cache.len(), 0);

        //tick in the cache + entry
        for _ in 0..min_tick_height {
            let (_bank, (e, _tick_height)) = entry_receiver.recv().unwrap();
            assert!(e.is_tick());
        }

        let (_bank, (e, _tick_height)) = entry_receiver.recv().unwrap();
        assert!(!e.is_tick());
    }

    #[test]
    fn test_poh_recorder_record_at_max_fails() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        poh_recorder.set_bank_for_test(bank.clone());
        let num_ticks_to_max = bank.max_tick_height() - poh_recorder.tick_height();
        for _ in 0..num_ticks_to_max {
            poh_recorder.tick();
        }
        let tx = test_tx();
        let h1 = hash(b"hello world!");
        assert!(poh_recorder
            .record(bank.slot(), vec![h1], vec![vec![tx.into()]])
            .is_err());
        for _ in 0..num_ticks_to_max {
            let (_bank, (entry, _tick_height)) = entry_receiver.recv().unwrap();
            assert!(entry.is_tick());
        }
    }

    #[test]
    fn test_poh_cache_on_disconnect() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank0.last_blockhash();
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank0.clone(),
            Some((4, 4)),
            bank0.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank0)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        bank0.fill_bank_with_ticks_for_tests();
        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        poh_recorder.set_bank_for_test(bank1);

        // Check we can make two ticks without hitting min_tick_height
        let remaining_ticks_to_min = poh_recorder.working_bank.as_ref().unwrap().min_tick_height
            - poh_recorder.tick_height();
        for _ in 0..remaining_ticks_to_min {
            poh_recorder.tick();
        }
        assert_eq!(poh_recorder.tick_height(), remaining_ticks_to_min);
        assert_eq!(
            poh_recorder.tick_cache.len(),
            remaining_ticks_to_min as usize
        );
        assert!(poh_recorder.working_bank.is_some());

        // Drop entry receiver, and try to tick again. Because
        // the receiver is closed, the ticks will not be drained from the cache,
        // and the working bank will be cleared
        drop(entry_receiver);
        poh_recorder.tick();

        // Check everything is cleared
        assert!(poh_recorder.working_bank.is_none());
        // Extra +1 for the tick that happened after the drop of the entry receiver.
        assert_eq!(
            poh_recorder.tick_cache.len(),
            remaining_ticks_to_min as usize + 1
        );
    }

    #[test]
    fn test_reset_current() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            Hash::default(),
            bank.clone(),
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 2);
        poh_recorder.reset(bank, Some((4, 4)));
        assert_eq!(poh_recorder.tick_cache.len(), 0);
    }

    #[test]
    fn test_reset_with_cached() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            Hash::default(),
            bank.clone(),
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 2);
        poh_recorder.reset(bank, Some((4, 4)));
        assert_eq!(poh_recorder.tick_cache.len(), 0);
    }

    #[test]
    fn test_reset_to_new_value() {
        agave_logger::setup();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            Hash::default(),
            bank.clone(),
            Some((4, 4)),
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.tick();
        poh_recorder.tick();
        poh_recorder.tick();
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_cache.len(), 4);
        assert_eq!(poh_recorder.tick_height(), 4);
        poh_recorder.reset(bank, Some((4, 4))); // parent slot 0 implies tick_height of 3
        assert_eq!(poh_recorder.tick_cache.len(), 0);
        poh_recorder.tick();
        assert_eq!(poh_recorder.tick_height(), DEFAULT_TICKS_PER_SLOT + 1);
    }

    #[test]
    fn test_reset_clear_bank() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            Hash::default(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        poh_recorder.set_bank_for_test(bank.clone());
        assert_eq!(bank.slot(), 0);
        poh_recorder.reset(bank, Some((4, 4)));
        assert!(poh_recorder.working_bank.is_none());
    }

    #[test]
    pub fn test_clear_signal() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (sender, receiver) = bounded(1);
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new_with_clear_signal(
            0,
            Hash::default(),
            bank.clone(),
            None,
            bank.ticks_per_slot(),
            false,
            Arc::new(blockstore),
            Some(sender),
            &Arc::new(LeaderScheduleCache::default()),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        poh_recorder.set_bank_for_test(bank);
        poh_recorder.clear_bank(true);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn test_poh_recorder_record_sets_start_slot() {
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let ticks_per_slot = 5;
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.ticks_per_slot = ticks_per_slot;
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        poh_recorder.set_bank_for_test(bank.clone());

        // Simulate ticking much further than working_bank.max_tick_height
        let max_tick_height = poh_recorder.working_bank.as_ref().unwrap().max_tick_height;
        for _ in 0..3 * max_tick_height {
            poh_recorder.tick();
        }

        let tx = test_tx();
        let h1 = hash(b"hello world!");
        assert!(poh_recorder
            .record(bank.slot(), vec![h1], vec![vec![tx.into()]])
            .is_err());
        assert!(poh_recorder.working_bank.is_none());

        // Even thought we ticked much further than working_bank.max_tick_height,
        // the `start_slot` is still the slot of the last working bank set by
        // the earlier call to `poh_recorder.set_bank()`
        assert_eq!(poh_recorder.start_slot(), bank.slot());
    }

    #[test]
    fn test_current_poh_slot() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let last_entry_hash = bank.last_blockhash();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            last_entry_hash,
            bank.clone(),
            None,
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(leader_schedule_cache),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        // Tick height is initialized as 0
        assert_eq!(0, poh_recorder.current_poh_slot());

        // Tick height will be reset to the last tick of the reset bank
        poh_recorder.reset(bank.clone(), None);
        assert_eq!(bank.slot() + 1, poh_recorder.current_poh_slot());

        // Check that any ticks before the last tick of the current poh slot will
        // not cause the current poh slot to advance
        for _ in 0..bank.ticks_per_slot() - 1 {
            poh_recorder.tick();
            assert_eq!(bank.slot() + 1, poh_recorder.current_poh_slot());
        }

        // Check that the current poh slot is advanced once the last tick of the
        // slot is reached
        poh_recorder.tick();
        assert_eq!(bank.slot() + 2, poh_recorder.current_poh_slot());
    }

    #[test]
    fn test_reached_leader_tick() {
        agave_logger::setup();

        // Setup genesis.
        let GenesisConfigInfo {
            genesis_config,
            validator_pubkey,
            ..
        } = create_genesis_config(2);

        // Setup start bank.
        let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();

        // Setup leader schedule.
        let leader_a_pubkey = validator_pubkey;
        let leader_b_pubkey = Pubkey::new_unique();
        let leader_c_pubkey = Pubkey::new_unique();
        let leader_d_pubkey = Pubkey::new_unique();
        let consecutive_leader_slots = NUM_CONSECUTIVE_LEADER_SLOTS as usize;
        let mut slot_leaders = Vec::with_capacity(consecutive_leader_slots * 3);
        slot_leaders.extend(std::iter::repeat_n(
            leader_a_pubkey,
            consecutive_leader_slots,
        ));
        slot_leaders.extend(std::iter::repeat_n(
            leader_b_pubkey,
            consecutive_leader_slots,
        ));
        slot_leaders.extend(std::iter::repeat_n(
            leader_c_pubkey,
            consecutive_leader_slots,
        ));
        slot_leaders.extend(std::iter::repeat_n(
            leader_d_pubkey,
            consecutive_leader_slots,
        ));
        let mut leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let fixed_schedule = solana_ledger::leader_schedule::FixedSchedule {
            leader_schedule: Arc::new(Box::new(
                solana_ledger::leader_schedule::IdentityKeyedLeaderSchedule::new_from_schedule(
                    slot_leaders,
                ),
            )),
        };
        leader_schedule_cache.set_fixed_leader_schedule(Some(fixed_schedule));

        // Setup PoH recorder.
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            None,
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(leader_schedule_cache),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let ticks_per_slot = bank.ticks_per_slot();
        let grace_ticks = ticks_per_slot * MAX_GRACE_SLOTS;
        poh_recorder.grace_ticks = grace_ticks;

        // Setup leader slot ranges.
        let leader_a_start_slot = 0;
        let leader_a_end_slot = leader_a_start_slot + NUM_CONSECUTIVE_LEADER_SLOTS - 1;
        let leader_b_start_slot = leader_a_end_slot + 1;
        let leader_b_end_slot = leader_b_start_slot + NUM_CONSECUTIVE_LEADER_SLOTS - 1;
        let leader_c_start_slot = leader_b_end_slot + 1;
        let leader_c_end_slot = leader_c_start_slot + NUM_CONSECUTIVE_LEADER_SLOTS - 1;
        let leader_d_start_slot = leader_c_end_slot + 1;
        let leader_d_end_slot = leader_d_start_slot + NUM_CONSECUTIVE_LEADER_SLOTS - 1;

        // Reset onto Leader A's first slot 0.
        poh_recorder.reset(
            bank.clone(),
            Some((leader_a_start_slot + 1, leader_a_end_slot)),
        );

        // Setup leader start ticks.
        let ticks_in_leader_slot_set = ticks_per_slot * NUM_CONSECUTIVE_LEADER_SLOTS;
        let leader_a_start_tick = 1;
        let leader_b_start_tick = leader_a_start_tick + ticks_in_leader_slot_set;
        let leader_c_start_tick = leader_b_start_tick + ticks_in_leader_slot_set;
        let leader_d_start_tick = leader_c_start_tick + ticks_in_leader_slot_set;

        // True, because from Leader A's perspective, the previous slot was also
        // its own slot, and validators don't give grace periods if previous
        // slot was also their own.
        assert!(poh_recorder.reached_leader_tick(&leader_a_pubkey, leader_a_start_tick));

        // Tick through grace ticks.
        for _ in 0..grace_ticks {
            poh_recorder.tick();
        }

        // True, because we have ticked through all the grace ticks.
        assert!(poh_recorder.reached_leader_tick(&leader_a_pubkey, leader_a_start_tick));

        // Reset PoH on Leader A's first slot 0, ticking towards Leader B's leader slots.
        poh_recorder.reset(bank.clone(), Some((leader_b_start_slot, leader_b_end_slot)));

        // False, because Leader B hasn't ticked to its starting slot yet.
        assert!(!poh_recorder.reached_leader_tick(&leader_b_pubkey, leader_b_start_tick));

        // Tick through Leader A's remaining slots.
        for _ in poh_recorder.tick_height()..ticks_in_leader_slot_set {
            poh_recorder.tick();
        }

        // False, because the PoH was reset on slot 0, which is a block produced
        // by previous leader A, so a grace period must be given.
        assert!(!poh_recorder.reached_leader_tick(&leader_b_pubkey, leader_b_start_tick));

        // Reset onto Leader A's last slot.
        for _ in leader_a_start_slot + 1..leader_b_start_slot {
            let child_slot = bank.slot() + 1;
            bank = Arc::new(Bank::new_from_parent(bank, &leader_a_pubkey, child_slot));
        }
        poh_recorder.reset(bank.clone(), Some((leader_b_start_slot, leader_b_end_slot)));

        // True, because the PoH was reset the last slot produced by the
        // previous leader, so we can run immediately.
        assert!(poh_recorder.reached_leader_tick(&leader_b_pubkey, leader_b_start_tick));

        // Simulate skipping Leader B's first slot.
        poh_recorder.reset(
            bank.clone(),
            Some((leader_b_start_slot + 1, leader_b_end_slot)),
        );
        for _ in 0..ticks_per_slot {
            poh_recorder.tick();
        }

        // True, because we're building off the previous leader A's last block.
        assert!(poh_recorder.reached_leader_tick(&leader_b_pubkey, leader_b_start_tick));

        // Simulate generating Leader B's second slot.
        let child_slot = bank.slot() + 1;
        bank = Arc::new(Bank::new_from_parent(bank, &leader_b_pubkey, child_slot));

        // Reset PoH targeting Leader D's slots.
        poh_recorder.reset(bank, Some((leader_d_start_slot, leader_d_end_slot)));

        // Tick through Leader B's remaining slots.
        for _ in ticks_per_slot..ticks_in_leader_slot_set {
            poh_recorder.tick();
        }

        // Tick through Leader C's slots.
        for _ in 0..ticks_in_leader_slot_set {
            poh_recorder.tick();
        }

        // True, because Leader D is not building on any of Leader C's slots.
        // The PoH was last reset onto Leader B's second slot.
        assert!(poh_recorder.reached_leader_tick(&leader_d_pubkey, leader_d_start_tick));

        // Add some active (partially received) blocks to the active fork.
        let active_descendants = vec![NUM_CONSECUTIVE_LEADER_SLOTS];
        poh_recorder.update_start_bank_active_descendants(&active_descendants);

        // True, because Leader D observes pending blocks on the active fork,
        // but the config to delay for these is not set.
        assert!(poh_recorder.reached_leader_tick(&leader_d_pubkey, leader_d_start_tick));

        // Flip the config to delay for pending blocks.
        poh_recorder.delay_leader_block_for_pending_fork = true;

        // False, because Leader D observes pending blocks on the active fork,
        // and the config to delay for these is set.
        assert!(!poh_recorder.reached_leader_tick(&leader_d_pubkey, leader_d_start_tick));
    }

    #[test]
    fn test_reached_leader_slot() {
        agave_logger::setup();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        let GenesisConfigInfo {
            genesis_config,
            validator_pubkey,
            ..
        } = create_genesis_config(2);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank0.last_blockhash();
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank0.clone(),
            None,
            bank0.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank0)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        // Test that with no next leader slot, we don't reach the leader slot
        assert_eq!(
            poh_recorder.reached_leader_slot(&validator_pubkey),
            PohLeaderStatus::NotReached
        );

        // Test that with no next leader slot in reset(), we don't reach the leader slot
        assert_eq!(bank0.slot(), 0);
        poh_recorder.reset(bank0.clone(), None);
        assert_eq!(
            poh_recorder.reached_leader_slot(&validator_pubkey),
            PohLeaderStatus::NotReached
        );

        // Provide a leader slot one slot down
        poh_recorder.reset(bank0.clone(), Some((2, 2)));

        let init_ticks = poh_recorder.tick_height();

        // Send one slot worth of ticks
        for _ in 0..bank0.ticks_per_slot() {
            poh_recorder.tick();
        }

        // Tick should be recorded
        assert_eq!(
            poh_recorder.tick_height(),
            init_ticks + bank0.ticks_per_slot()
        );

        let parent_meta = SlotMeta {
            received: 1,
            ..SlotMeta::default()
        };
        poh_recorder.blockstore.put_meta(0, &parent_meta).unwrap();

        // Use a key that's different from the previous leader so that grace
        // ticks are enforced.
        let test_validator_pubkey = Pubkey::new_unique();

        // Test that we don't reach the leader slot because of grace ticks
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::NotReached
        );

        // reset poh now. we should immediately be leader
        let bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
        assert_eq!(bank1.slot(), 1);
        poh_recorder.reset(bank1.clone(), Some((2, 2)));
        assert_eq!(
            poh_recorder.reached_leader_slot(&validator_pubkey),
            PohLeaderStatus::Reached {
                poh_slot: 2,
                parent_slot: 1,
            }
        );

        // Now test that with grace ticks we can reach leader slot
        // Set the leader slot one slot down
        poh_recorder.reset(bank1.clone(), Some((3, 3)));

        // Send one slot worth of ticks ("skips" slot 2)
        for _ in 0..bank1.ticks_per_slot() {
            poh_recorder.tick();
        }

        // We are not the leader yet, as expected
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::NotReached
        );
        // Check that if prev slot was mine, grace ticks are ignored
        assert_eq!(
            poh_recorder.reached_leader_slot(bank1.collector_id()),
            PohLeaderStatus::Reached {
                poh_slot: 3,
                parent_slot: 1
            }
        );

        // Send the grace ticks
        for _ in 0..bank1.ticks_per_slot() / GRACE_TICKS_FACTOR {
            poh_recorder.tick();
        }

        // We should be the leader now
        // without sending more ticks, we should be leader now
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::Reached {
                poh_slot: 3,
                parent_slot: 1,
            }
        );

        // Let's test that correct grace ticks are reported
        // Set the leader slot one slot down
        let bank2 = Arc::new(Bank::new_from_parent(bank1.clone(), &Pubkey::default(), 2));
        poh_recorder.reset(bank2.clone(), Some((4, 4)));

        // send ticks for a slot
        for _ in 0..bank1.ticks_per_slot() {
            poh_recorder.tick();
        }

        // We are not the leader yet, as expected
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::NotReached
        );
        let bank3 = Arc::new(Bank::new_from_parent(bank2, &Pubkey::default(), 3));
        assert_eq!(bank3.slot(), 3);
        poh_recorder.reset(bank3.clone(), Some((4, 4)));

        // without sending more ticks, we should be leader now
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::Reached {
                poh_slot: 4,
                parent_slot: 3,
            }
        );

        // Let's test that if a node overshoots the ticks for its target
        // leader slot, reached_leader_slot() will return true, because it's overdue
        // Set the leader slot one slot down
        let bank4 = Arc::new(Bank::new_from_parent(bank3, &Pubkey::default(), 4));
        poh_recorder.reset(bank4.clone(), Some((5, 5)));

        // Overshoot ticks for the slot
        let overshoot_factor = 4;
        for _ in 0..overshoot_factor * bank4.ticks_per_slot() {
            poh_recorder.tick();
        }

        // We are overdue to lead
        assert_eq!(
            poh_recorder.reached_leader_slot(&test_validator_pubkey),
            PohLeaderStatus::Reached {
                poh_slot: 9,
                parent_slot: 4,
            }
        );

        // Test that grace ticks are not required if the previous leader's 4
        // slots got skipped.
        {
            poh_recorder.reset(bank4.clone(), Some((9, 9)));

            // Tick until leader slot
            for _ in 0..4 * bank4.ticks_per_slot() {
                poh_recorder.tick();
            }

            // We are due to lead
            assert_eq!(
                poh_recorder.reached_leader_slot(&test_validator_pubkey),
                PohLeaderStatus::Reached {
                    poh_slot: 9,
                    parent_slot: 4,
                }
            );

            // Add an active descendant which is considered to be a pending new
            // reset bank
            poh_recorder.update_start_bank_active_descendants(&[5]);
            assert!(poh_recorder.is_new_reset_bank_pending(8));

            // Without setting delay_leader_block_for_pending_fork, skip grace ticks
            assert_eq!(
                poh_recorder.reached_leader_slot(&test_validator_pubkey),
                PohLeaderStatus::Reached {
                    poh_slot: 9,
                    parent_slot: 4,
                }
            );

            // After setting delay_leader_block_for_pending_fork, grace ticks are required
            poh_recorder.delay_leader_block_for_pending_fork = true;
            assert_eq!(
                poh_recorder.reached_leader_slot(&test_validator_pubkey),
                PohLeaderStatus::NotReached,
            );

            // Tick through grace ticks
            for _ in 0..poh_recorder.grace_ticks {
                poh_recorder.tick();
            }

            // After grace ticks, we are due to lead
            assert_eq!(
                poh_recorder.reached_leader_slot(&test_validator_pubkey),
                PohLeaderStatus::Reached {
                    poh_slot: 9,
                    parent_slot: 4,
                }
            );
        }
    }

    #[test]
    fn test_would_be_leader_soon() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let prev_hash = bank.last_blockhash();
        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            prev_hash,
            bank.clone(),
            None,
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        // Test that with no leader slot, we don't reach the leader tick
        assert!(!poh_recorder.would_be_leader(2 * bank.ticks_per_slot()));

        assert_eq!(bank.slot(), 0);
        poh_recorder.reset(bank.clone(), None);

        assert!(!poh_recorder.would_be_leader(2 * bank.ticks_per_slot()));

        // We reset with leader slot after 3 slots
        let bank_slot = bank.slot() + 3;
        poh_recorder.reset(bank.clone(), Some((bank_slot, bank_slot)));

        // Test that the node won't be leader in next 2 slots
        assert!(!poh_recorder.would_be_leader(2 * bank.ticks_per_slot()));

        // Test that the node will be leader in next 3 slots
        assert!(poh_recorder.would_be_leader(3 * bank.ticks_per_slot()));

        assert!(!poh_recorder.would_be_leader(2 * bank.ticks_per_slot()));

        // Move the bank up a slot (so that max_tick_height > slot 0's tick_height)
        let bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), 1));
        // If we set the working bank, the node should be leader within next 2 slots
        poh_recorder.set_bank_for_test(bank.clone());
        assert!(poh_recorder.would_be_leader(2 * bank.ticks_per_slot()));
    }

    #[test]
    fn test_flush_virtual_ticks() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        // test that virtual ticks are flushed into a newly set bank asap
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let genesis_hash = bank.last_blockhash();

        let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
            0,
            bank.last_blockhash(),
            bank.clone(),
            Some((2, 2)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        //create a new bank
        let bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), 2));
        // add virtual ticks into poh for slots 0, 1, and 2
        for _ in 0..(bank.ticks_per_slot() * 3) {
            poh_recorder.tick();
        }
        poh_recorder.set_bank_for_test(bank.clone());
        assert!(!bank.is_hash_valid_for_age(&genesis_hash, 0));
        assert!(bank.is_hash_valid_for_age(&genesis_hash, 1));
    }

    #[test]
    fn test_compute_leader_slot_tick_heights() {
        assert_eq!(
            PohRecorder::compute_leader_slot_tick_heights(None, 0),
            (None, 0, 0)
        );

        assert_eq!(
            PohRecorder::compute_leader_slot_tick_heights(Some((4, 4)), 8),
            (Some(33), 40, 4)
        );

        assert_eq!(
            PohRecorder::compute_leader_slot_tick_heights(Some((4, 7)), 8),
            (Some(33), 64, 2 * 8)
        );

        assert_eq!(
            PohRecorder::compute_leader_slot_tick_heights(Some((6, 7)), 8),
            (Some(49), 64, 8)
        );

        assert_eq!(
            PohRecorder::compute_leader_slot_tick_heights(Some((6, 7)), 4),
            (Some(25), 32, 4)
        );
    }
}
