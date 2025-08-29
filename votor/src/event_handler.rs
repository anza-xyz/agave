//! Handles incoming VotorEvents to take action or
//! notify block creation loop

use {
    crate::{
        commitment::{alpenglow_update_commitment_cache, AlpenglowCommitmentType},
        event::{CompletedBlock, VotorEvent, VotorEventReceiver},
        event_handler::stats::EventHandlerStats,
        root_utils::{self, RootContext},
        timer_manager::TimerManager,
        vote_history::{VoteHistory, VoteHistoryError},
        voting_utils::{generate_vote_message, BLSOp, VoteError, VotingContext},
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
                "Unable to load new vote history when attempting to change identity from {} \
                 to {} on voting loop startup, Exiting: {}",
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

            let root_bank = vctx.root_bank.load();
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

            let mut send_vote_time = Measure::start("send_vote");
            for vote in votes {
                local_context.stats.incr_vote(&vote);
                vctx.bls_sender.send(vote).map_err(|_| SendError(()))?;
            }
            send_vote_time.stop();
            local_context.stats.send_vote_time_us = local_context
                .stats
                .send_vote_time_us
                .saturating_add(send_vote_time.as_us() as u32);
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
                        "{my_pubkey}: Attempting to start leader window for {}-{}, \
                        however there is already a pending window to produce {}-{}. \
                        Our production is lagging, discarding in favor of the newer window",
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
                            "Unable to load new vote history when attempting to change identity from {} \
                             to {} in voting loop, Exiting: {}",
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
            "{}: Triggering parent ready for slot {slot} with parent {parent_slot} {parent_block_id}",
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
        alpenglow_update_commitment_cache(
            AlpenglowCommitmentType::Notarize,
            slot,
            &voting_context.commitment_sender,
        )?;
        pending_blocks.remove(&slot);

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
        let root_bank = voting_context.root_bank.load();
        let start = first_of_consecutive_leader_slots(slot).max(root_bank.slot());
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
