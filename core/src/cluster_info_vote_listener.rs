use {
    crate::{
        consensus::vote_stake_tracker::VoteStakeTracker,
        optimistic_confirmation_verifier::OptimisticConfirmationVerifier,
        replay_stage::DUPLICATE_THRESHOLD,
        result::{Error, Result},
    },
    agave_bls_sigverify::pubkeys::{VerifiedVotorSlotsMessage, VoteAccountPubkeys},
    agave_votor_messages::migration::MigrationStatus,
    crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender},
    log::*,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::blockstore::Blockstore,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank::{Bank, BankId},
        bank_forks::{BankForks, SharableBanks},
        commitment::VOTE_THRESHOLD_SIZE,
        vote_sender_types::{ReplayVoteMessage, ReplayVoteReceiver},
    },
    solana_signature::Signature,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    solana_time_utils::AtomicInterval,
    solana_vote::{vote_parser::ParsedVote, vote_transaction::VoteTransaction},
    std::{
        cmp::max,
        collections::{HashMap, hash_map::Entry},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// Map from a vote account to the authorized voter for an epoch
pub type ThresholdConfirmedSlots = Vec<(Slot, Hash)>;
pub type DuplicateConfirmedSlotsSender = Sender<ThresholdConfirmedSlots>;
pub type DuplicateConfirmedSlotsReceiver = Receiver<ThresholdConfirmedSlots>;

const THRESHOLDS_TO_CHECK: [f64; 2] = [DUPLICATE_THRESHOLD, VOTE_THRESHOLD_SIZE];
const MAX_VOTE_HASHES_PER_PUBKEY_PER_SLOT: u8 = 2;

/// Notification channels and context threaded through the vote confirmation
/// pipeline. Groups the senders used to communicate threshold crossings
/// (duplicate confirmation and optimistic confirmation) together with the
/// migration status that gates notifications.
struct ConfirmationNotifiers {
    verified_voter_slots_sender: EvictingSender<VerifiedVotorSlotsMessage>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    duplicate_confirmed_slot_sender: Option<DuplicateConfirmedSlotsSender>,
    migration_status: Arc<MigrationStatus>,
}

#[derive(Default)]
pub struct SlotVoteTracker {
    // Maps pubkeys that have voted for this slot
    // to legacy vote-tracking metadata.
    voted: HashMap<Pubkey, bool>,
    optimistic_votes_tracker: HashMap<Hash, VoteStakeTracker>,
    num_optimistic_vote_hashes: HashMap<Pubkey, u8>,
    voted_slot_updates: Option<Vec<Pubkey>>,
}

impl SlotVoteTracker {
    pub(crate) fn get_voted_slot_updates(&mut self) -> Option<Vec<Pubkey>> {
        self.voted_slot_updates.take()
    }

    fn add_optimistic_vote(
        &mut self,
        hash: Hash,
        pubkey: Pubkey,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (Vec<bool>, bool) {
        let num_vote_hashes = self.num_optimistic_vote_hashes.entry(pubkey).or_default();
        if *num_vote_hashes >= MAX_VOTE_HASHES_PER_PUBKEY_PER_SLOT {
            return (vec![false; THRESHOLDS_TO_CHECK.len()], false);
        }

        let result @ (_, is_new) = self
            .optimistic_votes_tracker
            .entry(hash)
            .or_default()
            .add_vote_pubkey(pubkey, stake, total_epoch_stake, &THRESHOLDS_TO_CHECK);

        if is_new {
            *num_vote_hashes += 1;
        }

        result
    }
    pub(crate) fn optimistic_votes_tracker(&self, hash: &Hash) -> Option<&VoteStakeTracker> {
        self.optimistic_votes_tracker.get(hash)
    }
}

#[derive(Default)]
pub struct VoteTracker {
    // Map from a slot to a set of validators who have voted for that slot
    slot_vote_trackers: RwLock<HashMap<Slot, Arc<RwLock<SlotVoteTracker>>>>,
}

impl VoteTracker {
    fn get_or_insert_slot_tracker(&self, slot: Slot) -> Arc<RwLock<SlotVoteTracker>> {
        if let Some(slot_vote_tracker) = self.slot_vote_trackers.read().unwrap().get(&slot) {
            return slot_vote_tracker.clone();
        }
        let mut slot_vote_trackers = self.slot_vote_trackers.write().unwrap();
        slot_vote_trackers.entry(slot).or_default().clone()
    }

    pub(crate) fn get_slot_vote_tracker(&self, slot: Slot) -> Option<Arc<RwLock<SlotVoteTracker>>> {
        self.slot_vote_trackers.read().unwrap().get(&slot).cloned()
    }

    #[cfg(test)]
    pub(crate) fn insert_vote(&self, slot: Slot, pubkey: Pubkey) {
        let mut w_slot_vote_trackers = self.slot_vote_trackers.write().unwrap();

        let slot_vote_tracker = w_slot_vote_trackers.entry(slot).or_default();

        let mut w_slot_vote_tracker = slot_vote_tracker.write().unwrap();

        w_slot_vote_tracker.voted.insert(pubkey, true);
        if let Some(ref mut voted_slot_updates) = w_slot_vote_tracker.voted_slot_updates {
            voted_slot_updates.push(pubkey)
        } else {
            w_slot_vote_tracker.voted_slot_updates = Some(vec![pubkey]);
        }
    }

    fn purge_stale_state(&self, root_bank: &Bank) {
        // Purge any outdated slot data
        let new_root = root_bank.slot();
        self.slot_vote_trackers
            .write()
            .unwrap()
            .retain(|slot, _| *slot >= new_root);
    }

    fn progress_with_new_root_bank(&self, root_bank: &Bank) {
        self.purge_stale_state(root_bank);
    }

    fn clear(&self) {
        self.slot_vote_trackers.write().unwrap().clear();
    }
}

#[derive(Default)]
struct VoteProcessingTiming {
    txn_processing_time_us: u64,
    slot_confirming_time_us: u64,
    last_report: AtomicInterval,
}

const VOTE_PROCESSING_REPORT_INTERVAL_MS: u64 = 1_000;

impl VoteProcessingTiming {
    fn reset(&mut self) {
        self.txn_processing_time_us = 0;
        self.slot_confirming_time_us = 0;
    }

    fn update(&mut self, vote_txn_processing_time_us: u64, vote_slot_confirming_time_us: u64) {
        self.txn_processing_time_us += vote_txn_processing_time_us;
        self.slot_confirming_time_us += vote_slot_confirming_time_us;

        if self
            .last_report
            .should_update(VOTE_PROCESSING_REPORT_INTERVAL_MS)
        {
            datapoint_info!(
                "vote-processing-timing",
                (
                    "vote_txn_processing_us",
                    self.txn_processing_time_us as i64,
                    i64
                ),
                (
                    "slot_confirming_time_us",
                    self.slot_confirming_time_us as i64,
                    i64
                ),
            );
            self.reset();
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum BufferedVote {
    Executed(ParsedVote),
    Verified,
}

#[allow(clippy::large_enum_variant)]
enum ReplayVoteAction {
    Executed(ParsedVote),
    Verified,
}

impl From<ReplayVoteAction> for BufferedVote {
    fn from(action: ReplayVoteAction) -> Self {
        match action {
            ReplayVoteAction::Executed(parsed_vote) => Self::Executed(parsed_vote),
            ReplayVoteAction::Verified => Self::Verified,
        }
    }
}

impl BufferedVote {
    fn apply(
        self,
        action: ReplayVoteAction,
        ready_votes: &mut Vec<ParsedVote>,
        replay_bank_id: BankId,
        message_hash: Hash,
    ) -> Option<Self> {
        match (self, action) {
            (Self::Executed(parsed_vote), ReplayVoteAction::Verified)
            | (Self::Verified, ReplayVoteAction::Executed(parsed_vote)) => {
                ready_votes.push(parsed_vote);
                None
            }
            (Self::Verified, ReplayVoteAction::Verified) => Some(Self::Verified),
            (Self::Executed(parsed_vote), ReplayVoteAction::Executed(_)) => {
                debug_assert!(
                    false,
                    "duplicate Executed replay vote for same bank {replay_bank_id:?} message hash \
                     {message_hash}"
                );
                Some(Self::Executed(parsed_vote))
            }
        }
    }
}

#[derive(Debug)]
enum BankVoteBuffer {
    /// The bank is active and we are buffering votes for it.
    Active {
        slot: Slot,
        votes: HashMap<Hash, BufferedVote>,
    },
    /// The bank is invalid and we are discarding votes.
    Invalid { slot: Slot },
}

impl BankVoteBuffer {
    fn slot(&self) -> Slot {
        match self {
            BankVoteBuffer::Active { slot, .. } => *slot,
            BankVoteBuffer::Invalid { slot } => *slot,
        }
    }
}

struct VoteBuffer {
    bank_votes: HashMap<BankId, BankVoteBuffer>,
}

impl VoteBuffer {
    fn new() -> Self {
        Self {
            bank_votes: HashMap::new(),
        }
    }

    fn receive_and_collect_ready_votes(
        &mut self,
        replay_votes: impl Iterator<Item = ReplayVoteMessage>,
    ) -> Vec<ParsedVote> {
        let mut ready_votes = Vec::new();
        for replay_vote in replay_votes {
            match replay_vote {
                ReplayVoteMessage::VerifiedExecuted(parsed_vote) => ready_votes.push(parsed_vote),
                ReplayVoteMessage::Executed {
                    replay_bank_id,
                    replay_slot,
                    message_hash,
                    parsed_vote,
                } => {
                    match self.bank_votes.entry(replay_bank_id) {
                        Entry::Vacant(entry) => {
                            entry.insert(BankVoteBuffer::Active {
                                slot: replay_slot,
                                votes: HashMap::from([(
                                    message_hash,
                                    BufferedVote::Executed(parsed_vote),
                                )]),
                            });
                        }
                        Entry::Occupied(mut entry) => {
                            let BankVoteBuffer::Active {
                                slot: stored_replay_slot,
                                votes,
                            } = entry.get_mut()
                            else {
                                // Skip votes for invalid banks
                                continue;
                            };
                            debug_assert_eq!(*stored_replay_slot, replay_slot);
                            Self::apply_action_for_message_hash(
                                votes,
                                message_hash,
                                ReplayVoteAction::Executed(parsed_vote),
                                &mut ready_votes,
                                replay_bank_id,
                            );
                            if votes.is_empty() {
                                entry.remove();
                            }
                        }
                    }
                }

                ReplayVoteMessage::Verified {
                    replay_bank_id,
                    replay_slot,
                    message_hashes,
                } => {
                    debug_assert!(
                        !message_hashes.is_empty(),
                        "empty replay Verified message for bank {replay_bank_id:?}, slot \
                         {replay_slot}"
                    );
                    match self.bank_votes.entry(replay_bank_id) {
                        Entry::Vacant(entry) => {
                            entry.insert(BankVoteBuffer::Active {
                                slot: replay_slot,
                                votes: message_hashes
                                    .into_iter()
                                    .map(|message_hash| (message_hash, BufferedVote::Verified))
                                    .collect(),
                            });
                        }
                        Entry::Occupied(mut entry) => {
                            let BankVoteBuffer::Active {
                                slot: stored_replay_slot,
                                votes,
                            } = entry.get_mut()
                            else {
                                // Skip votes for invalid banks
                                continue;
                            };
                            debug_assert_eq!(*stored_replay_slot, replay_slot);
                            for message_hash in message_hashes.into_iter() {
                                Self::apply_action_for_message_hash(
                                    votes,
                                    message_hash,
                                    ReplayVoteAction::Verified,
                                    &mut ready_votes,
                                    replay_bank_id,
                                );
                            }
                            if votes.is_empty() {
                                entry.remove();
                            }
                        }
                    }
                }
                ReplayVoteMessage::InvalidBank {
                    replay_bank_id,
                    replay_slot,
                } => {
                    self.bank_votes.insert(
                        replay_bank_id,
                        BankVoteBuffer::Invalid { slot: replay_slot },
                    );
                }
                ReplayVoteMessage::BankComplete { replay_bank_id, .. } => {
                    self.bank_votes.remove(&replay_bank_id);
                }
            }
        }

        ready_votes
    }

    #[inline]
    fn apply_action_for_message_hash(
        votes: &mut HashMap<Hash, BufferedVote>,
        message_hash: Hash,
        action: ReplayVoteAction,
        ready_votes: &mut Vec<ParsedVote>,
        replay_bank_id: BankId,
    ) {
        match votes.entry(message_hash) {
            Entry::Vacant(entry) => {
                entry.insert(action.into());
            }
            Entry::Occupied(entry) => {
                let (_message_hash, current_state) = entry.remove_entry();
                if let Some(next_state) =
                    current_state.apply(action, ready_votes, replay_bank_id, message_hash)
                {
                    votes.insert(message_hash, next_state);
                }
            }
        }
    }

    fn prune_stale_slots(&mut self, root_slot: Slot) {
        self.bank_votes.retain(|_, state| state.slot() > root_slot);
    }
}

pub struct ClusterInfoVoteListener {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ClusterInfoVoteListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        exit: Arc<AtomicBool>,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Option<Arc<RpcSubscriptions>>,
        verified_voter_slots_sender: EvictingSender<VerifiedVotorSlotsMessage>,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: DuplicateConfirmedSlotsSender,
    ) -> Self {
        let migration_status = bank_forks.read().unwrap().migration_status();
        let process_thread = Builder::new()
            .name("solCiProcVotes".to_string())
            .spawn(move || {
                let sharable_banks = bank_forks.read().unwrap().sharable_banks();
                let notifiers = ConfirmationNotifiers {
                    verified_voter_slots_sender,
                    rpc_subscriptions: subscriptions,
                    bank_notification_sender,
                    duplicate_confirmed_slot_sender: Some(duplicate_confirmed_slot_sender),
                    migration_status,
                };
                let _ = Self::process_votes_loop(
                    exit,
                    vote_tracker,
                    sharable_banks,
                    replay_votes_receiver,
                    blockstore,
                    notifiers,
                );
            })
            .unwrap();

        Self {
            thread_hdls: vec![process_thread],
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }

    fn process_votes_loop(
        exit: Arc<AtomicBool>,
        vote_tracker: Arc<VoteTracker>,
        sharable_banks: SharableBanks,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        notifiers: ConfirmationNotifiers,
    ) -> Result<()> {
        let mut confirmation_verifier = Some(OptimisticConfirmationVerifier::new(
            sharable_banks.root().slot(),
        ));
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut last_process_root = Instant::now();
        let mut vote_processing_time = Some(VoteProcessingTiming::default());
        let mut replay_vote_buffer = VoteBuffer::new();
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let root_bank = sharable_banks.root();
            let is_alpenglow_enabled = notifiers.migration_status.is_alpenglow_enabled();
            if is_alpenglow_enabled {
                // Drop the verifier's retained optimistic-confirmation state at the handoff.
                confirmation_verifier = None;
            } else if last_process_root.elapsed().as_nanos() > root_bank.ns_per_slot {
                let confirmation_verifier = confirmation_verifier
                    .as_mut()
                    .expect("Alpenglow migration status cannot revert");
                let unrooted_optimistic_slots = confirmation_verifier
                    .verify_for_unrooted_optimistic_slots(&root_bank, &blockstore);
                // SlotVoteTracker's for all `slots` in `unrooted_optimistic_slots`
                // should still be available because we haven't purged in
                // `progress_with_new_root_bank()` yet, which is called below
                OptimisticConfirmationVerifier::log_unrooted_optimistic_slots(
                    &root_bank,
                    &vote_tracker,
                    &unrooted_optimistic_slots,
                );
                vote_tracker.progress_with_new_root_bank(&root_bank);
                replay_vote_buffer.prune_stale_slots(root_bank.slot());
                last_process_root = Instant::now();
            }
            let confirmed_slots = Self::listen_and_confirm_votes(
                &vote_tracker,
                &root_bank,
                &replay_votes_receiver,
                &mut replay_vote_buffer,
                &notifiers,
                &mut vote_processing_time,
                &mut latest_vote_slot_per_validator,
            );
            match confirmed_slots {
                Ok(confirmed_slots) => {
                    let confirmed_slots = confirmed_slots
                        .into_iter()
                        .filter(|(slot, _hash)| {
                            notifiers
                                .migration_status
                                .should_report_commitment_or_root(*slot)
                        })
                        .collect();
                    if let Some(confirmation_verifier) = confirmation_verifier.as_mut() {
                        confirmation_verifier
                            .add_new_optimistic_confirmed_slots(confirmed_slots, &blockstore);
                    }
                }
                Err(e) => match e {
                    Error::RecvTimeout(RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                    Error::ReadyTimeout => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                },
            }
        }
    }

    fn listen_and_confirm_votes(
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        replay_votes_receiver: &ReplayVoteReceiver,
        replay_vote_buffer: &mut VoteBuffer,
        notifiers: &ConfirmationNotifiers,
        vote_processing_time: &mut Option<VoteProcessingTiming>,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
    ) -> Result<ThresholdConfirmedSlots> {
        let mut sel = Select::new();
        sel.recv(replay_votes_receiver);
        if notifiers.migration_status.is_alpenglow_enabled() {
            vote_tracker.clear();
            latest_vote_slot_per_validator.clear();
            replay_vote_buffer.bank_votes.clear();
        }
        let mut remaining_wait_time = Duration::from_millis(200);
        while remaining_wait_time > Duration::ZERO {
            let start = Instant::now();
            // Wait for one of the receivers to be ready. `ready_timeout`
            // will return if channels either have something, or are
            // disconnected. `ready_timeout` can wake up spuriously,
            // hence the loop
            let _ = sel.ready_timeout(remaining_wait_time)?;

            if notifiers.migration_status.is_alpenglow_enabled() {
                // Replay can still have legacy vote notifications in flight when the migration
                // completes. Keep draining the channels so their producers remain connected, but
                // discard all Tower vote state and skip optimistic confirmation aggregation.
                for _ in replay_votes_receiver.try_iter() {}
                vote_tracker.clear();
                latest_vote_slot_per_validator.clear();
                replay_vote_buffer.bank_votes.clear();
                return Ok(vec![]);
            }
            // Should not early return from this point onwards until `process_votes()`
            // returns below to avoid missing any potential `optimistic_confirmed_slots`
            let replay_votes = replay_vote_buffer
                .receive_and_collect_ready_votes(replay_votes_receiver.try_iter());
            if !replay_votes.is_empty() {
                return Ok(Self::filter_and_confirm_with_new_votes(
                    vote_tracker,
                    replay_votes,
                    root_bank,
                    notifiers,
                    vote_processing_time,
                    latest_vote_slot_per_validator,
                ));
            }
            remaining_wait_time = remaining_wait_time.saturating_sub(start.elapsed());
        }
        Ok(vec![])
    }

    /// Fast-track processing of the last vote slot in a vote transaction for
    /// optimistic confirmation. Checks stake thresholds and sends notifications
    /// for duplicate confirmation and optimistic confirmation as needed.
    /// Returns whether this is a new vote for the slot.
    fn process_last_vote_for_optimistic_confirmation(
        vote_tracker: &VoteTracker,
        last_vote_slot: Slot,
        last_vote_hash: Hash,
        vote_pubkey: &Pubkey,
        root_bank: &Bank,
        notifiers: &ConfirmationNotifiers,
        new_optimistic_confirmed_slots: &mut ThresholdConfirmedSlots,
    ) -> bool {
        if notifiers.migration_status.is_alpenglow_enabled() {
            return false;
        }
        if last_vote_slot <= root_bank.slot() {
            return false;
        }

        let epoch = root_bank.epoch_schedule().get_epoch(last_vote_slot);
        let Some(epoch_stakes) = root_bank.epoch_stakes(epoch) else {
            return false;
        };

        let stake = epoch_stakes
            .stakes()
            .vote_accounts()
            .get_delegated_stake(vote_pubkey);
        let total_stake = epoch_stakes.total_stake();

        let (reached_threshold_results, is_new) = Self::track_optimistic_confirmation_vote(
            vote_tracker,
            last_vote_slot,
            last_vote_hash,
            *vote_pubkey,
            stake,
            total_stake,
        );

        let reached_duplicate_confirmed = reached_threshold_results[0];
        let reached_optimistic_confirmed = reached_threshold_results[1];

        if reached_duplicate_confirmed
            && let Some(ref sender) = notifiers.duplicate_confirmed_slot_sender
        {
            let _ = sender.send(vec![(last_vote_slot, last_vote_hash)]);
        }

        if reached_optimistic_confirmed {
            new_optimistic_confirmed_slots.push((last_vote_slot, last_vote_hash));
            if let Some(ref sender) = notifiers.bank_notification_sender
                && notifiers
                    .migration_status
                    .should_report_commitment_or_root(last_vote_slot)
            {
                let dependency_work = sender
                    .dependency_tracker
                    .as_ref()
                    .map(|s| s.get_current_declared_work());
                sender
                    .sender
                    .send((
                        BankNotification::OptimisticallyConfirmed(last_vote_slot, last_vote_hash),
                        dependency_work,
                    ))
                    .unwrap_or_else(|err| warn!("bank_notification_sender failed: {err:?}"));
            }
        }

        is_new
    }

    #[allow(clippy::too_many_arguments)]
    fn track_new_votes_and_notify_confirmations(
        vote: VoteTransaction,
        vote_pubkey: &Pubkey,
        vote_transaction_signature: Signature,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        notifiers: &ConfirmationNotifiers,
        diff: &mut HashMap<Slot, HashMap<Pubkey, bool>>,
        new_optimistic_confirmed_slots: &mut ThresholdConfirmedSlots,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
    ) {
        if vote.is_empty() || notifiers.migration_status.is_alpenglow_enabled() {
            return;
        }

        let (last_vote_slot, last_vote_hash) = vote.last_voted_slot_hash().unwrap();

        let latest_vote_slot = latest_vote_slot_per_validator
            .entry(*vote_pubkey)
            .or_insert(0);

        let root = root_bank.slot();
        let vote_slots = vote.slots();

        // Replay votes have already been executed and their slots validated.
        let is_new_vote = Self::process_last_vote_for_optimistic_confirmation(
            vote_tracker,
            last_vote_slot,
            last_vote_hash,
            vote_pubkey,
            root_bank,
            notifiers,
            new_optimistic_confirmed_slots,
        );

        if notifiers.migration_status.is_alpenglow_enabled() {
            return;
        }

        if !is_new_vote {
            // By now:
            // 1) The vote must have come from ReplayStage,
            // 2) We've seen this vote from replay for this hash before
            // (`track_optimistic_confirmation_vote()` will not set `is_new == true`
            // for same slot different hash), so short circuit because this vote
            // has no new information

            return;
        }

        if is_new_vote {
            if let Some(ref rpc_subscriptions) = notifiers.rpc_subscriptions {
                rpc_subscriptions.notify_vote(*vote_pubkey, vote, vote_transaction_signature);
            }
            for slot in vote_slots.iter().cloned() {
                let _ = notifiers
                    .verified_voter_slots_sender
                    .try_send((slot, VoteAccountPubkeys::Owned(vec![*vote_pubkey])));
            }
        }

        // Track all vote slots for propagated check (iterates from most recent to oldest)
        for slot in vote_slots
            .into_iter()
            .filter(|&slot| slot > root && slot >= *latest_vote_slot)
            .rev()
        {
            // If we don't have stake information, ignore it
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            if root_bank.epoch_stakes(epoch).is_none() {
                continue;
            }

            diff.entry(slot)
                .or_default()
                .entry(*vote_pubkey)
                .or_insert(false);
        }

        *latest_vote_slot = max(*latest_vote_slot, last_vote_slot);
    }

    fn filter_and_confirm_with_new_votes(
        vote_tracker: &VoteTracker,
        replayed_votes: Vec<ParsedVote>,
        root_bank: &Bank,
        notifiers: &ConfirmationNotifiers,
        vote_processing_time: &mut Option<VoteProcessingTiming>,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
    ) -> ThresholdConfirmedSlots {
        if notifiers.migration_status.is_alpenglow_enabled() {
            return vec![];
        }

        let mut diff: HashMap<Slot, HashMap<Pubkey, bool>> = HashMap::new();
        let mut new_optimistic_confirmed_slots = vec![];

        // Process replayed vote transactions.
        let mut vote_txn_processing_time = Measure::start("replay_vote_processing_time");
        for (vote_pubkey, vote, _switch_proof, signature) in replayed_votes {
            Self::track_new_votes_and_notify_confirmations(
                vote,
                &vote_pubkey,
                signature,
                vote_tracker,
                root_bank,
                notifiers,
                &mut diff,
                &mut new_optimistic_confirmed_slots,
                latest_vote_slot_per_validator,
            );
        }
        vote_txn_processing_time.stop();
        let vote_txn_processing_time_us = vote_txn_processing_time.as_us();

        // Process the slots accumulated from replay.
        let mut vote_slot_confirming_time = Measure::start("replay_vote_slot_confirm_time");
        for (slot, mut slot_diff) in diff {
            let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
            {
                let r_slot_tracker = slot_tracker.read().unwrap();
                // Only keep the pubkeys we haven't seen voting for this slot
                slot_diff.retain(|pubkey, _| !r_slot_tracker.voted.contains_key(pubkey));
            }
            let mut w_slot_tracker = slot_tracker.write().unwrap();
            if w_slot_tracker.voted_slot_updates.is_none() {
                w_slot_tracker.voted_slot_updates = Some(vec![]);
            }
            for (pubkey, _) in slot_diff {
                w_slot_tracker.voted.insert(pubkey, false);
                w_slot_tracker
                    .voted_slot_updates
                    .as_mut()
                    .unwrap()
                    .push(pubkey);
            }
        }
        vote_slot_confirming_time.stop();
        let vote_slot_confirming_time_us = vote_slot_confirming_time.as_us();

        if let Some(vote_processing_time) = vote_processing_time {
            vote_processing_time.update(vote_txn_processing_time_us, vote_slot_confirming_time_us)
        }
        new_optimistic_confirmed_slots
    }

    // Returns if the slot was optimistically confirmed, and whether
    // the slot was new
    fn track_optimistic_confirmation_vote(
        vote_tracker: &VoteTracker,
        slot: Slot,
        hash: Hash,
        pubkey: Pubkey,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (Vec<bool>, bool) {
        let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
        // Insert vote and check for optimistic confirmation
        let mut w_slot_tracker = slot_tracker.write().unwrap();

        w_slot_tracker.add_optimistic_vote(hash, pubkey, stake, total_epoch_stake)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_perf::packet,
        solana_pubkey::Pubkey,
        solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        solana_runtime::{
            bank::Bank,
            commitment::BlockCommitmentCache,
            genesis_utils::{self, GenesisConfigInfo, ValidatorVoteKeypairs},
            vote_sender_types::ReplayVoteMessage,
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_vote::{vote_parser, vote_transaction},
        solana_vote_program::vote_state::{MAX_LOCKOUT_HISTORY, TowerSync, Vote},
        std::sync::{Arc, atomic::AtomicU64},
    };

    #[test]
    fn test_max_vote_tx_fits() {
        agave_logger::setup();
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let tower_sync = TowerSync::new_from_slot(MAX_LOCKOUT_HISTORY as u64, Hash::default());
        let vote_tx = vote_transaction::new_tower_sync_transaction(
            tower_sync,
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            Some(Hash::default()),
        );

        use bincode::serialized_size;
        info!("max vote size {}", serialized_size(&vote_tx).unwrap());

        let packet_batches = packet::to_packet_batches(&[vote_tx], 1); // panics if won't fit

        assert_eq!(packet_batches.len(), 1);
    }

    #[test]
    fn test_update_new_root() {
        let SetupComponents {
            vote_tracker,
            bank,
            bank_forks: _bank_forks,
            ..
        } = setup();

        // Check outdated slots are purged with new root
        let new_voter = solana_pubkey::new_rand();
        // Make separate copy so the original doesn't count toward
        // the ref count, which would prevent cleanup
        let new_voter_ = new_voter;
        vote_tracker.insert_vote(bank.slot(), new_voter_);
        assert!(
            vote_tracker
                .slot_vote_trackers
                .read()
                .unwrap()
                .contains_key(&bank.slot())
        );
        let bank1 = Bank::new_from_parent(bank.clone(), SlotLeader::default(), bank.slot() + 1);
        vote_tracker.progress_with_new_root_bank(&bank1);
        assert!(
            !vote_tracker
                .slot_vote_trackers
                .read()
                .unwrap()
                .contains_key(&bank.slot())
        );

        // Check `keys` and `epoch_authorized_voters` are purged when new
        // root bank moves to the next epoch
        let current_epoch = bank.epoch();
        let new_epoch_slot = bank
            .epoch_schedule()
            .get_first_slot_in_epoch(current_epoch + 1);
        let new_epoch_bank = Bank::new_from_parent(bank, SlotLeader::default(), new_epoch_slot);
        vote_tracker.progress_with_new_root_bank(&new_epoch_bank);
    }

    #[test]
    fn test_update_new_leader_schedule_epoch() {
        let SetupComponents { bank, .. } = setup();

        // Check outdated slots are purged with new root
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let next_leader_schedule_epoch = leader_schedule_epoch + 1;
        let mut next_leader_schedule_computed = bank.slot();
        loop {
            next_leader_schedule_computed += 1;
            if bank.get_leader_schedule_epoch(next_leader_schedule_computed)
                == next_leader_schedule_epoch
            {
                break;
            }
        }
        assert_eq!(
            bank.get_leader_schedule_epoch(next_leader_schedule_computed),
            next_leader_schedule_epoch
        );
    }

    fn sample_parsed_vote(slot: Slot) -> ParsedVote {
        (
            Pubkey::new_unique(),
            VoteTransaction::from(Vote::new(vec![slot], Hash::default())),
            None,
            Signature::default(),
        )
    }

    #[test]
    fn test_replay_vote_buffer_gates_unverified_votes() {
        let replay_bank_id = BankId::new(1);
        let replay_slot = 42;
        let parsed_vote = sample_parsed_vote(replay_slot);
        let message_hash = Hash::default();
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash,
                parsed_vote: parsed_vote.clone(),
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert!(matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { .. })
        ));

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Verified {
                replay_bank_id,
                replay_slot,
                message_hashes: vec![message_hash],
            }]
            .into_iter(),
        );
        assert_eq!(ready_votes, vec![parsed_vote]);
        assert!(replay_vote_buffer.bank_votes.is_empty());
    }

    #[test]
    fn test_replay_vote_buffer_gates_unexecuted_votes() {
        let replay_bank_id = BankId::new(3);
        let replay_slot = 77;
        let parsed_vote = sample_parsed_vote(replay_slot);
        let message_hash = Hash::default();
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Verified {
                replay_bank_id,
                replay_slot,
                message_hashes: vec![message_hash],
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert!(matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { .. })
        ));

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash,
                parsed_vote: parsed_vote.clone(),
            }]
            .into_iter(),
        );
        assert_eq!(ready_votes, vec![parsed_vote]);
        assert!(replay_vote_buffer.bank_votes.is_empty());
    }

    #[test]
    fn test_replay_vote_buffer_same_signature_different_tx() {
        let replay_bank_id = BankId::new(4);
        let replay_slot = 88;
        let valid_vote = sample_parsed_vote(replay_slot);
        let spoofed_vote = sample_parsed_vote(replay_slot + 1);
        assert_eq!(valid_vote.3, spoofed_vote.3);

        let valid_message_hash = Hash::new_from_array([1; 32]);
        let spoofed_message_hash = Hash::new_from_array([2; 32]);
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![
                ReplayVoteMessage::Executed {
                    replay_bank_id,
                    replay_slot,
                    message_hash: spoofed_message_hash,
                    parsed_vote: spoofed_vote,
                },
                ReplayVoteMessage::Verified {
                    replay_bank_id,
                    replay_slot,
                    message_hashes: vec![valid_message_hash],
                },
            ]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { votes, .. }) if votes.len() == 2
        );

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash: valid_message_hash,
                parsed_vote: valid_vote.clone(),
            }]
            .into_iter(),
        );
        assert_eq!(ready_votes, vec![valid_vote]);
    }

    #[test]
    fn test_replay_vote_buffer_invalid_bank_drops_late_messages() {
        let replay_bank_id = BankId::new(2);
        let replay_slot = 100;
        let parsed_vote = sample_parsed_vote(replay_slot);
        let message_hash = Hash::default();
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Verified {
                replay_bank_id,
                replay_slot,
                message_hashes: vec![message_hash],
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert!(matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { .. })
        ));

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::InvalidBank {
                replay_bank_id,
                replay_slot,
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert!(matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Invalid {
                slot: invalid_slot
            }) if *invalid_slot == replay_slot
        ));

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash,
                parsed_vote: parsed_vote.clone(),
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert!(matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Invalid {
                slot: invalid_slot
            }) if *invalid_slot == replay_slot
        ));
    }

    #[test]
    fn test_replay_vote_buffer_bank_complete_clears_pending_state() {
        let replay_bank_id = BankId::new(5);
        let replay_slot = 123;
        let parsed_vote = sample_parsed_vote(replay_slot);
        let message_hash = Hash::default();
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash,
                parsed_vote,
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert_matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { .. })
        );

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::BankComplete {
                replay_bank_id,
                replay_slot,
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert!(replay_vote_buffer.bank_votes.is_empty());
    }

    #[test]
    fn test_replay_vote_buffer_processes_verified_message_hash() {
        let replay_bank_id = BankId::new(6);
        let replay_slot = 124;
        let parsed_vote = sample_parsed_vote(replay_slot);
        let message_hash = Hash::default();
        let mut replay_vote_buffer = VoteBuffer::new();

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Executed {
                replay_bank_id,
                replay_slot,
                message_hash,
                parsed_vote: parsed_vote.clone(),
            }]
            .into_iter(),
        );
        assert!(ready_votes.is_empty());
        assert_eq!(replay_vote_buffer.bank_votes.len(), 1);
        assert_matches!(
            replay_vote_buffer.bank_votes.get(&replay_bank_id),
            Some(BankVoteBuffer::Active { .. })
        );

        let ready_votes = replay_vote_buffer.receive_and_collect_ready_votes(
            vec![ReplayVoteMessage::Verified {
                replay_bank_id,
                replay_slot,
                message_hashes: vec![message_hash],
            }]
            .into_iter(),
        );
        assert_eq!(ready_votes, vec![parsed_vote]);
        assert!(replay_vote_buffer.bank_votes.is_empty());
    }

    #[test]
    fn test_vote_tracker_references() {
        // Create some voters at genesis
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let vote_tracker = VoteTracker::default();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));
        let mut latest_vote_slot_per_validator = HashMap::new();

        // Send a vote to process, should add a reference to the pubkey for that voter
        // in the tracker
        let validator0_keypairs = &validator_keypairs[0];
        let voted_slot = bank.slot() + 1;
        let _vote_tx = vec![vote_transaction::new_tower_sync_transaction(
            // Must vote > root to be processed
            TowerSync::from(vec![(voted_slot, 1)]),
            Hash::default(),
            &validator0_keypairs.node_keypair,
            &validator0_keypairs.vote_keypair,
            &validator0_keypairs.vote_keypair,
            None,
        )];

        let (verified_voter_slots_sender, _verified_voter_slots_receiver) =
            EvictingSender::new_bounded(1024);
        let notifiers = ConfirmationNotifiers {
            verified_voter_slots_sender: verified_voter_slots_sender.clone(),
            rpc_subscriptions: Some(subscriptions.clone()),
            bank_notification_sender: None,
            duplicate_confirmed_slot_sender: None,
            migration_status: Arc::new(MigrationStatus::default()),
        };
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vec![(
                validator0_keypairs.vote_keypair.pubkey(),
                VoteTransaction::from(Vote::new(vec![voted_slot], Hash::default())),
                None,
                Signature::default(),
            )],
            &bank,
            &notifiers,
            &mut None,
            &mut latest_vote_slot_per_validator,
        );

        // Setup next epoch
        let old_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let new_epoch = old_epoch + 1;

        // Test with votes across two epochs
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);

        // Make 2 new votes in two different epochs for the same pubkey,
        // the ref count should go up by 3 * ref_count_per_vote
        // Add 1 vote through the replay channel for a different pubkey,
        // ref count should equal `current_ref_count` for that pubkey.

        let new_root_bank =
            Bank::new_from_parent(bank, SlotLeader::default(), first_slot_in_new_epoch - 2);
        let notifiers = ConfirmationNotifiers {
            verified_voter_slots_sender: verified_voter_slots_sender.clone(),
            rpc_subscriptions: Some(subscriptions.clone()),
            bank_notification_sender: None,
            duplicate_confirmed_slot_sender: None,
            migration_status: Arc::new(MigrationStatus::default()),
        };
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vec![(
                validator_keypairs[1].vote_keypair.pubkey(),
                VoteTransaction::from(Vote::new(vec![first_slot_in_new_epoch], Hash::default())),
                None,
                Signature::default(),
            )],
            &new_root_bank,
            &notifiers,
            &mut None,
            &mut latest_vote_slot_per_validator,
        );
    }

    struct SetupComponents {
        vote_tracker: Arc<VoteTracker>,
        bank: Arc<Bank>,
        bank_forks: Arc<RwLock<BankForks>>,
    }

    fn setup() -> SetupComponents {
        let validator_voting_keypairs: Vec<_> =
            (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![100; validator_voting_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        SetupComponents {
            vote_tracker: Arc::new(VoteTracker::default()),
            bank,
            bank_forks,
        }
    }

    #[test]
    fn test_track_new_votes_filter() {
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let vote_tracker = VoteTracker::default();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));
        let mut latest_vote_slot_per_validator = HashMap::new();

        let (verified_voter_slots_sender, _verified_voter_slots_receiver) =
            EvictingSender::new_bounded(1024);
        let mut diff = HashMap::default();
        let mut new_optimistic_confirmed_slots = vec![];

        let validator0_keypairs = &validator_keypairs[0];
        let (vote_pubkey, vote, _, signature) =
            vote_parser::parse_vote_transaction(&vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(1, 3), (2, 2), (6, 1)]),
                Hash::default(),
                &validator0_keypairs.node_keypair,
                &validator0_keypairs.vote_keypair,
                &validator0_keypairs.vote_keypair,
                None,
            ))
            .unwrap();

        let notifiers = ConfirmationNotifiers {
            verified_voter_slots_sender: verified_voter_slots_sender.clone(),
            rpc_subscriptions: Some(subscriptions.clone()),
            bank_notification_sender: None,
            duplicate_confirmed_slot_sender: None,
            migration_status: Arc::new(MigrationStatus::default()),
        };
        ClusterInfoVoteListener::track_new_votes_and_notify_confirmations(
            vote,
            &vote_pubkey,
            signature,
            &vote_tracker,
            &bank,
            &notifiers,
            &mut diff,
            &mut new_optimistic_confirmed_slots,
            &mut latest_vote_slot_per_validator,
        );
        assert_eq!(diff.keys().copied().sorted().collect_vec(), vec![1, 2, 6]);

        // Vote on a new slot, only those later than 6 should show up. 4 is skipped.
        diff.clear();
        let (vote_pubkey, vote, _, signature) =
            vote_parser::parse_vote_transaction(&vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(1, 6), (2, 5), (3, 4), (4, 3), (7, 2), (8, 1)]),
                Hash::default(),
                &validator0_keypairs.node_keypair,
                &validator0_keypairs.vote_keypair,
                &validator0_keypairs.vote_keypair,
                None,
            ))
            .unwrap();

        ClusterInfoVoteListener::track_new_votes_and_notify_confirmations(
            vote,
            &vote_pubkey,
            signature,
            &vote_tracker,
            &bank,
            &notifiers,
            &mut diff,
            &mut new_optimistic_confirmed_slots,
            &mut latest_vote_slot_per_validator,
        );
        assert_eq!(diff.keys().copied().sorted().collect_vec(), vec![7, 8]);
    }
}
