//! The `poh_recorder` module provides an object for synchronizing with Proof of History.
//! It synchronizes PoH, bank's register_tick and the ledger
//!
//! `PohRecorder` sends transaction entries and the final Alpenglow tick to the current working
//! bank.
//!
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    agave_votor_messages::reward_certificate::BuildRewardCertsRespError,
    arc_swap::ArcSwap,
    crossbeam_channel::{Receiver, SendError, Sender, TrySendError, bounded},
    log::*,
    solana_clock::Slot,
    solana_entry::{
        block_component::{BlockFooterV1, VersionedBlockMarker},
        entry::Entry,
        poh::Poh,
        recorder_message::RecorderMessage,
    },
    solana_hash::Hash,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_measure::measure_us,
    solana_poh_config::PohConfig,
    solana_runtime::{
        bank::{Bank, BankId},
        block_component_processor::BankFooterError,
        installed_scheduler_pool::BankWithScheduler,
        validated_reward_certificate::Error as ValidatedRewardCertError,
    },
    solana_transaction::versioned::VersionedTransaction,
    std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
    thiserror::Error,
};

pub const MAX_LEADER_SLOT_RANGE: u64 = 4;

/// Maximum number of messages waiting to be consumed by the broadcast pipeline.
///
/// Sending is intentionally blocking: once a record has been mixed into PoH, its message must not
/// be dropped. This capacity absorbs transient downstream stalls;
/// a sustained stall applies backpressure instead of growing memory without bound.
pub const WORKING_BANK_CHANNEL_CAPACITY: usize = 2048;

/// Try the nonblocking fast path first so saturation is observable, then block
/// rather than dropping a message that has already been mixed into PoH.
fn send_working_bank_message(
    sender: &Sender<WorkingBankMessage>,
    message: WorkingBankMessage,
) -> std::result::Result<(), Box<SendError<WorkingBankMessage>>> {
    match sender.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(message)) => {
            error!(
                "PohRecorder output channel is full (capacity {WORKING_BANK_CHANNEL_CAPACITY}); \
                 blocking to preserve the message"
            );
            sender.send(message).map_err(Box::new)
        }
        Err(TrySendError::Disconnected(message)) => Err(Box::new(SendError(message))),
    }
}

#[derive(Debug, Error)]
pub enum PohRecorderError {
    #[error("max height reached")]
    MaxHeightReached,

    #[error("send WorkingBankMessage error")]
    SendError(#[from] Box<SendError<WorkingBankMessage>>),

    #[error("channel full")]
    ChannelFull,

    #[error("channel disconnected")]
    ChannelDisconnected,

    #[error("producing reward certs failed with {0}")]
    BuildRewardCerts(#[from] BuildRewardCertsRespError),

    #[error("constructing validated reward cert failed with {0}")]
    ValidatedRewardCert(#[from] ValidatedRewardCertError),

    #[error("updating bank footer failed with \"{0}\"")]
    UpdateBankFooter(#[from] BankFooterError),

    #[error("bank {0} did not freeze before block footer timeout")]
    BankFreezeTimeout(Slot),

    #[error("couldn't reset bank during fast leader handover slot {0} -> slot {1}")]
    ResetBankError(Slot, Slot),

    #[error("couldn't reschedule pre-UpdateParent transactions for slot {0}")]
    RescheduleTransactionsError(Slot),

    #[error("leader window moved past slot {0}")]
    WindowMovedOn(Slot),
}

pub(crate) type Result<T> = std::result::Result<T, PohRecorderError>;

pub type WorkingBankMessage = (Arc<Bank>, (RecorderMessage, u64));

pub struct Record {
    pub mixin: Hash,
    pub transactions: Vec<VersionedTransaction>,
    pub bank_id: BankId,
}

impl Record {
    pub fn new(mixin: Hash, transactions: Vec<VersionedTransaction>, bank_id: BankId) -> Self {
        Self {
            mixin,
            transactions,
            bank_id,
        }
    }
}

struct WorkingBank {
    bank: BankWithScheduler,
    start: Arc<Instant>,
}

struct PohRecorderMetrics {
    record_us: u64,
    record_lock_contention_us: u64,
    report_metrics_us: u64,
    send_entry_us: u64,
    tick_lock_contention_us: u64,
    last_metric: Instant,
}

impl Default for PohRecorderMetrics {
    fn default() -> Self {
        Self {
            record_us: 0,
            record_lock_contention_us: 0,
            report_metrics_us: 0,
            send_entry_us: 0,
            tick_lock_contention_us: 0,
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
                ("record_us", self.record_us, i64),
                (
                    "record_lock_contention_us",
                    self.record_lock_contention_us,
                    i64
                ),
                ("report_metrics_us", self.report_metrics_us, i64),
                ("send_entry_us", self.send_entry_us, i64),
                ("tick_lock_contention", self.tick_lock_contention_us, i64),
            );

            *self = Self::default();
        }
    }
}

pub struct PohRecorder {
    poh: Mutex<Poh>,
    clear_bank_signal: Option<Sender<bool>>,
    start_bank: Arc<Bank>, // parent slot
    /// This stores the current working bank + scheduler and other metadata,
    /// if they exist.
    /// This field MUST be kept consistent with the `shared_leader_state` field.
    working_bank: Option<WorkingBank>,
    shared_leader_state: SharedLeaderState,
    working_bank_sender: Sender<WorkingBankMessage>,
    blockstore: Arc<Blockstore>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    ticks_per_slot: u64,
    metrics: PohRecorderMetrics,
    is_exited: Arc<AtomicBool>,
}

impl PohRecorder {
    /// A recorder to synchronize PoH with the following data structures
    /// * bank - the LastId's queue is updated on `tick` and `record` events
    #[cfg(feature = "dev-context-only-utils")]
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(
        tick_height: u64,
        last_entry_hash: Hash,
        start_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
        ticks_per_slot: u64,
        blockstore: Arc<Blockstore>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        poh_config: &PohConfig,
        is_exited: Arc<AtomicBool>,
    ) -> (Self, Receiver<WorkingBankMessage>) {
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
        _delay_leader_block_for_pending_fork: bool,
        blockstore: Arc<Blockstore>,
        clear_bank_signal: Option<Sender<bool>>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        _poh_config: &PohConfig,
        is_exited: Arc<AtomicBool>,
    ) -> (Self, Receiver<WorkingBankMessage>) {
        let tick_number = 0;
        let poh = Mutex::new(Poh::new_with_slot_info(last_entry_hash, None, tick_number));

        let (working_bank_sender, working_bank_receiver) = bounded(WORKING_BANK_CHANNEL_CAPACITY);
        (
            Self {
                poh,
                working_bank: None,
                shared_leader_state: SharedLeaderState::new(tick_height, next_leader_slot),
                working_bank_sender,
                clear_bank_signal,
                start_bank,
                blockstore,
                leader_schedule_cache: leader_schedule_cache.clone(),
                ticks_per_slot,
                metrics: PohRecorderMetrics::default(),
                is_exited,
            },
            working_bank_receiver,
        )
    }

    // synchronize PoH with a bank
    pub fn reset(&mut self, reset_bank: Arc<Bank>, next_leader_slot: Option<(Slot, Slot)>) {
        self.clear_bank(false);
        let tick_height = self.reset_poh(reset_bank, true);

        // Above call to `clear_bank` did not set the shared state,
        // nor did `reset_poh` update the tick_height.
        // Do the atomic swap of state here to reflect the reset.
        self.shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            tick_height,
            next_leader_slot,
        )));
    }

    /// Send the block marker to be broadcast
    pub fn send_marker(&mut self, marker: VersionedBlockMarker) -> Result<()> {
        self.send_recorder_message(RecorderMessage::Marker(marker))
    }

    fn send_recorder_message(&mut self, message: RecorderMessage) -> Result<()> {
        let tick_height = self.tick_height();
        let working_bank = self
            .working_bank
            .as_ref()
            .ok_or(PohRecorderError::MaxHeightReached)?;

        send_working_bank_message(
            &self.working_bank_sender,
            (working_bank.bank.clone(), (message, tick_height)),
        )?;

        Ok(())
    }

    pub fn record(
        &mut self,
        bank_id: BankId,
        mixin: Hash,
        transactions: Vec<VersionedTransaction>,
    ) -> Result<()> {
        // Entries without transactions are used to track real-time passing in the ledger and
        // cannot be generated by `record()`
        assert!(!transactions.is_empty(), "No transactions provided");

        if let Some(working_bank) = self.working_bank.as_ref() {
            let ((), report_metrics_us) =
                measure_us!(self.metrics.report(working_bank.bank.slot()));
            self.metrics.report_metrics_us += report_metrics_us;
        }

        let tick_height = self.tick_height();
        let working_bank = self
            .working_bank
            .as_ref()
            .ok_or(PohRecorderError::MaxHeightReached)?;
        if bank_id != working_bank.bank.bank_id() {
            return Err(PohRecorderError::MaxHeightReached);
        }

        let (mut poh, poh_lock_us) = measure_us!(self.poh.lock().unwrap());
        self.metrics.record_lock_contention_us += poh_lock_us;
        let (entry, record_mixin_us) = measure_us!(poh.record(mixin));
        self.metrics.record_us += record_mixin_us;
        let entry = entry.expect("low-power PoH must produce every recorded entry");
        drop(poh);

        let (send_entry_res, send_entry_us) = measure_us!(send_working_bank_message(
            &self.working_bank_sender,
            (
                working_bank.bank.clone(),
                (
                    Entry {
                        num_hashes: entry.num_hashes,
                        hash: entry.hash,
                        transactions,
                    }
                    .into(),
                    tick_height,
                ),
            ),
        ));
        self.metrics.send_entry_us += send_entry_us;
        send_entry_res?;
        Ok(())
    }

    /// Installs a working bank and notifies broadcast that its slot has started.
    pub fn set_bank_and_send_slot_start(&mut self, bank: BankWithScheduler) -> Result<()> {
        self.set_bank(bank);
        self.send_recorder_message(RecorderMessage::SlotStart)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn set_bank(&mut self, bank: BankWithScheduler) {
        assert!(self.working_bank.is_none());
        let working_bank = WorkingBank {
            bank,
            start: Arc::new(Instant::now()),
        };
        trace!("new working bank");
        assert_eq!(working_bank.bank.ticks_per_slot(), self.ticks_per_slot);

        let leader_state = self.shared_leader_state.load();
        let next_leader_slot = leader_state.next_leader_slot_range();
        drop(leader_state);
        self.shared_leader_state.store(Arc::new(LeaderState::new(
            Some(working_bank.bank.clone_without_scheduler()),
            self.tick_height(),
            next_leader_slot,
        )));
        self.working_bank = Some(working_bank);
    }

    fn notify_replay_wakeup(&self) {
        if let Some(signal) = &self.clear_bank_signal {
            match signal.try_send(true) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    trace!("replay wake up signal channel is full.")
                }
                Err(TrySendError::Disconnected(_)) => {
                    trace!("replay wake up signal channel is disconnected.")
                }
            }
        }
    }

    /// Clears the working bank.
    /// Updates [`Self::shared_leader_state`] if `set_shared_state` is true.
    /// Otherwise the caller is responsible for setting the state before
    /// releasing the lock.
    pub fn clear_bank(&mut self, set_shared_state: bool) {
        if let Some(WorkingBank { bank, start, .. }) = self.working_bank.take() {
            let next_leader_slot = self.leader_schedule_cache.next_leader_slot(
                bank.leader_id(),
                bank.slot(),
                &bank,
                Some(&self.blockstore),
                MAX_LEADER_SLOT_RANGE,
            );
            assert_eq!(self.ticks_per_slot, bank.ticks_per_slot());
            // Only update if `set_shared_state` is true.
            // If `false` it is the caller's responsibility to set the shared state.
            if set_shared_state {
                self.shared_leader_state.store(Arc::new(LeaderState::new(
                    None,
                    self.tick_height(),
                    next_leader_slot,
                )));
            }

            datapoint_info!(
                "leader-slot-start-to-cleared-elapsed-ms",
                ("slot", bank.slot(), i64),
                ("elapsed", start.elapsed().as_millis(), i64),
            );
        }

        self.notify_replay_wakeup();
    }

    /// Returns tick_height - does not update the internal state for tick_height.
    #[must_use]
    fn reset_poh(&mut self, reset_bank: Arc<Bank>, reset_start_bank: bool) -> u64 {
        let blockhash = reset_bank.last_blockhash();
        let poh_hash = {
            let mut poh = self.poh.lock().unwrap();
            poh.reset(blockhash, None);
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

        if reset_start_bank {
            self.start_bank = reset_bank;
        }

        (self.start_slot() + 1) * self.ticks_per_slot
    }

    /// Waits for the bank to freeze and sends the block footer with the bank hash.
    /// Returns once the footer has been handed to broadcast, or errors if the
    /// bank cannot freeze quickly enough to populate the footer's bank hash.
    fn wait_for_freeze_and_send_footer(
        &self,
        mut footer: BlockFooterV1,
        working_bank: &WorkingBank,
    ) -> Result<()> {
        // Wake replay as soon as the slot reaches max tick height so it can freeze the bank
        // before we block on the footer/bank-hash handoff.
        self.notify_replay_wakeup();

        // Wait for the bank to be frozen with timeout
        let start = Instant::now();
        let delta_block = Duration::from_nanos_u128(working_bank.bank.ns_per_slot);
        while !working_bank.bank.is_frozen() && !self.is_exited.load(Ordering::Relaxed) {
            if start.elapsed() > delta_block {
                break;
            }
            std::hint::spin_loop();
        }

        // If the bank still isn't frozen, we've timed out
        if !working_bank.bank.is_frozen() {
            if self.is_exited.load(Ordering::Relaxed) {
                return Err(PohRecorderError::ChannelDisconnected);
            }
            error!(
                "slot = {} block production failure. bank freezing timed out.",
                working_bank.bank.slot()
            );
            return Err(PohRecorderError::BankFreezeTimeout(
                working_bank.bank.slot(),
            ));
        }

        // Send out the block footer - we now have the bank hash
        footer.bank_hash = working_bank.bank.hash();

        let footer = VersionedBlockMarker::from_block_footer(footer);
        let footer_message = (
            RecorderMessage::Marker(footer),
            working_bank.bank.max_tick_height() - 1,
        );

        send_working_bank_message(
            &self.working_bank_sender,
            (working_bank.bank.clone(), footer_message),
        )
        .map_err(|err| {
            error!(
                "slot = {} block production failure. failed to broadcast footer",
                working_bank.bank.slot()
            );
            PohRecorderError::SendError(err)
        })
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

    fn tick_height(&self) -> u64 {
        self.shared_leader_state.load().tick_height()
    }

    fn start_slot(&self) -> Slot {
        self.start_bank.slot()
    }

    /// Returns the unique identity of the bank on which the current PoH state is based.
    pub fn start_bank_id(&self) -> BankId {
        self.start_bank.bank_id()
    }

    pub fn tick_alpenglow(
        &mut self,
        slot_max_tick_height: u64,
        footer: BlockFooterV1,
    ) -> Result<()> {
        let (poh_entry, tick_lock_contention_us) = measure_us!({
            let mut poh_l = self.poh.lock().unwrap();
            poh_l.tick()
        });
        self.metrics.tick_lock_contention_us += tick_lock_contention_us;
        let poh_entry = poh_entry.expect("low-power PoH must produce every requested tick");
        self.shared_leader_state
            .0
            .load()
            .tick_height
            .store(slot_max_tick_height, Ordering::Release);

        let working_bank = self
            .working_bank
            .as_ref()
            .ok_or(PohRecorderError::MaxHeightReached)?;
        working_bank.bank.register_tick(&poh_entry.hash);
        self.wait_for_freeze_and_send_footer(footer, working_bank)?;

        let tick = RecorderMessage::from(Entry {
            num_hashes: poh_entry.num_hashes,
            hash: poh_entry.hash,
            transactions: vec![],
        });
        send_working_bank_message(
            &self.working_bank_sender,
            (working_bank.bank.clone(), (tick, slot_max_tick_height)),
        )?;

        info!(
            "poh_record: max_tick_height {slot_max_tick_height} reached, clearing working_bank {}",
            working_bank.bank.slot()
        );
        self.start_bank = working_bank.bank.clone();
        self.clear_bank(true);
        Ok(())
    }
}

/// A shareable leader status that can be used to
/// determine the current leader status of the
/// `PohRecorder`.
#[derive(Clone)]
pub struct SharedLeaderState(Arc<ArcSwap<LeaderState>>);

impl SharedLeaderState {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(tick_height: u64, next_leader_slot_range: Option<(Slot, Slot)>) -> Self {
        let inner = LeaderState {
            working_bank: None,
            tick_height: AtomicU64::new(tick_height),
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
}

pub struct LeaderState {
    working_bank: Option<Arc<Bank>>,
    tick_height: AtomicU64,
    next_leader_slot_range: Option<(Slot, Slot)>,
}

impl LeaderState {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(
        working_bank: Option<Arc<Bank>>,
        tick_height: u64,
        next_leader_slot_range: Option<(u64, u64)>,
    ) -> Self {
        Self {
            working_bank,
            tick_height: AtomicU64::new(tick_height),
            next_leader_slot_range,
        }
    }

    pub fn working_bank(&self) -> Option<&Arc<Bank>> {
        self.working_bank.as_ref()
    }

    pub fn tick_height(&self) -> u64 {
        self.tick_height.load(Ordering::Acquire)
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
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
            get_tmp_ledger_path_auto_delete,
        },
    };

    fn new_test_recorder(
        bank: Arc<Bank>,
        blockstore: Arc<Blockstore>,
        clear_bank_signal: Option<Sender<bool>>,
        is_exited: Arc<AtomicBool>,
    ) -> (PohRecorder, Receiver<WorkingBankMessage>) {
        PohRecorder::new_with_clear_signal(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 7)),
            bank.ticks_per_slot(),
            false,
            blockstore,
            clear_bank_signal,
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            is_exited,
        )
    }

    fn test_footer() -> BlockFooterV1 {
        BlockFooterV1 {
            bank_hash: Hash::default(),
            block_producer_time_nanos: 0,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        }
    }

    fn new_alpenglow_recorder_for_bank(
        bank: Arc<Bank>,
        blockstore: Arc<Blockstore>,
    ) -> (PohRecorder, Receiver<WorkingBankMessage>) {
        let (mut poh_recorder, entry_receiver) =
            new_test_recorder(bank.clone(), blockstore, None, Arc::default());
        bank.set_tick_height(bank.max_tick_height() - 1);
        poh_recorder.set_bank(BankWithScheduler::new_without_scheduler(bank));
        (poh_recorder, entry_receiver)
    }

    #[test]
    fn test_record_and_slot_start() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, receiver) =
            new_test_recorder(bank.clone(), blockstore, None, Arc::default());

        poh_recorder
            .set_bank_and_send_slot_start(BankWithScheduler::new_without_scheduler(bank.clone()))
            .unwrap();
        let (_, (message, tick_height)) = receiver.recv().unwrap();
        assert!(matches!(message, RecorderMessage::SlotStart));
        assert_eq!(tick_height, poh_recorder.tick_height());

        poh_recorder
            .record(
                bank.bank_id(),
                Hash::new_unique(),
                vec![VersionedTransaction::default()],
            )
            .unwrap();
        let (message_bank, (message, record_tick_height)) = receiver.recv().unwrap();
        assert_eq!(message_bank.bank_id(), bank.bank_id());
        assert_eq!(record_tick_height, tick_height);
        assert!(matches!(
            message,
            RecorderMessage::Entry(Entry {
                transactions,
                ..
            }) if transactions.len() == 1
        ));

        assert_matches!(
            poh_recorder.record(
                bank.bank_id_generator().next(),
                Hash::new_unique(),
                vec![VersionedTransaction::default()],
            ),
            Err(PohRecorderError::MaxHeightReached)
        );
    }

    #[test]
    fn test_send_marker_error_leaves_bank_attached() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, receiver) =
            new_test_recorder(bank.clone(), blockstore, None, Arc::default());
        poh_recorder.set_bank(BankWithScheduler::new_without_scheduler(bank));
        drop(receiver);

        let marker = VersionedBlockMarker::from_block_footer(test_footer());
        assert_matches!(
            poh_recorder.send_marker(marker),
            Err(PohRecorderError::SendError(_))
        );
        assert!(poh_recorder.has_bank());
    }

    #[test]
    fn test_clear_bank_updates_shared_state_and_signals() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (signal_sender, signal_receiver) = bounded(1);
        let (mut poh_recorder, _receiver) = new_test_recorder(
            bank.clone(),
            blockstore,
            Some(signal_sender),
            Arc::default(),
        );
        poh_recorder.set_bank(BankWithScheduler::new_without_scheduler(bank));

        let shared_leader_state = poh_recorder.shared_leader_state();
        assert!(shared_leader_state.load().working_bank().is_some());
        poh_recorder.clear_bank(true);
        assert!(shared_leader_state.load().working_bank().is_none());
        assert!(signal_receiver.try_recv().is_ok());
    }

    #[test]
    fn test_reset_uses_low_power_poh_and_updates_leader_range() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _receiver) =
            new_test_recorder(bank.clone(), blockstore, None, Arc::default());

        poh_recorder.reset(bank.clone(), Some((8, 11)));

        assert_eq!(
            poh_recorder.tick_height(),
            (bank.slot() + 1) * bank.ticks_per_slot()
        );
        assert_eq!(
            poh_recorder
                .shared_leader_state()
                .load()
                .next_leader_slot_range(),
            Some((8, 11))
        );
        assert_eq!(
            poh_recorder.poh.lock().unwrap().hashes_per_tick_config(),
            None
        );
    }

    #[test]
    fn test_alpenglow_tick_sends_footer_then_tick_and_clears_bank() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, receiver) =
            new_alpenglow_recorder_for_bank(bank.clone(), blockstore);

        let bank_to_freeze = bank.clone();
        let freezer = std::thread::spawn(move || {
            while bank_to_freeze.tick_height() < bank_to_freeze.max_tick_height() {
                std::hint::spin_loop();
            }
            bank_to_freeze.freeze();
        });

        poh_recorder
            .tick_alpenglow(bank.max_tick_height(), test_footer())
            .unwrap();
        freezer.join().unwrap();

        let (_, (footer, footer_tick_height)) = receiver.recv().unwrap();
        assert!(matches!(footer, RecorderMessage::Marker(_)));
        assert_eq!(footer_tick_height, bank.max_tick_height() - 1);
        let (_, (tick, tick_height)) = receiver.recv().unwrap();
        assert!(matches!(
            tick,
            RecorderMessage::Entry(Entry { transactions, .. }) if transactions.is_empty()
        ));
        assert_eq!(tick_height, bank.max_tick_height());
        assert!(!poh_recorder.has_bank());
        assert_eq!(poh_recorder.start_bank_id(), bank.bank_id());
    }

    #[test]
    fn test_alpenglow_tick_send_error_leaves_bank_attached() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, receiver) =
            new_alpenglow_recorder_for_bank(bank.clone(), blockstore);
        drop(receiver);

        let bank_to_freeze = bank.clone();
        let freezer = std::thread::spawn(move || {
            while bank_to_freeze.tick_height() < bank_to_freeze.max_tick_height() {
                std::hint::spin_loop();
            }
            bank_to_freeze.freeze();
        });

        assert_matches!(
            poh_recorder.tick_alpenglow(bank.max_tick_height(), test_footer()),
            Err(PohRecorderError::SendError(_))
        );
        freezer.join().unwrap();
        assert!(poh_recorder.has_bank());
    }

    #[test]
    fn test_alpenglow_tick_timeout_leaves_bank_attached() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let (mut poh_recorder, _receiver) =
            new_alpenglow_recorder_for_bank(bank.clone(), blockstore);

        assert_matches!(
            poh_recorder.tick_alpenglow(bank.max_tick_height(), test_footer()),
            Err(PohRecorderError::BankFreezeTimeout(slot)) if slot == bank.slot()
        );
        assert!(poh_recorder.has_bank());
    }
}
