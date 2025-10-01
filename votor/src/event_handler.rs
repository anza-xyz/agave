//! Handles incoming VotorEvents to take action or
//! notify block creation loop

use {
    crate::{
        commitment::{update_commitment_cache, CommitmentType},
        event::{CompletedBlock, VotorEvent, VotorEventReceiver},
        event_handler::stats::EventHandlerStats,
        root_utils::{self, RootContext},
        timer_manager::TimerManager,
        vote_history::{VoteHistory, VoteHistoryError},
        voting_service::BLSOp,
        voting_utils::{generate_vote_message, VoteError, VotingContext},
        votor::{SharedContext, Votor},
    },
    crossbeam_channel::{select, RecvError, SendError},
    parking_lot::RwLock,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::leader_schedule_utils::{
        first_of_consecutive_leader_slots, last_of_consecutive_leader_slots, leader_slot_index,
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SetRootError},
    solana_signer::Signer,
    solana_votor_messages::{consensus_message::Block, vote::Vote},
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
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
    ReceiverDisconnected(#[from] RecvError),

    #[error("Sender is disconnected")]
    SenderDisconnected(#[from] SendError<()>),

    #[error("Error generating and inserting vote")]
    VotingError(#[from] VoteError),

    #[error("Unable to set root")]
    SetRootError(#[from] SetRootError),

    #[error("Set identity error")]
    SetIdentityError(#[from] VoteHistoryError),
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
            .name("solVotorEventLoop".to_string())
            .spawn(move || {
                if let Err(e) = Self::event_loop(ctx) {
                    info!("Event loop exited: {e:?}. Shutting down");
                    exit.store(true, Ordering::Relaxed);
                }
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

        if exit.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Check for set identity
        if let Err(e) = Self::handle_set_identity(&mut local_context.my_pubkey, &ctx, &mut vctx) {
            error!(
                "Unable to load new vote history when attempting to change identity from {} to {} \
                 on voting loop startup, Exiting: {}",
                vctx.vote_history.node_pubkey,
                ctx.cluster_info.id(),
                e
            );
            return Err(EventLoopError::SetIdentityError(e));
        }

        while !exit.load(Ordering::Relaxed) {
            let mut receive_event_time = Measure::start("receive_event");
            let event = select! {
                recv(event_receiver) -> msg => {
                    msg?
                },
                default(Duration::from_secs(1))  => continue
            };
            receive_event_time.stop();
            local_context.stats.receive_event_time_us = local_context
                .stats
                .receive_event_time_us
                .saturating_add(receive_event_time.as_us() as u32);

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
                vctx.bls_sender.send(vote).map_err(|_| SendError(()))?;
            }
            send_votes_batch_time.stop();
            local_context.stats.send_votes_batch_time_us = local_context
                .stats
                .send_votes_batch_time_us
                .saturating_add(send_votes_batch_time.as_us() as u32);
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
            ref mut my_pubkey,
            ref mut pending_blocks,
            ref mut finalized_blocks,
            ref mut received_shred,
            ref mut stats,
        } = local_context;
        match event {
            // Block has completed replay
            VotorEvent::Block(CompletedBlock { slot, bank }) => {
                debug_assert!(bank.is_frozen());
                vctx.consensus_metrics.record_start_of_slot(slot);
                match vctx
                    .consensus_metrics
                    .record_block_hash_seen(*bank.collector_id(), slot)
                {
                    Ok(()) => (),
                    Err(err) => {
                        error!("{my_pubkey}: recording block on slot {slot} failed with {err:?}");
                    }
                }
                vctx.consensus_metrics.maybe_new_epoch(bank.epoch());
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
                if let Some(parent_block) =
                    Self::add_missing_parent_ready(block, ctx, vctx, local_context)
                {
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
                vctx.consensus_metrics.record_start_of_slot(slot);
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
                    vctx.consensus_metrics
                        .record_start_of_slot(slot.saturating_add(1));
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
                let (slot, _) = block;
                if slot != last_of_consecutive_leader_slots(slot) {
                    vctx.consensus_metrics
                        .record_start_of_slot(slot.saturating_add(1));
                }
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
                if let Some(parent_block) =
                    Self::add_missing_parent_ready(block, ctx, vctx, local_context)
                {
                    Self::handle_parent_ready_event(
                        block.0,
                        parent_block,
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
    ///
    /// Returns [`Some(Block)`] of the parent if the parent ready for the `finalized_block` should be added.
    fn add_missing_parent_ready(
        finalized_block: Block,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        local_context: &mut LocalContext,
    ) -> Option<Block> {
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
        Some((parent_slot, parent_block_id))
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
    /// are eligble to be voted on now
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

    /// Checks if we can set root on a new block
    /// The block must be:
    /// - Present in bank forks
    /// - Newer than the current root
    /// - We must have already voted on bank.slot()
    /// - Bank is frozen and finished shredding
    /// - Block has a finalization certificate
    ///
    /// If so set root on the highest block that fits these conditions
    fn check_rootable_blocks(
        my_pubkey: &Pubkey,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        rctx: &RootContext,
        pending_blocks: &mut PendingBlocks,
        finalized_blocks: &mut BTreeSet<Block>,
        received_shred: &mut BTreeSet<Slot>,
        stats: &mut EventHandlerStats,
    ) -> Result<(), SetRootError> {
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
        root_utils::set_root(
            my_pubkey,
            new_root,
            ctx,
            vctx,
            rctx,
            pending_blocks,
            finalized_blocks,
            received_shred,
        )?;
        stats.set_root(new_root);
        Ok(())
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
            consensus_metrics::ConsensusMetrics,
            event::{LeaderWindowInfo, VotorEventSender},
            vote_history_storage::{
                FileVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
                VoteHistoryStorage,
            },
            voting_service::BLSOp,
            votor::LeaderWindowNotifier,
        },
        crossbeam_channel::{unbounded, Receiver, RecvTimeoutError},
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
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
            installed_scheduler_pool::BankWithScheduler,
        },
        solana_streamer::socket::SocketAddrSpace,
        solana_votor_messages::{
            consensus_message::{ConsensusMessage, VoteMessage, BLS_KEYPAIR_DERIVE_SEED},
            vote::Vote,
        },
        std::{
            collections::HashMap,
            fs::remove_file,
            path::PathBuf,
            sync::{Arc, RwLock},
            thread::sleep,
            time::Instant,
        },
        test_case::test_case,
    };

    struct EventHandlerTestContext {
        exit: Arc<AtomicBool>,
        event_sender: VotorEventSender,
        event_handler: EventHandler,
        bls_receiver: Receiver<BLSOp>,
        commitment_receiver: Receiver<CommitmentAggregationData>,
        own_vote_receiver: Receiver<ConsensusMessage>,
        bank_forks: Arc<RwLock<BankForks>>,
        my_bls_keypair: BLSKeypair,
        timer_manager: Arc<PlRwLock<TimerManager>>,
        leader_window_notifier: Arc<LeaderWindowNotifier>,
        drop_bank_receiver: Receiver<Vec<BankWithScheduler>>,
        cluster_info: Arc<ClusterInfo>,
    }

    fn setup() -> EventHandlerTestContext {
        let (bls_sender, bls_receiver) = unbounded();
        let (commitment_sender, commitment_receiver) = unbounded();
        let (own_vote_sender, own_vote_receiver) = unbounded();
        let (drop_bank_sender, drop_bank_receiver) = unbounded();
        let exit = Arc::new(AtomicBool::new(false));
        let start = Arc::new((Mutex::new(true), Condvar::new()));
        let (event_sender, event_receiver) = unbounded();
        let consensus_metrics = ConsensusMetrics::new(0);
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
            identity_keypair: Arc::new(my_node_keypair),
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
            consensus_metrics,
        };

        let root_context = RootContext {
            leader_schedule_cache: Arc::new(LeaderScheduleCache::new_from_bank(
                &bank_forks.read().unwrap().root_bank(),
            )),
            snapshot_controller: None,
            bank_notification_sender: None,
            drop_bank_sender,
        };

        let event_handler_context = EventHandlerContext {
            exit: exit.clone(),
            start,
            event_receiver,
            timer_manager: timer_manager.clone(),
            shared_context,
            voting_context,
            root_context,
        };
        let event_handler = EventHandler::new(event_handler_context);

        EventHandlerTestContext {
            event_sender,
            exit,
            event_handler,
            bls_receiver,
            commitment_receiver,
            own_vote_receiver,
            bank_forks,
            my_bls_keypair,
            timer_manager,
            leader_window_notifier,
            drop_bank_receiver,
            cluster_info,
        }
    }

    const TEST_SHORT_TIMEOUT: Duration = Duration::from_millis(100);

    fn send_parent_ready_event(
        test_context: &EventHandlerTestContext,
        slot: Slot,
        parent_block: Block,
    ) {
        test_context
            .event_sender
            .send(VotorEvent::ParentReady { slot, parent_block })
            .unwrap();
    }

    fn send_timeout_event(test_context: &EventHandlerTestContext, slot: Slot) {
        test_context
            .event_sender
            .send(VotorEvent::Timeout(slot))
            .unwrap();
    }

    fn send_block_event(test_context: &EventHandlerTestContext, slot: Slot, bank: Arc<Bank>) {
        test_context
            .event_sender
            .send(VotorEvent::Block(CompletedBlock { slot, bank }))
            .unwrap();
    }

    fn send_block_notarized_event(test_context: &EventHandlerTestContext, block: Block) {
        test_context
            .event_sender
            .send(VotorEvent::BlockNotarized(block))
            .unwrap();
    }

    fn send_timeout_crashed_leader_event(test_context: &EventHandlerTestContext, slot: Slot) {
        test_context
            .event_sender
            .send(VotorEvent::TimeoutCrashedLeader(slot))
            .unwrap();
    }

    fn send_first_shred_event(test_context: &EventHandlerTestContext, slot: Slot) {
        test_context
            .event_sender
            .send(VotorEvent::FirstShred(slot))
            .unwrap();
    }

    fn send_safe_to_notar_event(test_context: &EventHandlerTestContext, block: Block) {
        test_context
            .event_sender
            .send(VotorEvent::SafeToNotar(block))
            .unwrap();
    }

    fn send_safe_to_skip_event(test_context: &EventHandlerTestContext, slot: Slot) {
        test_context
            .event_sender
            .send(VotorEvent::SafeToSkip(slot))
            .unwrap();
    }

    fn send_produce_window_event(
        test_context: &EventHandlerTestContext,
        start_slot: Slot,
        end_slot: Slot,
        parent_block: Block,
    ) {
        test_context
            .event_sender
            .send(VotorEvent::ProduceWindow(LeaderWindowInfo {
                start_slot,
                end_slot,
                parent_block,
                skip_timer: Instant::now(),
            }))
            .unwrap();
    }

    fn send_finalized_event(
        test_context: &EventHandlerTestContext,
        block: Block,
        is_fast_finalization: bool,
    ) {
        test_context
            .event_sender
            .send(VotorEvent::Finalized(block, is_fast_finalization))
            .unwrap();
    }

    fn create_block_only(
        test_context: &EventHandlerTestContext,
        slot: Slot,
        parent_bank: Arc<Bank>,
    ) -> Arc<Bank> {
        let bank = Bank::new_from_parent(parent_bank, &Pubkey::new_unique(), slot);
        bank.set_block_id(Some(Hash::new_unique()));
        bank.freeze();
        let mut bank_forks_w = test_context.bank_forks.write().unwrap();
        bank_forks_w.insert(bank);
        bank_forks_w.get(slot).unwrap()
    }

    fn create_block_and_send_block_event(
        test_context: &EventHandlerTestContext,
        slot: Slot,
        parent_bank: Arc<Bank>,
    ) -> Arc<Bank> {
        let bank = create_block_only(test_context, slot, parent_bank);
        send_block_event(test_context, slot, bank.clone());
        bank
    }

    fn check_for_votes(test_context: &EventHandlerTestContext, expected_votes: &[Vote]) {
        let mut expected_messages = expected_votes
            .iter()
            .map(|v| {
                let expected_vote_serialized = bincode::serialize(v).unwrap();
                let signature: BLSSignature = test_context
                    .my_bls_keypair
                    .sign(&expected_vote_serialized)
                    .into();
                ConsensusMessage::Vote(VoteMessage {
                    vote: *v,
                    rank: 0,
                    signature,
                })
            })
            .collect::<Vec<_>>();
        while !expected_messages.is_empty() {
            let bls_op = test_context
                .bls_receiver
                .recv_timeout(TEST_SHORT_TIMEOUT)
                .unwrap();
            // Remove the matched expected message
            expected_messages.retain(|expected_message| {
                !matches!(
                    &bls_op,
                    BLSOp::PushVote { message, .. } if **message == *expected_message
                )
            });
        }
    }

    fn check_for_vote(test_context: &EventHandlerTestContext, expected_vote: &Vote) {
        let bls_op = test_context
            .bls_receiver
            .recv_timeout(TEST_SHORT_TIMEOUT)
            .unwrap();
        let expected_vote_serialized = bincode::serialize(expected_vote).unwrap();
        let signature: BLSSignature = test_context
            .my_bls_keypair
            .sign(&expected_vote_serialized)
            .into();
        let expected_message = ConsensusMessage::Vote(VoteMessage {
            vote: *expected_vote,
            rank: 0,
            signature,
        });
        assert!(matches!(
            bls_op,
            BLSOp::PushVote { message, .. } if *message == expected_message
        ));
        // Also check own_vote_receiver
        let own_vote = test_context
            .own_vote_receiver
            .recv_timeout(TEST_SHORT_TIMEOUT)
            .unwrap();
        assert_eq!(own_vote, expected_message);
    }

    fn check_for_commitment(
        test_context: &EventHandlerTestContext,
        expected_type: CommitmentType,
        expected_slot: Slot,
    ) {
        let commitment = test_context
            .commitment_receiver
            .recv_timeout(TEST_SHORT_TIMEOUT)
            .unwrap();
        assert_eq!(commitment.commitment_type, expected_type);
        assert_eq!(commitment.slot, expected_slot);
    }

    fn check_no_vote_or_commitment(test_context: &EventHandlerTestContext) {
        assert_eq!(
            test_context
                .bls_receiver
                .recv_timeout(TEST_SHORT_TIMEOUT)
                .err(),
            Some(RecvTimeoutError::Timeout)
        );
        assert_eq!(
            test_context
                .commitment_receiver
                .recv_timeout(TEST_SHORT_TIMEOUT)
                .err(),
            Some(RecvTimeoutError::Timeout)
        );
    }

    fn check_parent_ready_slot(test_context: &EventHandlerTestContext, expected: (Slot, Block)) {
        let started = Instant::now();
        while started.elapsed() < TEST_SHORT_TIMEOUT {
            if *test_context
                .leader_window_notifier
                .highest_parent_ready
                .read()
                .unwrap()
                == expected
            {
                let slot = expected.0;
                check_timeout_set(test_context, slot);
                return;
            }
            sleep(Duration::from_millis(10));
        }
        panic!("Parent ready slot not set to expected {expected:?}");
    }

    fn check_timeout_set(test_context: &EventHandlerTestContext, expected_slot: Slot) {
        assert!(test_context
            .timer_manager
            .read()
            .is_timeout_set(expected_slot));
    }

    fn create_vote_history_storage_and_switch_identity(
        test_context: &EventHandlerTestContext,
        new_identity: &Keypair,
    ) -> PathBuf {
        let file_vote_history_storage = FileVoteHistoryStorage::default();
        let saved_vote_history =
            SavedVoteHistory::new(&VoteHistory::new(new_identity.pubkey(), 0), &new_identity)
                .unwrap();
        assert!(file_vote_history_storage
            .store(&SavedVoteHistoryVersions::from(saved_vote_history),)
            .is_ok());
        test_context
            .cluster_info
            .set_keypair(Arc::new(new_identity.insecure_clone()));
        test_context
            .event_sender
            .send(VotorEvent::SetIdentity)
            .unwrap();
        // Sleep to make sure SetIdentity event is processed
        sleep(TEST_SHORT_TIMEOUT);
        file_vote_history_storage.filename(&new_identity.pubkey())
    }

    #[test]
    fn test_received_block_event_and_parent_ready_event() {
        // Test different orders of received block event and parent ready event
        // some will send Notarize immediately, some will wait for parent ready
        let test_context = setup();
        // Received block event which says block has completed replay

        // If there is a parent ready for block 1 Notarization is sent out.
        let slot = 1;
        let parent_slot = 0;
        send_parent_ready_event(&test_context, slot, (parent_slot, Hash::default()));
        check_parent_ready_slot(&test_context, (slot, (parent_slot, Hash::default())));
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = create_block_and_send_block_event(&test_context, slot, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        // We should receive Notarize Vote for block 1
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(slot, block_id_1),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, slot);

        // Add block event for 1 again will not trigger another Notarize or commitment
        send_block_event(&test_context, 1, bank1.clone());
        check_no_vote_or_commitment(&test_context);

        let slot = 2;
        let bank2 = create_block_and_send_block_event(&test_context, slot, bank1.clone());
        let block_id_2 = bank2.block_id().unwrap();

        // Because 2 is middle of window, we should see Notarize vote for block 2 even without parentready
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(slot, block_id_2),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, slot);

        // Slot 3 somehow links to block 1, should not trigger Notarize vote because it has a wrong parent (not 2)
        let _ = create_block_and_send_block_event(&test_context, 3, bank1.clone());
        check_no_vote_or_commitment(&test_context);

        // Slot 4 completed replay without parent ready or parent notarized should not trigger Notarize vote
        let slot = 4;
        let bank4 = create_block_and_send_block_event(&test_context, slot, bank2.clone());
        let block_id_4 = bank4.block_id().unwrap();
        check_no_vote_or_commitment(&test_context);

        // Send parent ready for slot 4 should trigger Notarize vote for slot 4
        send_parent_ready_event(&test_context, slot, (2, block_id_2));
        check_parent_ready_slot(&test_context, (slot, (2, block_id_2)));
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(slot, block_id_4),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, slot);

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_received_block_notarized_and_timeout() {
        // Test block notarized event will trigger Finalize vote when all conditions are met
        // But it will not trigger Finalize if any of the conditions are not met
        let test_context = setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = create_block_and_send_block_event(&test_context, 1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        // Add parent ready for 0 to trigger notar vote for 1
        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        check_parent_ready_slot(&test_context, (1, (0, Hash::default())));
        check_for_vote(&test_context, &Vote::new_notarization_vote(1, block_id_1));
        check_for_commitment(&test_context, CommitmentType::Notarize, 1);

        // Send block notarized event should trigger Finalize vote
        send_block_notarized_event(&test_context, (1, block_id_1));
        check_for_vote(&test_context, &Vote::new_finalization_vote(1));

        let bank2 = create_block_and_send_block_event(&test_context, 2, bank1.clone());
        let block_id_2 = bank2.block_id().unwrap();
        // Both Notarize and Finalize votes should trigger for 2
        check_for_vote(&test_context, &Vote::new_notarization_vote(2, block_id_2));
        check_for_commitment(&test_context, CommitmentType::Notarize, 2);
        send_block_notarized_event(&test_context, (2, block_id_2));
        check_for_vote(&test_context, &Vote::new_finalization_vote(2));

        // Create bank3 but do not Notarize, so Finalize vote should not trigger
        let slot = 3;
        let bank3 = create_block_only(&test_context, slot, bank2.clone());
        let block_id_3 = bank3.block_id().unwrap();
        // Check no notarization vote for 3
        check_no_vote_or_commitment(&test_context);

        send_block_notarized_event(&test_context, (slot, block_id_3));
        // Check no Finalize vote for 3
        check_no_vote_or_commitment(&test_context);

        // Now send Block event simulating replay completed for 3
        send_block_event(&test_context, slot, bank3.clone());
        // There should be a notarization vote for 3
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(slot, block_id_3),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, slot);
        // Check there is a Finalize vote for 3
        check_for_vote(&test_context, &Vote::new_finalization_vote(slot));

        // After casting finalization vote for 3, we will not send skip fallback
        send_safe_to_skip_event(&test_context, slot);
        check_no_vote_or_commitment(&test_context);

        // Simulate that block 4 never arrives, we create block 4 but send timeout event
        let slot = 4;
        let bank4 = create_block_only(&test_context, slot, bank3.clone());
        send_timeout_event(&test_context, slot);
        // We did eventually complete replay for 4
        send_block_event(&test_context, slot, bank4.clone());
        // There should be a skip vote for 4 to 7 each
        check_for_vote(&test_context, &Vote::new_skip_vote(slot));
        check_for_vote(&test_context, &Vote::new_skip_vote(slot + 1));
        check_for_vote(&test_context, &Vote::new_skip_vote(slot + 2));
        check_for_vote(&test_context, &Vote::new_skip_vote(slot + 3));

        // Now we get block 5, it's replayed and we get block_notarized, but since 4~7 is a bad
        // window already, we shouldn't have notarize or finalize vote for 5
        let slot = 5;
        let bank5 = create_block_only(&test_context, slot, bank4.clone());
        send_block_event(&test_context, slot, bank5.clone());
        check_no_vote_or_commitment(&test_context);

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_received_timeout_crashed_leader_and_first_shred() {
        let test_context = setup();

        // Simulate a crashed leader for slot 4
        send_timeout_crashed_leader_event(&test_context, 4);

        // Since we don't have any shred for block 4, we should vote skip for 4-7
        check_for_vote(&test_context, &Vote::new_skip_vote(4));
        check_for_vote(&test_context, &Vote::new_skip_vote(5));
        check_for_vote(&test_context, &Vote::new_skip_vote(6));
        check_for_vote(&test_context, &Vote::new_skip_vote(7));

        // Now if we received one shred for slot 8, then we should not do anything when
        // receiving timeout_crashed_leader_event for slot 8
        send_first_shred_event(&test_context, 8);
        send_timeout_crashed_leader_event(&test_context, 8);
        check_no_vote_or_commitment(&test_context);
    }

    #[test]
    fn test_received_safe_to_notar() {
        let test_context = setup();

        // We can theoretically not vote skip here and test will pass, but in real world
        // safe_to_notar event only fires after we voted skip for the whole window
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank_1 = create_block_and_send_block_event(&test_context, 1, root_bank);
        let block_id_1_old = bank_1.block_id().unwrap();
        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        check_parent_ready_slot(&test_context, (1, (0, Hash::default())));
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(1, block_id_1_old),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, 1);

        // Now we got safe_to_notar event for slot 1 and a different block id
        let block_id_1_1 = Hash::new_unique();
        send_safe_to_notar_event(&test_context, (1, block_id_1_1));
        // We should see rest of the window skipped
        check_for_vote(&test_context, &Vote::new_skip_vote(2));
        check_for_vote(&test_context, &Vote::new_skip_vote(3));
        // We should also see notarize fallback for the new block id
        check_for_vote(
            &test_context,
            &Vote::new_notarization_fallback_vote(1, block_id_1_1),
        );

        // We can trigger safe_to_notar event again for a different block id
        // In this test you can trigger this any number of times, but the white paper
        // proved we can only get up to 3 different block ids on a slot, and our
        // certificate pool implementation checks that.
        let block_id_1_2 = Hash::new_unique();
        send_safe_to_notar_event(&test_context, (1, block_id_1_2));
        // No skips this time because we already skipped the rest of the window
        // We should also see notarize fallback for the new block id
        check_for_vote(
            &test_context,
            &Vote::new_notarization_fallback_vote(1, block_id_1_2),
        );

        // But getting safe_to_notar for a block id we voted before should be no-op
        send_safe_to_notar_event(&test_context, (1, block_id_1_1));
        check_no_vote_or_commitment(&test_context);
    }

    #[test]
    fn test_received_safe_to_skip() {
        let test_context = setup();

        // The safe_to_skip event only fires after we voted notarize for the slot
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank_1 = create_block_and_send_block_event(&test_context, 1, root_bank);
        let block_id_1 = bank_1.block_id().unwrap();
        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        check_parent_ready_slot(&test_context, (1, (0, Hash::default())));
        check_for_vote(&test_context, &Vote::new_notarization_vote(1, block_id_1));
        check_for_commitment(&test_context, CommitmentType::Notarize, 1);

        // Now we got safe_to_skip event for slot 1
        send_safe_to_skip_event(&test_context, 1);
        // We should see rest of the window skipped
        check_for_vote(&test_context, &Vote::new_skip_vote(2));
        check_for_vote(&test_context, &Vote::new_skip_vote(3));
        // We should see skip fallback for slot 1
        check_for_vote(&test_context, &Vote::new_skip_fallback_vote(1));

        // We can trigger safe_to_skip event again, this should be a no-op
        send_safe_to_skip_event(&test_context, 1);
        check_no_vote_or_commitment(&test_context);
    }

    #[test]
    fn test_received_produce_window() {
        let test_context = setup();

        // Produce a full window of blocks
        // Assume the leader for 1-3 is us, send produce window event
        send_produce_window_event(&test_context, 1, 3, (0, Hash::default()));

        // Check that leader_window_notifier is updated
        let window_info = test_context
            .leader_window_notifier
            .window_info
            .lock()
            .unwrap();
        let (mut guard, timeout_res) = test_context
            .leader_window_notifier
            .window_notification
            .wait_timeout_while(window_info, TEST_SHORT_TIMEOUT, |wi| wi.is_none())
            .unwrap();
        assert!(!timeout_res.timed_out());
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
        send_produce_window_event(&test_context, 2, 3, (1, block_id_1));
        let window_info = test_context
            .leader_window_notifier
            .window_info
            .lock()
            .unwrap();
        let (mut guard, timeout_res) = test_context
            .leader_window_notifier
            .window_notification
            .wait_timeout_while(window_info, TEST_SHORT_TIMEOUT, |wi| wi.is_none())
            .unwrap();
        assert!(!timeout_res.timed_out());
        let received_leader_window_info = guard.take().unwrap();
        assert_eq!(received_leader_window_info.start_slot, 2);
        assert_eq!(received_leader_window_info.end_slot, 3);
        assert_eq!(received_leader_window_info.parent_block, (1, block_id_1));
        drop(guard);

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_received_finalized() {
        solana_logger::setup();
        let test_context = setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = create_block_and_send_block_event(&test_context, 1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        check_parent_ready_slot(&test_context, (1, (0, Hash::default())));
        check_for_vote(&test_context, &Vote::new_notarization_vote(1, block_id_1));
        check_for_commitment(&test_context, CommitmentType::Notarize, 1);

        // Now we got finalized event for slot 1
        send_finalized_event(&test_context, (1, block_id_1), true);
        // Listen on drop bank receiver, it should get bank 0
        let dropped_banks = test_context
            .drop_bank_receiver
            .recv_timeout(TEST_SHORT_TIMEOUT)
            .unwrap();
        assert_eq!(dropped_banks.len(), 1);
        assert_eq!(dropped_banks[0].slot(), 0);
        // The bank forks root should be updated to 1
        assert_eq!(test_context.bank_forks.read().unwrap().root(), 1);

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_parent_ready_in_middle_of_window() {
        solana_logger::setup();
        let test_context = setup();

        // We just woke up and received finalize for slot 5
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank4 = create_block_and_send_block_event(&test_context, 4, root_bank);
        let block_id_4 = bank4.block_id().unwrap();

        let bank5 = create_block_and_send_block_event(&test_context, 5, bank4.clone());
        let block_id_5 = bank5.block_id().unwrap();

        send_finalized_event(&test_context, (5, block_id_5), true);
        // We should now have parent ready for slot 5
        check_parent_ready_slot(&test_context, (5, (4, block_id_4)));

        // We are partitioned off from rest of the network, and suddenly received finalize for
        // slot 9 a little before we finished replay slot 9
        let bank9 = create_block_only(&test_context, 9, bank5.clone());
        let block_id_9 = bank9.block_id().unwrap();
        send_finalized_event(&test_context, (9, block_id_9), true);
        send_block_event(&test_context, 9, bank9.clone());

        // We should now have parent ready for slot 9
        check_parent_ready_slot(&test_context, (9, (5, block_id_5)));

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_received_standstill() {
        solana_logger::setup();
        let test_context = setup();

        // Send notarize vote for slot 1 then skip rest of the window
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = create_block_and_send_block_event(&test_context, 1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();
        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        check_for_vote(&test_context, &Vote::new_notarization_vote(1, block_id_1));
        send_timeout_event(&test_context, 2);
        check_for_vote(&test_context, &Vote::new_skip_vote(2));
        check_for_vote(&test_context, &Vote::new_skip_vote(3));

        // Send a standstill event with highest parent ready at 0, we should refresh all the votes
        test_context
            .event_sender
            .send(VotorEvent::Standstill(0))
            .unwrap();
        check_for_votes(
            &test_context,
            &[
                Vote::new_notarization_vote(1, block_id_1),
                Vote::new_skip_vote(2),
                Vote::new_skip_vote(3),
            ],
        );

        // Send another standstill event with highest parent ready at 1, we should refresh votes for 2 and 3 only
        test_context
            .event_sender
            .send(VotorEvent::Standstill(1))
            .unwrap();
        check_for_votes(
            &test_context,
            &[Vote::new_skip_vote(2), Vote::new_skip_vote(3)],
        );

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();
    }

    #[test]
    fn test_received_set_identity() {
        solana_logger::setup();
        let test_context = setup();
        let old_identity = test_context.cluster_info.keypair().insecure_clone();
        let new_identity = Keypair::new();
        let mut files_to_remove = vec![];

        // Before set identity we need to manually create the vote history storage file for new identity
        files_to_remove.push(create_vote_history_storage_and_switch_identity(
            &test_context,
            &new_identity,
        ));

        // Should not send any votes because we set to a different identity
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let _ = create_block_and_send_block_event(&test_context, 1, root_bank.clone());
        send_parent_ready_event(&test_context, 1, (0, Hash::default()));
        // There should be no votes but we should see commitments for hot spares
        assert_eq!(
            test_context
                .bls_receiver
                .recv_timeout(TEST_SHORT_TIMEOUT)
                .err(),
            Some(RecvTimeoutError::Timeout)
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, 1);

        // Now set back to original identity
        files_to_remove.push(create_vote_history_storage_and_switch_identity(
            &test_context,
            &old_identity,
        ));

        // We should now be able to vote again
        let slot = 4;
        let bank4 = create_block_and_send_block_event(&test_context, slot, root_bank);
        let block_id_4 = bank4.block_id().unwrap();
        send_parent_ready_event(&test_context, slot, (0, Hash::default()));
        check_for_vote(
            &test_context,
            &Vote::new_notarization_vote(slot, block_id_4),
        );
        check_for_commitment(&test_context, CommitmentType::Notarize, slot);

        test_context.exit.store(true, Ordering::Relaxed);
        test_context.event_handler.join().unwrap();

        for file in files_to_remove {
            let _ = remove_file(file);
        }
    }

    #[test_case("bls_receiver")]
    #[test_case("commitment_receiver")]
    #[test_case("own_vote_receiver")]
    fn test_channel_disconnection(channel_name: &str) {
        solana_logger::setup();
        let mut setup_result = setup();
        match channel_name {
            "bls_receiver" => {
                let bls_receiver = setup_result.bls_receiver.clone();
                setup_result.bls_receiver = unbounded().1;
                drop(bls_receiver);
            }
            "commitment_receiver" => {
                let commitment_receiver = setup_result.commitment_receiver.clone();
                setup_result.commitment_receiver = unbounded().1;
                drop(commitment_receiver);
            }
            "own_vote_receiver" => {
                let own_vote_receiver = setup_result.own_vote_receiver.clone();
                setup_result.own_vote_receiver = unbounded().1;
                drop(own_vote_receiver);
            }
            _ => panic!("Unknown channel name"),
        }
        // We normally need some event hitting all the senders to trigger exit
        let root_bank = setup_result.bank_forks.read().unwrap().root_bank();
        let _ = create_block_and_send_block_event(&setup_result, 1, root_bank);
        send_parent_ready_event(&setup_result, 1, (0, Hash::default()));
        // Verify that the event_handler exits within 5 seconds
        let start = Instant::now();
        while !setup_result.exit.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(5)
        {
            sleep(TEST_SHORT_TIMEOUT);
        }
        assert!(setup_result.exit.load(Ordering::Relaxed));
        setup_result.event_handler.join().unwrap();
    }
}
