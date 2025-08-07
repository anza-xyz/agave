//! Handles incoming VotorEvents to take action or
//! notify block creation loop

use {
    crate::{
        commitment::{alpenglow_update_commitment_cache, AlpenglowCommitmentType},
        event::{CompletedBlock, VotorEvent, VotorEventReceiver},
        root_utils::{self, RootContext},
        timer_manager::TimerManager,
        vote_history::{VoteHistory, VoteHistoryError},
        voting_utils::{self, BLSOp, VoteError, VotingContext},
        votor::{SharedContext, Votor},
    },
    crossbeam_channel::{select, RecvError, SendError},
    parking_lot::RwLock,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::leader_schedule_utils::{
        first_of_consecutive_leader_slots, last_of_consecutive_leader_slots, leader_slot_index,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SetRootError},
    solana_signer::Signer,
    solana_votor_messages::{bls_message::Block, vote::Vote},
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
        let mut my_pubkey = vctx.identity_keypair.pubkey();
        let mut pending_blocks = PendingBlocks::default();
        let mut finalized_blocks = BTreeSet::default();
        let mut received_shred = BTreeSet::default();

        // Wait until migration has completed
        info!("{my_pubkey}: Event loop initialized");
        Votor::wait_for_migration_or_exit(&exit, &start);
        info!("{my_pubkey}: Event loop starting");

        if exit.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Check for set identity
        if let Err(e) = Self::handle_set_identity(&mut my_pubkey, &ctx, &mut vctx) {
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
            let event = select! {
                recv(event_receiver) -> msg => {
                    msg?
                },
                default(Duration::from_secs(1))  => continue
            };

            if event.should_ignore(vctx.root_bank_cache.root_bank().slot()) {
                continue;
            }

            let votes = Self::handle_event(
                &mut my_pubkey,
                event,
                &timer_manager,
                &ctx,
                &mut vctx,
                &rctx,
                &mut pending_blocks,
                &mut finalized_blocks,
                &mut received_shred,
            )?;

            // TODO: properly bubble up error handling here and in call graph
            for vote in votes {
                vctx.bls_sender.send(vote?).map_err(|_| SendError(()))?;
            }
        }

        Ok(())
    }

    fn handle_event(
        my_pubkey: &mut Pubkey,
        event: VotorEvent,
        timer_manager: &RwLock<TimerManager>,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        rctx: &RootContext,
        pending_blocks: &mut PendingBlocks,
        finalized_blocks: &mut BTreeSet<Block>,
        received_shred: &mut BTreeSet<Slot>,
    ) -> Result<Vec<Result<BLSOp, VoteError>>, EventLoopError> {
        let mut votes = vec![];
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
                ) {
                    Self::check_pending_blocks(my_pubkey, pending_blocks, vctx, &mut votes);
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
                )?;
            }

            // Block has received a notarization certificate
            VotorEvent::BlockNotarized(block) => {
                info!("{my_pubkey}: Block Notarized {block:?}");
                vctx.vote_history.add_block_notarized(block);
                Self::try_final(my_pubkey, block, vctx, &mut votes);
            }

            VotorEvent::FirstShred(slot) => {
                info!("{my_pubkey}: First shred {slot}");
                received_shred.insert(slot);
            }

            // Received a parent ready notification for `slot`
            VotorEvent::ParentReady { slot, parent_block } => {
                info!("{my_pubkey}: Parent ready {slot} {parent_block:?}");
                let should_set_timeouts = vctx.vote_history.add_parent_ready(slot, parent_block);
                Self::check_pending_blocks(my_pubkey, pending_blocks, vctx, &mut votes);
                if should_set_timeouts {
                    timer_manager.write().set_timeouts(slot);
                }
            }

            VotorEvent::TimeoutCrashedLeader(slot) => {
                info!("{my_pubkey}: TimeoutCrashedLeader {slot}");
                if vctx.vote_history.voted(slot) || received_shred.contains(&slot) {
                    return Ok(votes);
                }
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes);
            }

            // Skip timer for the slot has fired
            VotorEvent::Timeout(slot) => {
                info!("{my_pubkey}: Timeout {slot}");
                if vctx.vote_history.voted(slot) {
                    return Ok(votes);
                }
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes);
            }

            // We have observed the safe to notar condition, and can send a notar fallback vote
            // TODO: update cert pool to check parent block id for intra window slots
            VotorEvent::SafeToNotar(block @ (slot, block_id)) => {
                info!("{my_pubkey}: SafeToNotar {block:?}");
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes);
                if vctx.vote_history.its_over(slot)
                    || vctx.vote_history.voted_notar_fallback(slot, block_id)
                {
                    return Ok(votes);
                }
                info!("{my_pubkey}: Voting notarize-fallback for {slot} {block_id}");
                votes.push(voting_utils::insert_vote_and_create_bls_message(
                    my_pubkey,
                    Vote::new_notarization_fallback_vote(slot, block_id),
                    false,
                    vctx,
                ));
            }

            // We have observed the safe to skip condition, and can send a skip fallback vote
            VotorEvent::SafeToSkip(slot) => {
                info!("{my_pubkey}: SafeToSkip {slot}");
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes);
                if vctx.vote_history.its_over(slot) || vctx.vote_history.voted_skip_fallback(slot) {
                    return Ok(votes);
                }
                info!("{my_pubkey}: Voting skip-fallback for {slot}");
                votes.push(voting_utils::insert_vote_and_create_bls_message(
                    my_pubkey,
                    Vote::new_skip_fallback_vote(slot),
                    false,
                    vctx,
                ));
            }

            // It is time to produce our leader window
            VotorEvent::ProduceWindow(window_info) => {
                info!("{my_pubkey}: ProduceWindow {window_info:?}");
                let mut l_window_info = ctx.leader_window_notifier.window_info.lock().unwrap();
                if let Some(old_window_info) = l_window_info.as_ref() {
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
            VotorEvent::Finalized(block) => {
                info!("{my_pubkey}: Finalized {block:?}");
                finalized_blocks.insert(block);
                Self::check_rootable_blocks(
                    my_pubkey,
                    ctx,
                    vctx,
                    rctx,
                    pending_blocks,
                    finalized_blocks,
                    received_shred,
                )?;
            }

            // We have not observed a finalization certificate in a while, refresh our votes
            VotorEvent::Standstill(highest_finalized_slot) => {
                info!("{my_pubkey}: Standstill {highest_finalized_slot}");
                // certs refresh happens in CertificatePoolService
                Self::refresh_votes(my_pubkey, highest_finalized_slot, vctx, &mut votes);
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
    ///
    /// If successful returns true
    fn try_notar(
        my_pubkey: &Pubkey,
        (slot, block_id): Block,
        parent_block @ (parent_slot, parent_block_id): Block,
        pending_blocks: &mut PendingBlocks,
        voting_context: &mut VotingContext,
        votes: &mut Vec<Result<BLSOp, VoteError>>,
    ) -> bool {
        if voting_context.vote_history.voted(slot) {
            return false;
        }

        if leader_slot_index(slot) == 0 || slot == 1 {
            if !voting_context
                .vote_history
                .is_parent_ready(slot, &parent_block)
            {
                // Need to ingest more certificates first
                return false;
            }
        } else {
            if parent_slot.saturating_add(1) != slot {
                // Non consecutive
                return false;
            }
            if voting_context.vote_history.voted_notar(parent_slot) != Some(parent_block_id) {
                // Voted skip, or notarize on a different version of the parent
                return false;
            }
        }

        info!("{my_pubkey}: Voting notarize for {slot} {block_id}");
        votes.push(voting_utils::insert_vote_and_create_bls_message(
            my_pubkey,
            Vote::new_notarization_vote(slot, block_id),
            false,
            voting_context,
        ));
        let _ = alpenglow_update_commitment_cache(
            AlpenglowCommitmentType::Notarize,
            slot,
            &voting_context.commitment_sender,
        );
        pending_blocks.remove(&slot);

        true
    }

    /// Checks the pending blocks that have completed replay to see if they
    /// are eligble to be voted on now
    fn check_pending_blocks(
        my_pubkey: &Pubkey,
        pending_blocks: &mut PendingBlocks,
        voting_context: &mut VotingContext,
        votes: &mut Vec<Result<BLSOp, VoteError>>,
    ) {
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
            );
        }
    }

    /// Tries to send a finalize vote for the block if
    /// - the block has a notarization certificate
    /// - we have not already voted finalize
    /// - we voted notarize for the block
    /// - we have not voted skip, notarize fallback or skip fallback in the slot (bad window)
    ///
    /// If successful returns true
    fn try_final(
        my_pubkey: &Pubkey,
        block @ (slot, block_id): Block,
        voting_context: &mut VotingContext,
        votes: &mut Vec<Result<BLSOp, VoteError>>,
    ) -> bool {
        if !voting_context.vote_history.is_block_notarized(&block)
            || voting_context.vote_history.its_over(slot)
            || voting_context.vote_history.bad_window(slot)
        {
            return false;
        }

        if voting_context
            .vote_history
            .voted_notar(slot)
            .is_none_or(|bid| bid != block_id)
        {
            return false;
        }

        info!("{my_pubkey}: Voting finalize for {slot}");
        votes.push(voting_utils::insert_vote_and_create_bls_message(
            my_pubkey,
            Vote::new_finalization_vote(slot),
            false,
            voting_context,
        ));
        true
    }

    fn try_skip_window(
        my_pubkey: &Pubkey,
        slot: Slot,
        voting_context: &mut VotingContext,
        votes: &mut Vec<Result<BLSOp, VoteError>>,
    ) {
        // In case we set root in the middle of a leader window,
        // it's not necessary to vote skip prior to it and we won't
        // be able to check vote history if we've already voted on it
        let start = first_of_consecutive_leader_slots(slot)
            .max(voting_context.root_bank_cache.root_bank().slot());
        for s in start..=last_of_consecutive_leader_slots(slot) {
            if voting_context.vote_history.voted(s) {
                continue;
            }
            info!("{my_pubkey}: Voting skip for {s}");
            votes.push(voting_utils::insert_vote_and_create_bls_message(
                my_pubkey,
                Vote::new_skip_vote(s),
                false,
                voting_context,
            ));
        }
    }

    /// Refresh all votes cast for slots > highest_finalized_slot
    fn refresh_votes(
        my_pubkey: &Pubkey,
        highest_finalized_slot: Slot,
        voting_context: &mut VotingContext,
        votes: &mut Vec<Result<BLSOp, VoteError>>,
    ) {
        for vote in voting_context
            .vote_history
            .votes_cast_since(highest_finalized_slot)
        {
            info!("{my_pubkey}: Refreshing vote {vote:?}");
            votes.push(voting_utils::insert_vote_and_create_bls_message(
                my_pubkey,
                vote,
                true,
                voting_context,
            ));
        }
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
        )
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_event_handler.join()
    }
}
