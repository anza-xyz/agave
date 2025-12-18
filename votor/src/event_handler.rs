//! Handles incoming VotorEvents to take action or
//! notify block creation loop
use {
    crate::{
        commitment::{update_commitment_cache, CommitmentType},
        consensus_metrics::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        event::{CompletedBlock, VotorEvent, VotorEventReceiver},
        event_handler::stats::EventHandlerStats,
        root_utils::{self, RootContext, SetRootError},
        timer_manager::TimerManager,
        vote_history::{VoteHistory, VoteHistoryError},
        voting_service::BLSOp,
        voting_utils::{generate_vote_message, VoteError, VotingContext},
        votor::{SharedContext, Votor},
    },
    agave_votor_messages::{consensus_message::Block, vote::Vote},
    crossbeam_channel::{RecvTimeoutError, TrySendError},
    parking_lot::RwLock,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::leader_schedule_utils::{
        first_of_consecutive_leader_slots, last_of_consecutive_leader_slots, leader_slot_index,
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_signer::Signer,
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

mod stats;

/// Banks that have completed replay, but are yet to be voted on
/// in the form of (block, parent block)
pub(crate) type PendingBlocks = BTreeMap<Slot, Vec<(Block, Block)>>;

/// Inputs for the event handler thread
pub(crate) struct EventHandlerContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) start: Arc<(Mutex<bool>, Condvar)>,

    pub(crate) event_receiver: VotorEventReceiver,
    pub(crate) timer_manager: Arc<RwLock<TimerManager>>,

    // Contexts
    pub(crate) shared_context: SharedContext,
    pub(crate) voting_context: VotingContext,
    pub(crate) root_context: RootContext,
}

#[derive(Debug, Error)]
enum EventLoopError {
    #[error("Receiver is disconnected")]
    ReceiverDisconnected(#[from] RecvTimeoutError),

    #[error("Sender is disconnected")]
    SenderDisconnected,

    #[error("Error generating and inserting vote")]
    VotingError(#[from] VoteError),

    #[error("Set identity error")]
    SetIdentityError(#[from] VoteHistoryError),

    #[error("Set root error: {0}")]
    SetRoot(#[from] SetRootError),
}

pub(crate) struct EventHandler {
    t_event_handler: JoinHandle<()>,
}

struct LocalContext {
    pub(crate) my_pubkey: Pubkey,
    pub(crate) pending_blocks: PendingBlocks,
    pub(crate) finalized_blocks: BTreeSet<Block>,
    pub(crate) received_shred: BTreeSet<Slot>,
    pub(crate) stats: EventHandlerStats,
}

impl EventHandler {
    pub(crate) fn new(ctx: EventHandlerContext) -> Self {
        let exit = ctx.exit.clone();
        let t_event_handler = Builder::new()
            .name("solVotorEvLoop".to_string())
            .spawn(move || {
                info!("EventHandler has started");
                if let Err(e) = Self::event_loop(ctx) {
                    exit.store(true, Ordering::Relaxed);
                    error!("EventHandler exited with error: {e}");
                }
                info!("EventHandler has stopped");
            })
            .unwrap();

        Self { t_event_handler }
    }

    fn event_loop(context: EventHandlerContext) -> Result<(), EventLoopError> {
        let EventHandlerContext {
            exit,
            start,
            event_receiver,
            timer_manager,
            shared_context: ctx,
            voting_context: mut vctx,
            root_context: rctx,
        } = context;
        let mut local_context = LocalContext {
            my_pubkey: ctx.cluster_info.keypair().pubkey(),
            pending_blocks: PendingBlocks::default(),
            finalized_blocks: BTreeSet::default(),
            received_shred: BTreeSet::default(),
            stats: EventHandlerStats::new(),
        };

        // Wait until migration has completed
        info!("{}: Event loop initialized", local_context.my_pubkey);
        Votor::wait_for_migration_or_exit(&exit, &start);
        info!("{}: Event loop starting", local_context.my_pubkey);

        while !exit.load(Ordering::Relaxed) {
            let mut receive_event_time = Measure::start("receive_event");
            let event = match event_receiver.recv_timeout(Duration::from_secs(1)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => return Err(EventLoopError::ReceiverDisconnected(e)),
            };
            receive_event_time.stop();
            local_context.stats.receive_event_time_us = local_context
                .stats
                .receive_event_time_us
                .saturating_add(receive_event_time.as_us());

            let root_bank = vctx.sharable_banks.root();
            if event.should_ignore(root_bank.slot()) {
                local_context.stats.ignored = local_context.stats.ignored.saturating_add(1);
                continue;
            }

            let mut event_processing_time = Measure::start("event_processing");
            let stats_event = local_context.stats.handle_event_arrival(&event);
            let votes = Self::handle_event(
                event,
                &timer_manager,
                &ctx,
                &mut vctx,
                &rctx,
                &mut local_context,
            )?;
            event_processing_time.stop();
            local_context
                .stats
                .incr_event_with_timing(stats_event, event_processing_time.as_us());

            let mut send_votes_batch_time = Measure::start("send_votes_batch");
            for vote in votes {
                local_context.stats.incr_vote(&vote);
                vctx.bls_sender
                    .send(vote)
                    .map_err(|_| EventLoopError::SenderDisconnected)?;
            }
            send_votes_batch_time.stop();
            local_context.stats.send_votes_batch_time_us = local_context
                .stats
                .send_votes_batch_time_us
                .saturating_add(send_votes_batch_time.as_us());
            local_context.stats.maybe_report();
        }

        Ok(())
    }

    fn handle_parent_ready_event(
        slot: Slot,
        parent_block: Block,
        vctx: &mut VotingContext,
        ctx: &SharedContext,
        local_context: &mut LocalContext,
        timer_manager: &RwLock<TimerManager>,
        votes: &mut Vec<BLSOp>,
    ) -> Result<(), EventLoopError> {
        let my_pubkey = &local_context.my_pubkey;
        info!("{my_pubkey}: Parent ready {slot} {parent_block:?}");
        let should_set_timeouts = vctx.vote_history.add_parent_ready(slot, parent_block);
        Self::check_pending_blocks(my_pubkey, &mut local_context.pending_blocks, vctx, votes)?;
        if should_set_timeouts {
            timer_manager.write().set_timeouts(slot);
            local_context.stats.timeout_set = local_context.stats.timeout_set.saturating_add(1);
        }
        let mut highest_parent_ready = ctx
            .leader_window_notifier
            .highest_parent_ready
            .write()
            .unwrap();

        let (current_slot, _) = *highest_parent_ready;

        if slot > current_slot {
            *highest_parent_ready = (slot, parent_block);
        }
        Ok(())
    }

    fn send_to_metrics(
        consensus_metrics_sender: &ConsensusMetricsEventSender,
        consensus_metrics_events: Vec<ConsensusMetricsEvent>,
        stats: &mut EventHandlerStats,
    ) -> Result<(), EventLoopError> {
        // Do not kill or block event handler threads just because metrics
        // send failed (maybe because the queue is full).
        match consensus_metrics_sender.try_send((Instant::now(), consensus_metrics_events)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(EventLoopError::SenderDisconnected),
            Err(TrySendError::Full(_)) => {
                warn!("send_to_metrics failed: queue is full");
                stats.metrics_queue_became_full = true;
                Ok(())
            }
        }
    }

    fn handle_event(
        event: VotorEvent,
        timer_manager: &RwLock<TimerManager>,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        rctx: &RootContext,
        local_context: &mut LocalContext,
    ) -> Result<Vec<BLSOp>, EventLoopError> {
        let mut votes = vec![];
        let LocalContext {
            my_pubkey,
            pending_blocks,
            finalized_blocks,
            received_shred,
            stats,
        } = local_context;
        match event {
            // Block has completed replay
            VotorEvent::Block(CompletedBlock { slot, bank }) => {
                debug_assert!(bank.is_frozen());
                let mut consensus_metrics_events =
                    vec![ConsensusMetricsEvent::StartOfSlot { slot }];
                if slot == first_of_consecutive_leader_slots(slot) {
                    // all slots except the first in the window would typically start when
                    // the block is seen so the recording would essentially record 0.
                    // hence we skip it.
                    consensus_metrics_events.push(ConsensusMetricsEvent::BlockHashSeen {
                        leader: *bank.leader_id(),
                        slot,
                    });
                }
                consensus_metrics_events.push(ConsensusMetricsEvent::MaybeNewEpoch {
                    epoch: bank.epoch(),
                });
                Self::send_to_metrics(
                    &vctx.consensus_metrics_sender,
                    consensus_metrics_events,
                    stats,
                )?;
                let (block, parent_block) = Self::get_block_parent_block(&bank);
                info!("{my_pubkey}: Block {block:?} parent {parent_block:?}");
                if Self::try_notar(
                    my_pubkey,
                    block,
                    parent_block,
                    pending_blocks,
                    vctx,
                    &mut votes,
                )? {
                    Self::check_pending_blocks(my_pubkey, pending_blocks, vctx, &mut votes)?;
                } else if !vctx.vote_history.voted(slot) {
                    pending_blocks
                        .entry(slot)
                        .or_default()
                        .push((block, parent_block));
                }
                Self::check_rootable_blocks(
                    my_pubkey,
                    ctx,
                    vctx,
                    rctx,
                    pending_blocks,
                    finalized_blocks,
                    received_shred,
                    stats,
                )?;
                if let Some((ready_slot, parent_block)) =
                    Self::add_missing_parent_ready(block, ctx, vctx, local_context)
                {
                    Self::handle_parent_ready_event(
                        ready_slot,
                        parent_block,
                        vctx,
                        ctx,
                        local_context,
                        timer_manager,
                        &mut votes,
                    )?;
                }
            }

            // Block has received a notarization certificate
            VotorEvent::BlockNotarized(block) => {
                info!("{my_pubkey}: Block Notarized {block:?}");
                vctx.vote_history.add_block_notarized(block);
                Self::try_final(my_pubkey, block, vctx, &mut votes)?;
            }

            VotorEvent::FirstShred(slot) => {
                info!("{my_pubkey}: First shred {slot}");
                received_shred.insert(slot);
            }

            // Received a parent ready notification for `slot`
            VotorEvent::ParentReady { slot, parent_block } => {
                Self::send_to_metrics(
                    &vctx.consensus_metrics_sender,
                    vec![ConsensusMetricsEvent::StartOfSlot { slot }],
                    stats,
                )?;
                Self::handle_parent_ready_event(
                    slot,
                    parent_block,
                    vctx,
                    ctx,
                    local_context,
                    timer_manager,
                    &mut votes,
                )?;
            }

            VotorEvent::TimeoutCrashedLeader(slot) => {
                info!("{my_pubkey}: TimeoutCrashedLeader {slot}");
                if vctx.vote_history.voted(slot) || received_shred.contains(&slot) {
                    return Ok(votes);
                }
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes)?;
            }

            // Skip timer for the slot has fired
            VotorEvent::Timeout(slot) => {
                info!("{my_pubkey}: Timeout {slot}");
                if slot != last_of_consecutive_leader_slots(slot) {
                    Self::send_to_metrics(
                        &vctx.consensus_metrics_sender,
                        vec![ConsensusMetricsEvent::StartOfSlot {
                            slot: slot.saturating_add(1),
                        }],
                        stats,
                    )?;
                }
                if vctx.vote_history.voted(slot) {
                    return Ok(votes);
                }
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes)?;
            }

            // We have observed the safe to notar condition, and can send a notar fallback vote
            // TODO: update cert pool to check parent block id for intra window slots
            VotorEvent::SafeToNotar(block @ (slot, block_id)) => {
                info!("{my_pubkey}: SafeToNotar {block:?}");
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes)?;
                if vctx.vote_history.its_over(slot)
                    || vctx.vote_history.voted_notar_fallback(slot, block_id)
                {
                    return Ok(votes);
                }
                info!("{my_pubkey}: Voting notarize-fallback for {slot} {block_id}");
                if let Some(bls_op) = generate_vote_message(
                    Vote::new_notarization_fallback_vote(slot, block_id),
                    false,
                    vctx,
                )? {
                    votes.push(bls_op);
                }
            }

            // We have observed the safe to skip condition, and can send a skip fallback vote
            VotorEvent::SafeToSkip(slot) => {
                info!("{my_pubkey}: SafeToSkip {slot}");
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes)?;
                if vctx.vote_history.its_over(slot) || vctx.vote_history.voted_skip_fallback(slot) {
                    return Ok(votes);
                }
                info!("{my_pubkey}: Voting skip-fallback for {slot}");
                if let Some(bls_op) =
                    generate_vote_message(Vote::new_skip_fallback_vote(slot), false, vctx)?
                {
                    votes.push(bls_op);
                }
            }

            // It is time to produce our leader window
            VotorEvent::ProduceWindow(window_info) => {
                info!("{my_pubkey}: ProduceWindow {window_info:?}");
                let mut l_window_info = ctx.leader_window_notifier.window_info.lock().unwrap();
                if let Some(old_window_info) = l_window_info.as_ref() {
                    stats.leader_window_replaced = stats.leader_window_replaced.saturating_add(1);
                    error!(
                        "{my_pubkey}: Attempting to start leader window for {}-{}, however there \
                         is already a pending window to produce {}-{}. Our production is lagging, \
                         discarding in favor of the newer window",
                        window_info.start_slot,
                        window_info.end_slot,
                        old_window_info.start_slot,
                        old_window_info.end_slot,
                    );
                }
                *l_window_info = Some(window_info);
                ctx.leader_window_notifier.window_notification.notify_one();
            }

            // We have finalized this block consider it for rooting
            VotorEvent::Finalized(block, is_fast_finalization) => {
                info!("{my_pubkey}: Finalized {block:?} fast: {is_fast_finalization}");
                finalized_blocks.insert(block);
                Self::check_rootable_blocks(
                    my_pubkey,
                    ctx,
                    vctx,
                    rctx,
                    pending_blocks,
                    finalized_blocks,
                    received_shred,
                    stats,
                )?;
                if let Some((slot, block)) =
                    Self::add_missing_parent_ready(block, ctx, vctx, local_context)
                {
                    Self::handle_parent_ready_event(
                        slot,
                        block,
                        vctx,
                        ctx,
                        local_context,
                        timer_manager,
                        &mut votes,
                    )?;
                }
            }

            // We have not observed a finalization certificate in a while, refresh our votes
            VotorEvent::Standstill(highest_finalized_slot) => {
                info!("{my_pubkey}: Standstill {highest_finalized_slot}");
                // certs refresh happens in CertificatePoolService
                Self::refresh_votes(my_pubkey, highest_finalized_slot, vctx, &mut votes)?;
            }

            // Operator called set identity make sure that our keypair is updated for voting
            VotorEvent::SetIdentity => {
                info!("{my_pubkey}: SetIdentity");
                if let Err(e) = Self::handle_set_identity(my_pubkey, ctx, vctx) {
                    error!(
                        "Unable to load new vote history when attempting to change identity from \
                         {} to {} in voting loop, Exiting: {}",
                        vctx.vote_history.node_pubkey,
                        ctx.cluster_info.id(),
                        e
                    );
                    return Err(EventLoopError::SetIdentityError(e));
                }
            }
        }
        Ok(votes)
    }

    /// Under normal cases we should have a parent ready for first slot of every window.
    /// But it could be we joined when the later slots of the window are finalized, then
    /// we never saw the parent ready for the first slot and haven't voted for first slot
    /// so we can't keep processing rest of the window. This is especially a problem for
    /// cluster standstill.
    /// For example:
    ///    A 40%
    ///    B 40%
    ///    C 30%
    /// A and B finalize block together up to slot 9, now A exited and C joined.
    /// C sees block 9 as finalized, but it never had parent ready triggered for slot 8.
    /// C can't vote for any slot in the window because there is no parent ready for slot 8.
    /// While B is stuck because it is waiting for >60% of the votes to finalize slot 9.
    /// The cluster will get stuck.
    /// After we add the following function, C will see that block 9 is finalized yet
    /// it never had parent ready for slot 9, so it will trigger parent ready for slot 9,
    /// this means C will immediately vote Notarize for  slot 9, then vote Notarize for
    /// all later slots. So B and C together can keep finalizing the blocks and unstuck the
    /// cluster. If we get a finalization cert for later slots of the window and we have the
    /// block replayed, trace back to the first slot of the window and emit parent ready.
    fn add_missing_parent_ready(
        finalized_block: Block,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        local_context: &mut LocalContext,
    ) -> Option<(Slot, Block)> {
        let (slot, block_id) = finalized_block;
        let first_slot_of_window = first_of_consecutive_leader_slots(slot);
        if first_slot_of_window == slot || first_slot_of_window == 0 {
            // No need to trigger parent ready for the first slot of the window
            return None;
        }
        if vctx.vote_history.highest_parent_ready_slot() >= Some(first_slot_of_window)
            || !local_context.finalized_blocks.contains(&finalized_block)
        {
            return None;
        }
        // If the block is missing, we can't trigger parent ready
        let bank = ctx.bank_forks.read().unwrap().get(slot)?;
        if !bank.is_frozen() {
            // We haven't finished replay for the block, so we can't trigger parent ready
            return None;
        }
        if bank.block_id() != Some(block_id) {
            // We have a different block id for the slot, repair should kick in later
            return None;
        }
        let parent_bank = bank.parent()?;
        let parent_slot = parent_bank.slot();
        let Some(parent_block_id) = parent_bank.block_id() else {
            // Maybe this bank is set to root after we drop bank_forks.
            error!(
                "{}: Unable to find block id for parent bank {parent_slot} to trigger parent ready",
                local_context.my_pubkey
            );
            return None;
        };
        info!(
            "{}: Triggering parent ready for slot {slot} with parent {parent_slot} \
             {parent_block_id}",
            local_context.my_pubkey
        );
        Some((slot, (parent_slot, parent_block_id)))
    }

    fn handle_set_identity(
        my_pubkey: &mut Pubkey,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
    ) -> Result<(), VoteHistoryError> {
        let new_identity = ctx.cluster_info.keypair();
        let new_pubkey = new_identity.pubkey();
        // This covers both:
        // - startup set-identity so that vote_history is outdated but my_pubkey == new_pubkey
        // - set-identity during normal operation, vote_history == my_pubkey != new_pubkey
        if *my_pubkey != new_pubkey || vctx.vote_history.node_pubkey != new_pubkey {
            let my_old_pubkey = vctx.vote_history.node_pubkey;
            *my_pubkey = new_pubkey;
            // The vote history file for the new identity must exist for set-identity to succeed
            vctx.vote_history = VoteHistory::restore(ctx.vote_history_storage.as_ref(), my_pubkey)?;
            vctx.identity_keypair = new_identity.clone();
            warn!("set-identity: from {my_old_pubkey} to {my_pubkey}");
        }
        Ok(())
    }

    fn get_block_parent_block(bank: &Bank) -> (Block, Block) {
        let slot = bank.slot();
        let block = (
            slot,
            bank.block_id().expect("Block id must be set upstream"),
        );
        let parent_slot = bank.parent_slot();
        let parent_block_id = bank.parent_block_id().unwrap_or_else(|| {
            // To account for child of genesis and snapshots we insert a
            // default block id here. Charlie is working on a SIMD to add block
            // id to snapshots, which can allow us to remove this and update
            // the default case in parent ready tracker.
            trace!("Using default block id for {slot} parent {parent_slot}");
            Hash::default()
        });
        let parent_block = (parent_slot, parent_block_id);
        (block, parent_block)
    }

    /// Tries to vote notarize on `block`:
    /// - We have not voted notarize or skip for `slot(block)`
    /// - Either it's the first leader block of the window and we are parent ready
    /// - or it's a consecutive slot and we have voted notarize on the parent
    ///
    /// The boolean in the Result indicates whether we actually voted notarize.
    /// An error returned will cause the voting process to be aborted.
    fn try_notar(
        my_pubkey: &Pubkey,
        (slot, block_id): Block,
        parent_block @ (parent_slot, parent_block_id): Block,
        pending_blocks: &mut PendingBlocks,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<bool, VoteError> {
        if voting_context.vote_history.voted(slot) {
            return Ok(false);
        }

        if leader_slot_index(slot) == 0 || slot == 1 {
            if !voting_context
                .vote_history
                .is_parent_ready(slot, &parent_block)
            {
                // Need to ingest more certificates first
                return Ok(false);
            }
        } else {
            if parent_slot.saturating_add(1) != slot {
                // Non consecutive
                return Ok(false);
            }
            if voting_context.vote_history.voted_notar(parent_slot) != Some(parent_block_id) {
                // Voted skip, or notarize on a different version of the parent
                return Ok(false);
            }
        }

        info!("{my_pubkey}: Voting notarize for {slot} {block_id}");
        if let Some(bls_op) = generate_vote_message(
            Vote::new_notarization_vote(slot, block_id),
            false,
            voting_context,
        )? {
            votes.push(bls_op);
        }
        update_commitment_cache(
            CommitmentType::Notarize,
            slot,
            &voting_context.commitment_sender,
        )?;
        pending_blocks.remove(&slot);

        Self::try_final(my_pubkey, (slot, block_id), voting_context, votes)?;

        Ok(true)
    }

    /// Checks the pending blocks that have completed replay to see if they
    /// are eligible to be voted on now
    fn check_pending_blocks(
        my_pubkey: &Pubkey,
        pending_blocks: &mut PendingBlocks,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<(), VoteError> {
        let blocks_to_check: Vec<(Block, Block)> = pending_blocks
            .values()
            .flat_map(|blocks| blocks.iter())
            .copied()
            .collect();

        for (block, parent_block) in blocks_to_check {
            Self::try_notar(
                my_pubkey,
                block,
                parent_block,
                pending_blocks,
                voting_context,
                votes,
            )?;
        }
        Ok(())
    }

    /// Tries to send a finalize vote for the block if
    /// - the block has a notarization certificate
    /// - we have not already voted finalize
    /// - we voted notarize for the block
    /// - we have not voted skip, notarize fallback or skip fallback in the slot (bad window)
    ///
    /// The boolean in the Result indicates whether we actually voted finalize.
    /// An error returned will cause the voting process to be aborted.
    fn try_final(
        my_pubkey: &Pubkey,
        block @ (slot, block_id): Block,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<bool, VoteError> {
        if !voting_context.vote_history.is_block_notarized(&block)
            || voting_context.vote_history.its_over(slot)
            || voting_context.vote_history.bad_window(slot)
        {
            return Ok(false);
        }

        if voting_context
            .vote_history
            .voted_notar(slot)
            .is_none_or(|bid| bid != block_id)
        {
            return Ok(false);
        }

        info!("{my_pubkey}: Voting finalize for {slot}");
        if let Some(bls_op) =
            generate_vote_message(Vote::new_finalization_vote(slot), false, voting_context)?
        {
            votes.push(bls_op);
        }
        Ok(true)
    }

    fn try_skip_window(
        my_pubkey: &Pubkey,
        slot: Slot,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<(), VoteError> {
        // In case we set root in the middle of a leader window,
        // it's not necessary to vote skip prior to it and we won't
        // be able to check vote history if we've already voted on it
        let root_bank = voting_context.sharable_banks.root();
        // No matter what happens, we should not vote skip for slot 0
        let start = first_of_consecutive_leader_slots(slot)
            .max(root_bank.slot())
            .max(1);
        for s in start..=last_of_consecutive_leader_slots(slot) {
            if voting_context.vote_history.voted(s) {
                continue;
            }
            info!("{my_pubkey}: Voting skip for {s}");
            if let Some(bls_op) =
                generate_vote_message(Vote::new_skip_vote(s), false, voting_context)?
            {
                votes.push(bls_op);
            }
        }
        Ok(())
    }

    /// Refresh all votes cast for slots > highest_finalized_slot
    fn refresh_votes(
        my_pubkey: &Pubkey,
        highest_finalized_slot: Slot,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<(), VoteError> {
        for vote in voting_context
            .vote_history
            .votes_cast_since(highest_finalized_slot)
        {
            info!("{my_pubkey}: Refreshing vote {vote:?}");
            if let Some(bls_op) = generate_vote_message(vote, true, voting_context)? {
                votes.push(bls_op);
            }
        }
        Ok(())
    }

    /// Checks if we can set root on a new block. The block must:
    /// - Be present in bank forks
    /// - Newer than the current root
    /// - Already been voted on (bank.slot())
    /// - Have its Bank frozen
    /// - Finished shredding
    /// - Have a finalization certificate (determined by presence in
    ///   `finalized_blocks`)
    ///
    /// If so, set root on the highest block that fits these conditions.
    fn check_rootable_blocks(
        my_pubkey: &Pubkey,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        rctx: &RootContext,
        pending_blocks: &mut PendingBlocks,
        finalized_blocks: &mut BTreeSet<Block>,
        received_shred: &mut BTreeSet<Slot>,
        stats: &mut EventHandlerStats,
    ) -> Result<(), EventLoopError> {
        let bank_forks_r = ctx.bank_forks.read().unwrap();
        let old_root = bank_forks_r.root();
        let Some(new_root) = finalized_blocks
            .iter()
            .filter_map(|&(slot, block_id)| {
                let bank = bank_forks_r.get(slot)?;
                (slot > old_root
                    && vctx.vote_history.voted(slot)
                    && bank.is_frozen()
                    && bank.block_id().is_some_and(|bid| bid == block_id))
                .then_some(slot)
            })
            .max()
        else {
            // No rootable banks
            return Ok(());
        };
        drop(bank_forks_r);
        let set_root_result = root_utils::set_root(
            my_pubkey,
            new_root,
            ctx,
            vctx,
            rctx,
            pending_blocks,
            finalized_blocks,
            received_shred,
        )
        .map_err(EventLoopError::SetRoot);

        if set_root_result.is_ok() {
            stats.set_root(new_root)
        }

        set_root_result
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_event_handler.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            commitment::CommitmentAggregationData,
            consensus_metrics::ConsensusMetricsEventReceiver,
            event::LeaderWindowInfo,
            vote_history_storage::{
                FileVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
                VoteHistoryStorage,
            },
            voting_service::BLSOp,
            votor::LeaderWindowNotifier,
        },
        agave_votor_messages::{
            consensus_message::{ConsensusMessage, VoteMessage, BLS_KEYPAIR_DERIVE_SEED},
            vote::Vote,
        },
        crossbeam_channel::{bounded, Receiver, TryRecvError},
        parking_lot::RwLock as PlRwLock,
        solana_bls_signatures::{
            keypair::Keypair as BLSKeypair, signature::Signature as BLSSignature,
        },
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::Blockstore, blockstore_options::BlockstoreOptions, get_tmp_ledger_path,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
            installed_scheduler_pool::BankWithScheduler,
        },
        std::{
            collections::HashMap,
            fs::remove_file,
            path::PathBuf,
            sync::{Arc, RwLock},
            time::Instant,
        },
    };

    struct EventHandlerTestContext {
        exit: Arc<AtomicBool>,
        bls_receiver: Receiver<BLSOp>,
        commitment_receiver: Receiver<CommitmentAggregationData>,
        own_vote_receiver: Receiver<ConsensusMessage>,
        bank_forks: Arc<RwLock<BankForks>>,
        my_bls_keypair: BLSKeypair,
        timer_manager: Arc<PlRwLock<TimerManager>>,
        leader_window_notifier: Arc<LeaderWindowNotifier>,
        drop_bank_receiver: Receiver<Vec<BankWithScheduler>>,
        cluster_info: Arc<ClusterInfo>,
        consensus_metrics_receiver: ConsensusMetricsEventReceiver,
        shared_context: SharedContext,
        voting_context: VotingContext,
        root_context: RootContext,
        local_context: LocalContext,
        bls_ops: Vec<BLSOp>,
    }

    impl EventHandlerTestContext {
        fn setup() -> EventHandlerTestContext {
            // For tests, we just make each queue bounded at 100, should be enough.
            let (bls_sender, bls_receiver) = bounded(100);
            let (commitment_sender, commitment_receiver) = bounded(100);
            let (own_vote_sender, own_vote_receiver) = bounded(100);
            let (drop_bank_sender, drop_bank_receiver) = bounded(100);
            let exit = Arc::new(AtomicBool::new(false));
            let (event_sender, _event_receiver) = bounded(100);
            let (consensus_metrics_sender, consensus_metrics_receiver) = bounded(100);
            let timer_manager = Arc::new(PlRwLock::new(TimerManager::new(
                event_sender.clone(),
                exit.clone(),
            )));

            // Create 10 node validatorvotekeypairs vec
            let validator_keypairs = (0..10)
                .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
                .collect::<Vec<_>>();
            let stakes = (0..validator_keypairs.len())
                .rev()
                .map(|i| 100_u64.saturating_add(i as u64))
                .collect::<Vec<_>>();
            let genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                stakes,
            );
            let my_index = 0;
            let my_node_keypair = validator_keypairs[my_index].node_keypair.insecure_clone();
            let my_vote_keypair = validator_keypairs[my_index].vote_keypair.insecure_clone();
            let my_bls_keypair =
                BLSKeypair::derive_from_signer(&my_vote_keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap();
            let bank0 = Bank::new_for_tests(&genesis.genesis_config);
            let bank_forks = BankForks::new_rw_arc(bank0);
            let contact_info = ContactInfo::new_localhost(&my_node_keypair.pubkey(), 0);
            let cluster_info = Arc::new(ClusterInfo::new(
                contact_info,
                Arc::new(my_node_keypair.insecure_clone()),
                SocketAddrSpace::Unspecified,
            ));
            let blockstore = Arc::new(
                Blockstore::open_with_options(
                    &get_tmp_ledger_path!(),
                    BlockstoreOptions::default_for_tests(),
                )
                .unwrap(),
            );

            let leader_window_notifier = Arc::new(LeaderWindowNotifier::default());
            let shared_context = SharedContext {
                cluster_info: cluster_info.clone(),
                bank_forks: bank_forks.clone(),
                vote_history_storage: Arc::new(FileVoteHistoryStorage::default()),
                leader_window_notifier: leader_window_notifier.clone(),
                blockstore,
                rpc_subscriptions: None,
            };

            let vote_history = VoteHistory::new(my_node_keypair.pubkey(), 0);
            let voting_context = VotingContext {
                identity_keypair: Arc::new(my_node_keypair.insecure_clone()),
                sharable_banks: bank_forks.read().unwrap().sharable_banks(),
                vote_history,
                bls_sender,
                commitment_sender,
                vote_account_pubkey: my_vote_keypair.pubkey(),
                wait_to_vote_slot: None,
                authorized_voter_keypairs: Arc::new(RwLock::new(vec![Arc::new(my_vote_keypair)])),
                derived_bls_keypairs: HashMap::new(),
                has_new_vote_been_rooted: false,
                own_vote_sender,
                consensus_metrics_sender,
            };

            let root_context = RootContext {
                leader_schedule_cache: Arc::new(LeaderScheduleCache::new_from_bank(
                    &bank_forks.read().unwrap().root_bank(),
                )),
                snapshot_controller: None,
                bank_notification_sender: None,
                drop_bank_sender,
            };

            let local_context = LocalContext {
                my_pubkey: my_node_keypair.pubkey(),
                pending_blocks: BTreeMap::new(),
                finalized_blocks: BTreeSet::new(),
                received_shred: BTreeSet::new(),
                stats: EventHandlerStats::default(),
            };

            EventHandlerTestContext {
                exit,
                bls_receiver,
                commitment_receiver,
                own_vote_receiver,
                bank_forks,
                my_bls_keypair,
                timer_manager,
                leader_window_notifier,
                drop_bank_receiver,
                cluster_info,
                consensus_metrics_receiver,
                shared_context,
                voting_context,
                root_context,
                local_context,
                bls_ops: vec![],
            }
        }

        fn send_parent_ready_event(&mut self, slot: Slot, parent_block: Block) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::ParentReady { slot, parent_block },
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_timeout_event(&mut self, slot: Slot) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::Timeout(slot),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_block_event(&mut self, slot: Slot, bank: Arc<Bank>) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::Block(CompletedBlock { slot, bank }),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_block_notarized_event(&mut self, block: Block) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::BlockNotarized(block),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_timeout_crashed_leader_event(&mut self, slot: Slot) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::TimeoutCrashedLeader(slot),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_first_shred_event(&mut self, slot: Slot) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::FirstShred(slot),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_safe_to_notar_event(&mut self, block: Block) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::SafeToNotar(block),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_safe_to_skip_event(&mut self, slot: Slot) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::SafeToSkip(slot),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_produce_window_event(
            &mut self,
            start_slot: Slot,
            end_slot: Slot,
            parent_block: Block,
        ) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::ProduceWindow(LeaderWindowInfo {
                    start_slot,
                    end_slot,
                    parent_block,
                    skip_timer: Instant::now(),
                }),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_finalized_event(&mut self, block: Block, is_fast_finalization: bool) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::Finalized(block, is_fast_finalization),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_set_identity_event(&mut self) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::SetIdentity,
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn send_standstill_event(&mut self, highest_finalized_slot: Slot) {
            let mut new_ops = EventHandler::handle_event(
                VotorEvent::Standstill(highest_finalized_slot),
                &self.timer_manager,
                &self.shared_context,
                &mut self.voting_context,
                &self.root_context,
                &mut self.local_context,
            )
            .unwrap();
            self.bls_ops.append(&mut new_ops);
        }

        fn create_block_only(&mut self, slot: Slot, parent_bank: Arc<Bank>) -> Arc<Bank> {
            let bank = Bank::new_from_parent(parent_bank, &Pubkey::new_unique(), slot);
            bank.set_block_id(Some(Hash::new_unique()));
            bank.freeze();
            let mut bank_forks_w = self.bank_forks.write().unwrap();
            bank_forks_w.insert(bank);
            bank_forks_w.get(slot).unwrap()
        }

        fn create_block_and_send_block_event(
            &mut self,
            slot: Slot,
            parent_bank: Arc<Bank>,
        ) -> Arc<Bank> {
            let bank = self.create_block_only(slot, parent_bank);
            self.send_block_event(slot, bank.clone());
            bank
        }

        fn check_for_votes(&mut self, expected_votes: &[Vote]) {
            for v in expected_votes {
                let expected_vote_serialized = bincode::serialize(v).unwrap();
                let signature: BLSSignature =
                    self.my_bls_keypair.sign(&expected_vote_serialized).into();
                let expected_message = ConsensusMessage::Vote(VoteMessage {
                    vote: *v,
                    rank: 0,
                    signature,
                });
                let prev_length = self.bls_ops.len();
                self.bls_ops.retain(|bls_op| {
                    !matches!(bls_op, BLSOp::PushVote { message, .. } if **message == expected_message)
                });
                assert!(
                    self.bls_ops.len() < prev_length,
                    "Did not find expected vote: {expected_message:?}",
                );
            }
        }

        fn check_for_vote(&mut self, expected_vote: &Vote) {
            let expected_vote_serialized = bincode::serialize(expected_vote).unwrap();
            let signature: BLSSignature =
                self.my_bls_keypair.sign(&expected_vote_serialized).into();
            let expected_message = ConsensusMessage::Vote(VoteMessage {
                vote: *expected_vote,
                rank: 0,
                signature,
            });
            let prev_length = self.bls_ops.len();
            self.bls_ops.retain(|bls_op| {
                !matches!(bls_op, BLSOp::PushVote { message, .. } if **message == expected_message)
            });
            assert!(
                self.bls_ops.len() < prev_length,
                "Did not find expected vote: {expected_message:?}",
            );
            // Also check own_vote_receiver
            let own_vote = self.own_vote_receiver.try_recv().unwrap();
            assert_eq!(own_vote, expected_message);
        }

        fn check_for_commitment(&mut self, expected_type: CommitmentType, expected_slot: Slot) {
            let commitment = self.commitment_receiver.try_recv().unwrap();
            assert_eq!(commitment.commitment_type, expected_type);
            assert_eq!(commitment.slot, expected_slot);
        }

        fn check_no_vote_or_commitment(&self) {
            assert_eq!(
                self.bls_receiver.try_recv().err(),
                Some(TryRecvError::Empty)
            );
            assert_eq!(
                self.commitment_receiver.try_recv().err(),
                Some(TryRecvError::Empty)
            );
        }

        fn check_parent_ready_slot(&mut self, expected: (Slot, Block)) {
            assert_eq!(
                *self
                    .leader_window_notifier
                    .highest_parent_ready
                    .read()
                    .unwrap(),
                expected
            );
            let slot = expected.0;
            self.check_timeout_set(slot);
        }

        fn check_timeout_set(&mut self, expected_slot: Slot) {
            assert!(self.timer_manager.read().is_timeout_set(expected_slot));
        }

        fn check_for_metrics_event(&mut self, expected: ConsensusMetricsEvent) {
            let event = self
                .consensus_metrics_receiver
                .try_recv()
                .expect("Should receive metrics event");
            assert!(event.1.contains(&expected));
        }

        fn create_vote_history_storage_and_switch_identity(
            &mut self,
            new_identity: &Keypair,
        ) -> PathBuf {
            let file_vote_history_storage = FileVoteHistoryStorage::default();
            let saved_vote_history =
                SavedVoteHistory::new(&VoteHistory::new(new_identity.pubkey(), 0), &new_identity)
                    .unwrap();
            assert!(file_vote_history_storage
                .store(&SavedVoteHistoryVersions::from(saved_vote_history),)
                .is_ok());
            self.cluster_info
                .set_keypair(Arc::new(new_identity.insecure_clone()));
            self.send_set_identity_event();
            file_vote_history_storage.filename(&new_identity.pubkey())
        }
    }

    #[test]
    fn test_received_block_event_and_parent_ready_event() {
        // Test different orders of received block event and parent ready event
        // some will send Notarize immediately, some will wait for parent ready
        let mut test_context = EventHandlerTestContext::setup();
        // Received block event which says block has completed replay

        // If there is a parent ready for block 1 Notarization is sent out.
        let slot = 1;
        let parent_slot = 0;
        test_context.send_parent_ready_event(slot, (parent_slot, Hash::default()));
        test_context.check_parent_ready_slot((slot, (parent_slot, Hash::default())));
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(slot, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        test_context.check_for_metrics_event(ConsensusMetricsEvent::StartOfSlot { slot });

        // We should receive Notarize Vote for block 1
        test_context.check_for_vote(&Vote::new_notarization_vote(slot, block_id_1));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);

        // Add block event for 1 again will not trigger another Notarize or commitment
        test_context.send_block_event(1, bank1.clone());
        test_context.check_no_vote_or_commitment();

        let slot = 2;
        let bank2 = test_context.create_block_and_send_block_event(slot, bank1.clone());
        let block_id_2 = bank2.block_id().unwrap();

        // Because 2 is middle of window, we should see Notarize vote for block 2 even without parentready
        test_context.check_for_vote(&Vote::new_notarization_vote(slot, block_id_2));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);

        // Slot 3 somehow links to block 1, should not trigger Notarize vote because it has a wrong parent (not 2)
        let _ = test_context.create_block_and_send_block_event(3, bank1.clone());
        test_context.check_no_vote_or_commitment();

        // Slot 4 completed replay without parent ready or parent notarized should not trigger Notarize vote
        let slot = 4;
        let bank4 = test_context.create_block_and_send_block_event(slot, bank2.clone());
        let block_id_4 = bank4.block_id().unwrap();
        test_context.check_no_vote_or_commitment();

        // Send parent ready for slot 4 should trigger Notarize vote for slot 4
        test_context.send_parent_ready_event(slot, (2, block_id_2));
        test_context.check_parent_ready_slot((slot, (2, block_id_2)));
        test_context.check_for_vote(&Vote::new_notarization_vote(slot, block_id_4));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
    }

    #[test]
    fn test_received_block_notarized_and_timeout() {
        // Test block notarized event will trigger Finalize vote when all conditions are met
        // But it will not trigger Finalize if any of the conditions are not met
        let mut test_context = EventHandlerTestContext::setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        // Add parent ready for 0 to trigger notar vote for 1
        test_context.send_parent_ready_event(1, (0, Hash::default()));
        test_context.check_parent_ready_slot((1, (0, Hash::default())));
        test_context.check_for_vote(&Vote::new_notarization_vote(1, block_id_1));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Send block notarized event should trigger Finalize vote
        test_context.send_block_notarized_event((1, block_id_1));
        test_context.check_for_vote(&Vote::new_finalization_vote(1));

        let bank2 = test_context.create_block_and_send_block_event(2, bank1.clone());
        let block_id_2 = bank2.block_id().unwrap();
        // Both Notarize and Finalize votes should trigger for 2
        test_context.check_for_vote(&Vote::new_notarization_vote(2, block_id_2));
        test_context.check_for_commitment(CommitmentType::Notarize, 2);
        test_context.send_block_notarized_event((2, block_id_2));
        test_context.check_for_vote(&Vote::new_finalization_vote(2));

        // Create bank3 but do not Notarize, so Finalize vote should not trigger
        let slot = 3;
        let bank3 = test_context.create_block_only(slot, bank2.clone());
        let block_id_3 = bank3.block_id().unwrap();
        // Check no notarization vote for 3
        test_context.check_no_vote_or_commitment();

        test_context.send_block_notarized_event((slot, block_id_3));
        // Check no Finalize vote for 3
        test_context.check_no_vote_or_commitment();

        // Now send Block event simulating replay completed for 3
        test_context.send_block_event(slot, bank3.clone());
        // There should be a notarization vote for 3
        test_context.check_for_vote(&Vote::new_notarization_vote(slot, block_id_3));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
        // Check there is a Finalize vote for 3
        test_context.check_for_vote(&Vote::new_finalization_vote(slot));

        // After casting finalization vote for 3, we will not send skip fallback
        test_context.send_safe_to_skip_event(slot);
        test_context.check_no_vote_or_commitment();

        // Simulate that block 4 never arrives, we create block 4 but send timeout event
        let slot = 4;
        let bank4 = test_context.create_block_only(slot, bank3.clone());
        test_context.send_timeout_event(slot);
        // We did eventually complete replay for 4
        test_context.send_block_event(slot, bank4.clone());
        // There should be a skip vote for 4 to 7 each
        test_context.check_for_vote(&Vote::new_skip_vote(slot));
        test_context.check_for_vote(&Vote::new_skip_vote(slot + 1));
        test_context.check_for_vote(&Vote::new_skip_vote(slot + 2));
        test_context.check_for_vote(&Vote::new_skip_vote(slot + 3));

        // Now we get block 5, it's replayed and we get block_notarized, but since 4~7 is a bad
        // window already, we shouldn't have notarize or finalize vote for 5
        let slot = 5;
        let bank5 = test_context.create_block_only(slot, bank4.clone());
        test_context.send_block_event(slot, bank5.clone());
        test_context.check_no_vote_or_commitment();

        test_context.exit.store(true, Ordering::Relaxed);
    }

    #[test]
    fn test_received_timeout_crashed_leader_and_first_shred() {
        let mut test_context = EventHandlerTestContext::setup();

        // Simulate a crashed leader for slot 4
        test_context.send_timeout_crashed_leader_event(4);

        // Since we don't have any shred for block 4, we should vote skip for 4-7
        test_context.check_for_vote(&Vote::new_skip_vote(4));
        test_context.check_for_vote(&Vote::new_skip_vote(5));
        test_context.check_for_vote(&Vote::new_skip_vote(6));
        test_context.check_for_vote(&Vote::new_skip_vote(7));

        // Now if we received one shred for slot 8, then we should not do anything when
        // receiving timeout_crashed_leader_event for slot 8
        test_context.send_first_shred_event(8);
        test_context.send_timeout_crashed_leader_event(8);
        test_context.check_no_vote_or_commitment();
    }

    #[test]
    fn test_received_safe_to_notar() {
        let mut test_context = EventHandlerTestContext::setup();

        // We can theoretically not vote skip here and test will pass, but in real world
        // safe_to_notar event only fires after we voted skip for the whole window
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank_1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1_old = bank_1.block_id().unwrap();
        test_context.send_parent_ready_event(1, (0, Hash::default()));
        test_context.check_parent_ready_slot((1, (0, Hash::default())));
        test_context.check_for_vote(&Vote::new_notarization_vote(1, block_id_1_old));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now we got safe_to_notar event for slot 1 and a different block id
        let block_id_1_1 = Hash::new_unique();
        test_context.send_safe_to_notar_event((1, block_id_1_1));
        // We should see rest of the window skipped
        test_context.check_for_vote(&Vote::new_skip_vote(2));
        test_context.check_for_vote(&Vote::new_skip_vote(3));
        // We should also see notarize fallback for the new block id
        test_context.check_for_vote(&Vote::new_notarization_fallback_vote(1, block_id_1_1));

        // We can trigger safe_to_notar event again for a different block id
        // In this test you can trigger this any number of times, but the white paper
        // proved we can only get up to 3 different block ids on a slot, and our
        // certificate pool implementation checks that.
        let block_id_1_2 = Hash::new_unique();
        test_context.send_safe_to_notar_event((1, block_id_1_2));
        // No skips this time because we already skipped the rest of the window
        // We should also see notarize fallback for the new block id
        test_context.check_for_vote(&Vote::new_notarization_fallback_vote(1, block_id_1_2));

        // But getting safe_to_notar for a block id we voted before should be no-op
        test_context.send_safe_to_notar_event((1, block_id_1_1));
        test_context.check_no_vote_or_commitment();
    }

    #[test]
    fn test_received_safe_to_skip() {
        let mut test_context = EventHandlerTestContext::setup();

        // The safe_to_skip event only fires after we voted notarize for the slot
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank_1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank_1.block_id().unwrap();
        test_context.send_parent_ready_event(1, (0, Hash::default()));
        test_context.check_parent_ready_slot((1, (0, Hash::default())));
        test_context.check_for_vote(&Vote::new_notarization_vote(1, block_id_1));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now we got safe_to_skip event for slot 1
        test_context.send_safe_to_skip_event(1);
        // We should see rest of the window skipped
        test_context.check_for_vote(&Vote::new_skip_vote(2));
        test_context.check_for_vote(&Vote::new_skip_vote(3));
        // We should see skip fallback for slot 1
        test_context.check_for_vote(&Vote::new_skip_fallback_vote(1));

        // We can trigger safe_to_skip event again, this should be a no-op
        test_context.send_safe_to_skip_event(1);
        test_context.check_no_vote_or_commitment();
    }

    #[test]
    fn test_received_produce_window() {
        let mut test_context = EventHandlerTestContext::setup();

        // Produce a full window of blocks
        // Assume the leader for 1-3 is us, send produce window event
        test_context.send_produce_window_event(1, 3, (0, Hash::default()));

        // Check that leader_window_notifier is updated
        let mut guard = test_context
            .leader_window_notifier
            .window_info
            .lock()
            .unwrap();
        let received_leader_window_info = guard.take().unwrap();
        assert_eq!(received_leader_window_info.start_slot, 1);
        assert_eq!(received_leader_window_info.end_slot, 3);
        assert_eq!(
            received_leader_window_info.parent_block,
            (0, Hash::default())
        );
        drop(guard);

        // Suddenly I found out I produced block 1 already, send new produce window event
        let block_id_1 = Hash::new_unique();
        test_context.send_produce_window_event(2, 3, (1, block_id_1));
        let mut guard = test_context
            .leader_window_notifier
            .window_info
            .lock()
            .unwrap();
        let received_leader_window_info = guard.take().unwrap();
        assert_eq!(received_leader_window_info.start_slot, 2);
        assert_eq!(received_leader_window_info.end_slot, 3);
        assert_eq!(received_leader_window_info.parent_block, (1, block_id_1));
        drop(guard);
    }

    #[test]
    fn test_received_finalized() {
        agave_logger::setup();
        let mut test_context = EventHandlerTestContext::setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        test_context.send_parent_ready_event(1, (0, Hash::default()));
        test_context.check_parent_ready_slot((1, (0, Hash::default())));
        test_context.check_for_vote(&Vote::new_notarization_vote(1, block_id_1));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now we got finalized event for slot 1
        test_context.send_finalized_event((1, block_id_1), true);
        // Listen on drop bank receiver, it should get bank 0
        let dropped_banks = test_context.drop_bank_receiver.try_recv().unwrap();
        assert_eq!(dropped_banks.len(), 1);
        assert_eq!(dropped_banks[0].slot(), 0);
        // The bank forks root should be updated to 1
        assert_eq!(test_context.bank_forks.read().unwrap().root(), 1);
    }

    #[test]
    fn test_parent_ready_in_middle_of_window() {
        agave_logger::setup();
        let mut test_context = EventHandlerTestContext::setup();

        // We just woke up and received finalize for slot 5
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank4 = test_context.create_block_and_send_block_event(4, root_bank);
        let block_id_4 = bank4.block_id().unwrap();

        let bank5 = test_context.create_block_and_send_block_event(5, bank4.clone());
        let block_id_5 = bank5.block_id().unwrap();

        test_context.send_finalized_event((5, block_id_5), true);
        // We should now have parent ready for slot 5
        test_context.check_parent_ready_slot((5, (4, block_id_4)));

        // We are partitioned off from rest of the network, and suddenly received finalize for
        // slot 9 a little before we finished replay slot 9
        let bank9 = test_context.create_block_only(9, bank5.clone());
        let block_id_9 = bank9.block_id().unwrap();
        test_context.send_finalized_event((9, block_id_9), true);
        test_context.send_block_event(9, bank9.clone());

        // We should now have parent ready for slot 9
        test_context.check_parent_ready_slot((9, (5, block_id_5)));
    }

    #[test]
    fn test_received_standstill() {
        agave_logger::setup();
        let mut test_context = EventHandlerTestContext::setup();

        // Send notarize vote for slot 1 then skip rest of the window
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();
        test_context.send_parent_ready_event(1, (0, Hash::default()));
        test_context.check_for_vote(&Vote::new_notarization_vote(1, block_id_1));
        test_context.send_timeout_event(2);
        test_context.check_for_vote(&Vote::new_skip_vote(2));
        test_context.check_for_vote(&Vote::new_skip_vote(3));

        // Send a standstill event with highest parent ready at 0, we should refresh all the votes
        test_context.send_standstill_event(0);
        test_context.check_for_votes(&[
            Vote::new_notarization_vote(1, block_id_1),
            Vote::new_skip_vote(2),
            Vote::new_skip_vote(3),
        ]);

        // Send another standstill event with highest parent ready at 1, we should refresh votes for 2 and 3 only
        test_context.bls_ops.clear();
        test_context.send_standstill_event(1);
        test_context.check_for_votes(&[Vote::new_skip_vote(2), Vote::new_skip_vote(3)]);
    }

    #[test]
    fn test_received_set_identity() {
        agave_logger::setup();
        let mut test_context = EventHandlerTestContext::setup();
        let old_identity = test_context.cluster_info.keypair().insecure_clone();
        let new_identity = Keypair::new();
        let mut files_to_remove = vec![];

        // Before set identity we need to manually create the vote history storage file for new identity
        files_to_remove
            .push(test_context.create_vote_history_storage_and_switch_identity(&new_identity));

        // Should not send any votes because we set to a different identity
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let _ = test_context.create_block_and_send_block_event(1, root_bank.clone());
        test_context.send_parent_ready_event(1, (0, Hash::default()));
        // There should be no votes but we should see commitments for hot spares
        assert_eq!(
            test_context.bls_receiver.try_recv().err(),
            Some(crossbeam_channel::TryRecvError::Empty)
        );
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now set back to original identity
        files_to_remove
            .push(test_context.create_vote_history_storage_and_switch_identity(&old_identity));

        // We should now be able to vote again
        let slot = 4;
        let bank4 = test_context.create_block_and_send_block_event(slot, root_bank);
        let block_id_4 = bank4.block_id().unwrap();
        test_context.send_parent_ready_event(slot, (0, Hash::default()));
        test_context.check_for_vote(&Vote::new_notarization_vote(slot, block_id_4));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);

        for file in files_to_remove {
            let _ = remove_file(file);
        }
    }
}
