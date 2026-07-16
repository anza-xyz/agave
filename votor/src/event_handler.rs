//! Handles incoming VotorEvents to take action or
//! notify block creation loop

use {
    crate::{
        commitment::{CommitmentType, update_commitment_cache},
        event::{
            CompletedBlock, LatestSwitchRequest, RepairEvent, RepairEventSender, SwitchBankEvent,
            VotorEvent, VotorEventReceiver,
        },
        event_handler::stats::EventHandlerStats,
        root_utils::{self, RootContext},
        timer_manager::TimerManager,
        vote_history::{VoteHistory, VoteHistoryError},
        voting_service::BLSOp,
        voting_utils::{
            VoteError, VotingContext, create_and_send_own_vote_message,
            generate_refresh_vote_message, insert_vote_and_create_bls_message,
        },
        votor::SharedContext,
    },
    agave_votor_messages::{
        consensus_message::Block, metric_types::ConsensusMetricsEvent, migration::MigrationStatus,
        vote::Vote,
    },
    crossbeam_channel::{RecvError, SendError, TrySendError, select},
    parking_lot::RwLock,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        leader_schedule_utils::{
            first_of_consecutive_leader_slots, last_of_consecutive_leader_slots, leader_slot_index,
        },
    },
    solana_signer::Signer,
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
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
    pub(crate) migration_status: Arc<MigrationStatus>,

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
    /// When in standstill, tracks the highest finalized slot at the time standstill was detected.
    /// Used to calculate dynamic timeout extensions (5% per leader window since standstill).
    /// Reset to `None` when a new finalization event is received.
    pub(crate) standstill_slot: Option<Slot>,
}

impl EventHandler {
    pub(crate) fn new(ctx: EventHandlerContext) -> Self {
        let exit = ctx.exit.clone();
        let t_event_handler = Builder::new()
            .name("solVotorEvLoop".to_string())
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
            migration_status,
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
            standstill_slot: None,
        };

        // Wait until migration has completed
        info!("{}: Event loop initialized", local_context.my_pubkey);
        let Some(genesis_block) = migration_status.wait_for_migration_or_exit(&exit) else {
            // Exited during migration
            return Ok(());
        };
        let root_slot = vctx.sharable_banks.root().slot();
        info!(
            "{}: Event loop starting genesis {genesis_block:?} root {root_slot}",
            local_context.my_pubkey
        );

        // Check for set identity
        if let Err(e) = Self::handle_set_identity(&mut local_context.my_pubkey, &ctx, &mut vctx) {
            error!(
                "Unable to load new vote history when attempting to change identity at startup \
                 from {} to {} on voting loop startup, Exiting: {}",
                vctx.vote_history.node_pubkey,
                ctx.cluster_info.id(),
                e
            );
            return Err(EventLoopError::SetIdentityError(e));
        }
        Self::send_vote_history_to_consensus_pool(&local_context.my_pubkey, &mut vctx)?;

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
            if event.should_ignore(root_bank.slot().max(vctx.vote_history.root())) {
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
                match vctx.bls_sender.try_send(vote) {
                    Ok(_) => (),
                    Err(TrySendError::Full(_)) => {
                        warn!("Vote propagation is backed up, skipping send");
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        return Err(EventLoopError::ReceiverDisconnected(RecvError));
                    }
                }
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

        // We need to ensure that we've replayed the parent bank.
        if parent_block.slot > vctx.sharable_banks.root().slot() {
            request_switch(&ctx.latest_switch_request, *my_pubkey, parent_block);
        }

        vctx.vote_history.add_parent_ready(slot, parent_block);
        Self::check_pending_blocks(my_pubkey, &mut local_context.pending_blocks, vctx, votes)?;
        let root_bank = vctx.sharable_banks.root();
        let delta_block = Duration::from_nanos_u128(root_bank.ns_per_slot_at_slot(slot));
        let delta_first_fec_set = delta_block;
        let timeout_inserted = timer_manager.write().set_timeouts(
            slot,
            local_context.standstill_slot,
            delta_first_fec_set,
            delta_block,
        );
        if timeout_inserted {
            local_context.stats.timeout_set = local_context.stats.timeout_set.saturating_add(1);
        }

        let mut highest_parent_ready = ctx.highest_parent_ready.write().unwrap();
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
        let &mut LocalContext {
            ref mut my_pubkey,
            ref mut pending_blocks,
            ref mut finalized_blocks,
            ref mut received_shred,
            ref mut stats,
            ref mut standstill_slot,
        } = local_context;
        match event {
            // Block has completed replay
            VotorEvent::Block(CompletedBlock { slot, bank }) => {
                debug_assert!(bank.is_frozen());
                let now = Instant::now();
                let mut consensus_metrics_events =
                    vec![ConsensusMetricsEvent::StartOfSlot { slot }];
                if slot == first_of_consecutive_leader_slots(slot) {
                    // all slots except the first in the window would typically start when the block is seen so the recording would essentially record 0.
                    // hence we skip it.
                    consensus_metrics_events.push(ConsensusMetricsEvent::BlockHashSeen {
                        leader: *bank.leader_id(),
                        slot,
                    });
                }
                vctx.consensus_metrics_sender
                    .send((now, consensus_metrics_events))
                    .map_err(|_| SendError(()))?;
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
                Self::check_rootable_blocks_and_bank_hash_mismatches(
                    my_pubkey,
                    ctx,
                    vctx,
                    rctx,
                    pending_blocks,
                    finalized_blocks,
                    received_shred,
                    stats,
                );
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
                request_repair(&ctx.repair_event_sender, *my_pubkey, block)?;
            }

            VotorEvent::BlockNotarFallback(block) => {
                info!("{my_pubkey}: Block notar-fallback {block:?}");
                request_repair(&ctx.repair_event_sender, *my_pubkey, block)?;
            }

            VotorEvent::FirstShred(slot) => {
                info!("{my_pubkey}: First shred {slot}");
                received_shred.insert(slot);
            }

            // Received a parent ready notification for `slot`
            VotorEvent::ParentReady { slot, parent_block } => {
                vctx.consensus_metrics_sender
                    .send((
                        Instant::now(),
                        vec![ConsensusMetricsEvent::StartOfSlot { slot }],
                    ))
                    .map_err(|_| SendError(()))?;
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
                    vctx.consensus_metrics_sender
                        .send((
                            Instant::now(),
                            vec![ConsensusMetricsEvent::StartOfSlot {
                                slot: slot.saturating_add(1),
                            }],
                        ))
                        .map_err(|_| SendError(()))?;
                }
                if vctx.vote_history.voted(slot) {
                    return Ok(votes);
                }
                Self::try_skip_window(my_pubkey, slot, vctx, &mut votes)?;
            }

            // We have observed the safe to notar condition, and can send a notar fallback vote
            VotorEvent::SafeToNotar(block) => {
                info!("{my_pubkey}: SafeToNotar {block:?}");
                Self::try_skip_window(my_pubkey, block.slot, vctx, &mut votes)?;
                if vctx.vote_history.its_over(block.slot)
                    || vctx
                        .vote_history
                        .voted_notar_fallback(block.slot, block.block_id)
                {
                    return Ok(votes);
                }
                info!("{my_pubkey}: Voting notarize-fallback for {block:?}");
                if let Some(bls_op) = insert_vote_and_create_bls_message(
                    Vote::new_notarization_fallback_vote(block),
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
                    insert_vote_and_create_bls_message(Vote::new_skip_fallback_vote(slot), vctx)?
                {
                    votes.push(bls_op);
                }
            }

            // It is time to produce our leader window
            VotorEvent::ProduceWindow(window_info) => {
                info!("{my_pubkey}: ProduceWindow {window_info:?}");
                ctx.leader_window_info_sender.send(window_info).unwrap();
            }

            // We have finalized this block consider it for rooting
            VotorEvent::Finalized(block, is_fast_finalization) => {
                info!("{my_pubkey}: Finalized {block:?} fast: {is_fast_finalization}");
                finalized_blocks.insert(block);
                request_repair(&ctx.repair_event_sender, *my_pubkey, block)?;

                Self::check_rootable_blocks_and_bank_hash_mismatches(
                    my_pubkey,
                    ctx,
                    vctx,
                    rctx,
                    pending_blocks,
                    finalized_blocks,
                    received_shred,
                    stats,
                );

                if let Some(slot) = *standstill_slot
                    && block.slot > slot
                {
                    *standstill_slot = None;
                    info!(
                        "{my_pubkey}: Standstill initially detected at slot={slot} has ended at \
                         slot={}. Ending timeout extension",
                        block.slot
                    );
                }

                if let Some(parent_block) =
                    Self::add_missing_parent_ready(block, ctx, vctx, local_context)
                {
                    Self::handle_parent_ready_event(
                        block.slot,
                        parent_block,
                        vctx,
                        ctx,
                        local_context,
                        timer_manager,
                        &mut votes,
                    )?;
                }
                vctx.consensus_metrics_sender
                    .send((
                        Instant::now(),
                        vec![ConsensusMetricsEvent::SlotFinalized { slot: block.slot }],
                    ))
                    .map_err(|_| SendError(()))?;
            }

            // We have not observed a finalization certificate in a while, refresh our votes
            VotorEvent::Standstill(highest_finalized_slot) => {
                info!("{my_pubkey}: Standstill {highest_finalized_slot}");
                // Record the highest finalized slot for dynamic timeout extension.
                match *standstill_slot {
                    Some(old_slot) => {
                        debug_assert_eq!(highest_finalized_slot, old_slot);
                        if highest_finalized_slot != old_slot {
                            warn!(
                                "{my_pubkey}: Standstill for slot {highest_finalized_slot}
                                 issued while standstill for slot {old_slot} active."
                            );
                        }
                    }
                    None => {
                        *standstill_slot = Some(highest_finalized_slot);
                        info!(
                            "{my_pubkey}: Extending timeouts starting at slot \
                             {highest_finalized_slot}"
                        );
                    }
                }
                // certs refresh happens in ConsensusPoolService
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
                Self::send_vote_history_to_consensus_pool(my_pubkey, vctx)?;
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
        let Block { slot, block_id } = finalized_block;
        let first_slot_of_window = first_of_consecutive_leader_slots(slot);
        if first_slot_of_window == 0 {
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
        Some(Block {
            slot: parent_slot,
            block_id: parent_block_id,
        })
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
            vctx.vote_history = VoteHistory::restore(ctx.vote_history_storage.as_ref(), my_pubkey)
                .unwrap_or_else(|_| VoteHistory::new(new_pubkey, 0));
            vctx.identity_keypair = new_identity;
            warn!("set-identity: from {my_old_pubkey} to {my_pubkey}");
        }
        Ok(())
    }

    fn send_vote_history_to_consensus_pool(
        my_pubkey: &Pubkey,
        vctx: &mut VotingContext,
    ) -> Result<(), VoteError> {
        let root = vctx
            .vote_history
            .root()
            .max(vctx.sharable_banks.root().slot());
        let votes = vctx.vote_history.votes_cast_since(root.saturating_sub(1));
        for vote in votes {
            info!("{my_pubkey}: Initializing consensus pool with restored vote {vote:?}");
            create_and_send_own_vote_message(vote, vctx, /* respect_wait_to_vote */ false)?;
        }

        Ok(())
    }

    fn get_block_parent_block(bank: &Bank) -> (Block, Block) {
        let slot = bank.slot();
        let block = Block {
            slot,
            block_id: bank.block_id().expect("Block id must be set upstream"),
        };
        let parent_slot = bank.parent_slot();
        let parent_block_id = bank.parent_block_id().unwrap_or_else(|| {
            // To account for child of genesis and snapshots we insert a
            // default block id here. Charlie is working on a SIMD to add block
            // id to snapshots, which can allow us to remove this and update
            // the default case in parent ready tracker.
            trace!("Using default block id for {slot} parent {parent_slot}");
            Hash::default()
        });
        let parent_block = Block {
            slot: parent_slot,
            block_id: parent_block_id,
        };
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
        Block { slot, block_id }: Block,
        parent_block: Block,
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
            if parent_block.slot.saturating_add(1) != slot {
                // Non consecutive
                return Ok(false);
            }
            if voting_context.vote_history.voted_notar(parent_block.slot)
                != Some(parent_block.block_id)
            {
                // Voted skip, or notarize on a different version of the parent
                return Ok(false);
            }
        }

        info!("{my_pubkey}: Voting notarize for {slot} {block_id}");
        if let Some(bls_op) = insert_vote_and_create_bls_message(
            Vote::new_notarization_vote(Block { slot, block_id }),
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

        Self::try_final(my_pubkey, Block { slot, block_id }, voting_context, votes)?;

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
        block: Block,
        voting_context: &mut VotingContext,
        votes: &mut Vec<BLSOp>,
    ) -> Result<bool, VoteError> {
        if !voting_context.vote_history.is_block_notarized(&block)
            || voting_context.vote_history.its_over(block.slot)
            || voting_context.vote_history.bad_window(block.slot)
        {
            return Ok(false);
        }

        if voting_context
            .vote_history
            .voted_notar(block.slot)
            .is_none_or(|bid| bid != block.block_id)
        {
            return Ok(false);
        }

        info!("{my_pubkey}: Voting finalize for {}", block.slot);
        if let Some(bls_op) = insert_vote_and_create_bls_message(
            Vote::new_finalization_vote(block.slot),
            voting_context,
        )? {
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
        let root_slot = voting_context
            .vote_history
            .root()
            .max(voting_context.sharable_banks.root().slot());
        // No matter what happens, we should not vote skip for slot 0
        let start = first_of_consecutive_leader_slots(slot)
            .max(root_slot)
            .max(1);
        for s in start..=last_of_consecutive_leader_slots(slot) {
            if voting_context.vote_history.voted(s) {
                continue;
            }
            info!("{my_pubkey}: Voting skip for {s}");
            if let Some(bls_op) =
                insert_vote_and_create_bls_message(Vote::new_skip_vote(s), voting_context)?
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
        let mut refreshed_votes = Vec::new();
        for vote in voting_context
            .vote_history
            .votes_cast_since(highest_finalized_slot)
        {
            info!("{my_pubkey}: Refreshing vote {vote:?}");
            if let Some(vote_msg) = generate_refresh_vote_message(vote, voting_context)? {
                refreshed_votes.push(Arc::new(vote_msg));
            }
        }
        if !refreshed_votes.is_empty() {
            votes.push(BLSOp::RefreshVotes {
                votes: refreshed_votes,
            });
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
    ///
    /// Additionally check if any of the finalized blocks is frozen with a bank hash mismatch.
    /// If so panic as it is unrecoverable.
    fn check_rootable_blocks_and_bank_hash_mismatches(
        my_pubkey: &Pubkey,
        ctx: &SharedContext,
        vctx: &mut VotingContext,
        rctx: &RootContext,
        pending_blocks: &mut PendingBlocks,
        finalized_blocks: &mut BTreeSet<Block>,
        received_shred: &mut BTreeSet<Slot>,
        stats: &mut EventHandlerStats,
    ) {
        let bank_forks_r = ctx.bank_forks.read().unwrap();
        let old_root = bank_forks_r.root().max(vctx.vote_history.root());
        let Some(new_root) = finalized_blocks
            .iter()
            .filter_map(|&block| {
                let bank = bank_forks_r.get(block.slot)?;

                // Check for bank hash mismatch
                if bank.is_frozen()
                    && bank.block_id() == Some(block.block_id)
                    && let Some(expected_hash) = bank.expected_bank_hash()
                    && expected_hash != bank.hash()
                {
                    panic!(
                        "{my_pubkey}: Block {block:?} has been finalized, however we have a bank \
                         hash mismatch. The cluster bank hash is {expected_hash} however we \
                         computed {}. At this point we will be unable to recover. Ensure that you \
                         are running a supported Agave version for this cluster. If this is not \
                         operator error,please save a copy of your ledger to share on discord and \
                         restart from a snapshot > {}.",
                        bank.hash(),
                        block.slot
                    );
                }

                // Check if this block is rootable
                (block.slot > old_root
                    && vctx.vote_history.voted(block.slot)
                    && bank.is_frozen()
                    && bank.block_id().is_some_and(|bid| bid == block.block_id))
                .then_some(block.slot)
            })
            .max()
        else {
            // No rootable banks
            return;
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
        );
        stats.set_root(new_root);
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_event_handler.join()
    }
}

/// Sends a repair event to the block ID repair service.
fn request_repair(
    sender: &RepairEventSender,
    my_pubkey: Pubkey,
    block: Block,
) -> Result<(), EventLoopError> {
    let event = RepairEvent::FetchBlock { block };
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(event)) => {
            error!(
                "{my_pubkey}: Repair event channel is full, this should not happen. Blocking to \
                 send event for slot {}",
                block.slot
            );
            sender
                .send(event)
                .map_err(|_| EventLoopError::SenderDisconnected(SendError(())))
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(EventLoopError::SenderDisconnected(SendError(())))
        }
    }
}

/// Updates the latest switch-bank request for replay to consume.
fn request_switch(latest: &LatestSwitchRequest, my_pubkey: Pubkey, block: Block) {
    let event = SwitchBankEvent::Switch { block };
    if let Some(prev) = latest.try_advance(event) {
        trace!("{my_pubkey}: Overwriting previous switch request {prev:?} with ({block:?})");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            commitment::CommitmentAggregationData,
            event::{LeaderWindowInfo, RepairEventReceiver},
            vote_history_storage::{
                FileVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
                VoteHistoryStorage,
            },
            voting_service::BLSOp,
        },
        agave_votor_messages::{
            consensus_message::{BLS_KEYPAIR_DERIVE_SEED, VoteMessage},
            metric_types::ConsensusMetricsEventReceiver,
            reward_certificate::AddVoteMessage,
            sig_verified_messages::SigVerifiedBatch,
            vote::Vote,
            wire::get_vote_payload_to_sign,
        },
        crossbeam_channel::{Receiver, Sender, TryRecvError, bounded},
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
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
            bank_forks_controller::{BankForksController, BankForksControllerError},
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
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
        tempfile::TempDir,
    };

    struct EventHandlerTestContext {
        bls_receiver: Receiver<BLSOp>,
        commitment_receiver: Receiver<CommitmentAggregationData>,
        own_vote_receiver: Receiver<SigVerifiedBatch>,
        #[allow(dead_code)] // Keep receiver alive to prevent SenderDisconnected errors
        reward_votes_receiver: Receiver<AddVoteMessage>,
        bank_forks: Arc<RwLock<BankForks>>,
        my_bls_keypair: BLSKeypair,
        timer_manager: Arc<PlRwLock<TimerManager>>,
        leader_window_info_receiver: Receiver<LeaderWindowInfo>,
        highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
        drop_bank_receiver: Receiver<Vec<BankWithScheduler>>,
        cluster_info: Arc<ClusterInfo>,
        consensus_metrics_receiver: ConsensusMetricsEventReceiver,
        #[allow(dead_code)] // Keep receiver alive to prevent SenderDisconnected errors
        repair_event_receiver: RepairEventReceiver,
        shared_context: SharedContext,
        voting_context: VotingContext,
        root_context: RootContext,
        local_context: LocalContext,
        bls_ops: Vec<BLSOp>,
        vote_history_storage: Arc<FileVoteHistoryStorage>,
        // Keep the temp directory alive for `vote_history_storage`.
        _vote_history_storage_dir: TempDir,
    }

    struct DirectBankForksController {
        my_pubkey: Pubkey,
        bank_forks: Arc<RwLock<BankForks>>,
        blockstore: Arc<Blockstore>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    }

    impl BankForksController for DirectBankForksController {
        fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError> {
            Ok(self.bank_forks.write().unwrap().insert(bank))
        }

        fn enqueue_set_root(
            &self,
            parent_slot: Slot,
            new_root: Slot,
            highest_super_majority_root: Option<Slot>,
        ) {
            root_utils::check_and_handle_new_root(
                parent_slot,
                new_root,
                None,
                highest_super_majority_root,
                &None,
                &self.drop_bank_sender,
                &self.blockstore,
                &self.leader_schedule_cache,
                &self.bank_forks,
                None,
                &self.my_pubkey,
                |_| {},
            );
        }

        fn clear_bank(&self, slot: Slot) -> Result<(), BankForksControllerError> {
            let bank_to_clear = self.bank_forks.read().unwrap().get_with_scheduler(slot);
            if let Some(bank) = bank_to_clear {
                let _ = bank.wait_for_completed_scheduler();
            }

            self.bank_forks.write().unwrap().clear_bank(slot, false);
            Ok(())
        }
    }

    fn setup() -> EventHandlerTestContext {
        let (bls_sender, bls_receiver) = bounded(1024);
        let (commitment_sender, commitment_receiver) = bounded(1024);
        let (own_vote_sender, own_vote_receiver) = bounded(1024);
        let (reward_votes_sender, reward_votes_receiver) = bounded(1024);
        let (drop_bank_sender, drop_bank_receiver) = bounded(1024);
        let exit = Arc::new(AtomicBool::new(false));
        let (event_sender, _event_receiver) = bounded(1024);
        let (consensus_metrics_sender, consensus_metrics_receiver) = bounded(1024);
        let (leader_window_info_sender, leader_window_info_receiver) = bounded(1024);
        let (repair_event_sender, repair_event_receiver) = bounded(1024);
        let latest_switch_request = LatestSwitchRequest::default();
        let timer_manager = Arc::new(PlRwLock::new(TimerManager::new(
            event_sender,
            exit,
            Arc::new(MigrationStatus::default()),
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
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(
            &bank_forks.read().unwrap().root_bank(),
        ));
        let bank_forks_controller = Arc::new(DirectBankForksController {
            my_pubkey: my_node_keypair.pubkey(),
            bank_forks: bank_forks.clone(),
            blockstore: blockstore.clone(),
            leader_schedule_cache,
            drop_bank_sender: drop_bank_sender.clone(),
        });
        let highest_parent_ready = Arc::new(RwLock::default());

        let vote_history_storage_dir = TempDir::new().unwrap();
        let vote_history_storage = Arc::new(FileVoteHistoryStorage::new(
            vote_history_storage_dir.path().to_path_buf(),
        ));
        let shared_context = SharedContext {
            cluster_info: cluster_info.clone(),
            bank_forks: bank_forks.clone(),
            vote_history_storage: vote_history_storage.clone(),
            leader_window_info_sender,
            blockstore: blockstore.clone(),
            highest_parent_ready: highest_parent_ready.clone(),
            repair_event_sender,
            latest_switch_request,
        };

        let vote_history = VoteHistory::new(my_node_keypair.pubkey(), 0);
        let voting_context = VotingContext {
            cluster_info: cluster_info.clone(),
            identity_keypair: Arc::new(my_node_keypair.insecure_clone()),
            sharable_banks: bank_forks.read().unwrap().sharable_banks(),
            vote_history,
            bls_sender,
            commitment_sender,
            vote_account_pubkey: my_vote_keypair.pubkey(),
            wait_to_vote_slot: None,
            authorized_voter_keypairs: Arc::new(RwLock::new(vec![Arc::new(my_vote_keypair)])),
            vote_history_storage: vote_history_storage.clone(),
            derived_bls_keypairs: HashMap::new(),
            own_vote_sender,
            reward_votes_sender,
            consensus_metrics_sender,
        };

        let root_context = RootContext {
            bank_notification_sender: None,
            bank_forks_controller,
        };

        let local_context = LocalContext {
            my_pubkey: my_node_keypair.pubkey(),
            pending_blocks: BTreeMap::new(),
            finalized_blocks: BTreeSet::new(),
            received_shred: BTreeSet::new(),
            stats: EventHandlerStats::default(),
            standstill_slot: None,
        };

        EventHandlerTestContext {
            bls_receiver,
            commitment_receiver,
            own_vote_receiver,
            reward_votes_receiver,
            bank_forks,
            my_bls_keypair,
            timer_manager,
            leader_window_info_receiver,
            drop_bank_receiver,
            cluster_info,
            consensus_metrics_receiver,
            repair_event_receiver,
            highest_parent_ready,
            shared_context,
            voting_context,
            root_context,
            local_context,
            bls_ops: vec![],
            vote_history_storage,
            _vote_history_storage_dir: vote_history_storage_dir,
        }
    }

    impl EventHandlerTestContext {
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
                    block_timer: Instant::now(),
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
            let bank = Bank::new_from_parent(parent_bank, SlotLeader::new_unique(), slot);
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
                let expected_message = self.expected_vote_message(v);
                let mut found = false;
                self.bls_ops.retain_mut(|bls_op| match bls_op {
                    BLSOp::PushVote { vote, .. } => {
                        let keep = vote.as_ref() != &expected_message;
                        found |= !keep;
                        keep
                    }
                    BLSOp::RefreshVotes { votes } => {
                        let previous_len = votes.len();
                        votes.retain(|vote| vote.as_ref() != &expected_message);
                        found |= votes.len() != previous_len;
                        !votes.is_empty()
                    }
                    BLSOp::PushCertificates { .. } | BLSOp::RefreshCertificates { .. } => true,
                });
                assert!(found, "Did not find expected vote: {expected_message:?}");
            }
        }

        fn expected_vote_message(&self, expected_vote: &Vote) -> VoteMessage {
            let payload =
                get_vote_payload_to_sign(*expected_vote, self.cluster_info.my_shred_version());
            let signature: BLSSignature = self.my_bls_keypair.sign(&payload).into();
            VoteMessage {
                vote: *expected_vote,
                rank: 0,
                signature,
            }
        }

        fn check_for_vote(&mut self, expected_vote: &Vote) {
            let expected_message = self.expected_vote_message(expected_vote);
            let mut found = false;
            self.bls_ops.retain_mut(|bls_op| match bls_op {
                BLSOp::PushVote { vote, .. } => {
                    let keep = vote.as_ref() != &expected_message;
                    found |= !keep;
                    keep
                }
                BLSOp::RefreshVotes { votes } => {
                    let previous_len = votes.len();
                    votes.retain(|vote| vote.as_ref() != &expected_message);
                    found |= votes.len() != previous_len;
                    !votes.is_empty()
                }
                BLSOp::PushCertificates { .. } | BLSOp::RefreshCertificates { .. } => true,
            });
            assert!(found, "Did not find expected vote: {expected_message:?}");
            // Also check own_vote_receiver
            let own_vote = self.own_vote_receiver.try_recv().unwrap();
            assert_eq!(own_vote, SigVerifiedBatch::Votes(vec![expected_message]));
        }

        fn check_for_own_vote(&self, expected_vote: &Vote) {
            let expected_message = self.expected_vote_message(expected_vote);
            let own_vote = self.own_vote_receiver.try_recv().unwrap();
            assert_eq!(own_vote, SigVerifiedBatch::Votes(vec![expected_message]));
        }

        fn check_for_own_votes(&self, expected_votes: &[Vote]) {
            let mut received_messages = Vec::with_capacity(expected_votes.len());
            for _ in expected_votes {
                let SigVerifiedBatch::Votes(votes) = self.own_vote_receiver.try_recv().unwrap()
                else {
                    panic!("expected own vote");
                };
                assert_eq!(votes.len(), 1);
                received_messages.push(votes[0].clone());
            }

            for expected_vote in expected_votes {
                let expected_message = self.expected_vote_message(expected_vote);
                let index = received_messages
                    .iter()
                    .position(|message| *message == expected_message)
                    .unwrap_or_else(|| panic!("missing own vote {expected_vote:?}"));
                received_messages.remove(index);
            }
            assert!(received_messages.is_empty());
        }

        fn check_no_own_vote(&self) {
            assert_eq!(
                self.own_vote_receiver.try_recv().err(),
                Some(TryRecvError::Empty)
            );
        }

        fn check_for_commitment(&mut self, expected_type: CommitmentType, expected_slot: Slot) {
            let commitment = self.commitment_receiver.try_recv().unwrap();
            assert_eq!(commitment.commitment_type, expected_type);
            assert_eq!(commitment.slot, expected_slot);
        }

        fn check_no_vote_or_commitment(&mut self) {
            assert_eq!(
                self.bls_receiver.try_recv().err(),
                Some(TryRecvError::Empty)
            );
            assert_eq!(
                self.commitment_receiver.try_recv().err(),
                Some(TryRecvError::Empty)
            );
        }

        fn check_parent_ready_slot(&self, expected: (Slot, Block)) {
            assert_eq!(*self.highest_parent_ready.read().unwrap(), expected);
            let slot = expected.0;
            self.check_timeout_set(slot);
        }

        fn check_timeout_set(&self, expected_slot: Slot) {
            assert!(self.timer_manager.read().is_timeout_set(expected_slot));
        }

        fn check_for_metrics_event(&self, expected: ConsensusMetricsEvent) {
            let event = self
                .consensus_metrics_receiver
                .try_recv()
                .expect("Should receive metrics event");
            assert!(event.1.contains(&expected));
        }

        fn crate_vote_history_storage_and_switch_identity(
            &mut self,
            new_identity: &Keypair,
        ) -> PathBuf {
            let saved_vote_history =
                SavedVoteHistory::new(&VoteHistory::new(new_identity.pubkey(), 0), &new_identity)
                    .unwrap();
            assert!(
                self.vote_history_storage
                    .store(&SavedVoteHistoryVersions::from(saved_vote_history),)
                    .is_ok()
            );
            self.cluster_info
                .set_keypair(Arc::new(new_identity.insecure_clone()));

            self.send_set_identity_event();
            self.vote_history_storage.filename(&new_identity.pubkey())
        }
    }

    #[test]
    fn test_received_block_event_and_parent_ready_event() {
        // Test different orders of received block event and parent ready event
        // some will send Notarize immediately, some will wait for parent ready
        let mut test_context = setup();
        // Received block event which says block has completed replay

        // If there is a parent ready for block 1 Notarization is sent out.
        let slot = 1;
        let parent_slot = 0;
        test_context.send_parent_ready_event(
            slot,
            Block {
                slot: parent_slot,
                block_id: Hash::default(),
            },
        );
        test_context.check_parent_ready_slot((
            slot,
            Block {
                slot: parent_slot,
                block_id: Hash::default(),
            },
        ));
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
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot,
            block_id: block_id_1,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
        // Add block event for 1 again will not trigger another Notarize or commitment
        test_context.send_block_event(1, bank1.clone());
        test_context.check_no_vote_or_commitment();

        let slot = 2;
        let bank2 = test_context.create_block_and_send_block_event(slot, bank1.clone());
        let block_id_2 = bank2.block_id().unwrap();

        // Because 2 is middle of window, we should see Notarize vote for block 2 even without parentready
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot,
            block_id: block_id_2,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
        // Slot 3 somehow links to block 1, should not trigger Notarize vote because it has a wrong parent (not 2)
        let _ = test_context.create_block_and_send_block_event(3, bank1);
        test_context.check_no_vote_or_commitment();

        // Slot 4 completed replay without parent ready or parent notarized should not trigger Notarize vote
        let slot = 4;
        let bank4 = test_context.create_block_and_send_block_event(slot, bank2);
        let block_id_4 = bank4.block_id().unwrap();

        // Send parent ready for slot 4 should trigger Notarize vote for slot 4
        test_context.send_parent_ready_event(
            slot,
            Block {
                slot: 2,
                block_id: block_id_2,
            },
        );
        test_context.check_parent_ready_slot((
            slot,
            Block {
                slot: 2,
                block_id: block_id_2,
            },
        ));
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot,
            block_id: block_id_4,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
    }

    #[test]
    fn test_restored_parent_ready_sets_timeout() {
        let mut test_context = setup();
        let slot = 4;
        let parent_block = Block {
            slot: 3,
            block_id: Hash::new_unique(),
        };

        assert!(
            test_context
                .voting_context
                .vote_history
                .add_parent_ready(slot, parent_block)
        );
        assert!(!test_context.timer_manager.read().is_timeout_set(slot));

        test_context.send_parent_ready_event(slot, parent_block);
        test_context.check_timeout_set(slot);
        assert_eq!(test_context.local_context.stats.timeout_set, 1);
    }

    #[test]
    fn test_received_block_notarized_and_timeout() {
        // Test block notarized event will trigger Finalize vote when all conditions are met
        // But it will not trigger Finalize if any of the conditions are not met
        let mut test_context = setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        // Add parent ready for 0 to trigger notar vote for 1
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );
        test_context.check_parent_ready_slot((
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        ));
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);
        // Send block notarized event should trigger Finalize vote
        test_context.send_block_notarized_event(Block {
            slot: 1,
            block_id: block_id_1,
        });
        test_context.check_for_vote(&Vote::new_finalization_vote(1));

        let bank2 = test_context.create_block_and_send_block_event(2, bank1);
        let block_id_2 = bank2.block_id().unwrap();
        // Both Notarize and Finalize votes should trigger for 2
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 2,
            block_id: block_id_2,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, 2);
        test_context.send_block_notarized_event(Block {
            slot: 2,
            block_id: block_id_2,
        });
        test_context.check_for_vote(&Vote::new_finalization_vote(2));

        // Create bank3 but do not Notarize, so Finalize vote should not trigger
        let slot = 3;
        let bank3 = test_context.create_block_only(slot, bank2);
        let block_id_3 = bank3.block_id().unwrap();
        // Check no notarization vote for 3
        test_context.check_no_vote_or_commitment();

        test_context.send_block_notarized_event(Block {
            slot,
            block_id: block_id_3,
        });
        // Check no Finalize vote for 3
        test_context.check_no_vote_or_commitment();

        // Now send Block event simulating replay completed for 3
        test_context.send_block_event(slot, bank3.clone());
        // There should be a notarization vote for 3
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot,
            block_id: block_id_3,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);
        // Check there is a Finalize vote for 3
        test_context.check_for_vote(&Vote::new_finalization_vote(slot));

        // After casting finalization vote for 3, we will not send skip fallback
        test_context.send_safe_to_skip_event(slot);
        test_context.check_no_vote_or_commitment();

        // Simulate that block 4 never arrives, we create block 4 but send timeout event
        let slot = 4;
        let bank4 = test_context.create_block_only(slot, bank3);
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
        let bank5 = test_context.create_block_only(slot, bank4);
        test_context.send_block_event(slot, bank5);
        test_context.check_no_vote_or_commitment();
    }

    #[test]
    fn test_received_timeout_crashed_leader_and_first_shred() {
        let mut test_context = setup();

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
        let mut test_context = setup();

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
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        test_context.check_parent_ready_slot((
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        ));
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1_old,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now we got safe_to_notar event for slot 1 and a different block id
        let block_id_1_1 = Hash::new_unique();
        test_context.send_safe_to_notar_event(Block {
            slot: 1,
            block_id: block_id_1_1,
        });
        // We should see rest of the window skipped
        test_context.check_for_vote(&Vote::new_skip_vote(2));
        test_context.check_for_vote(&Vote::new_skip_vote(3));
        // We should also see notarize fallback for the new block id
        test_context.check_for_vote(&Vote::new_notarization_fallback_vote(Block {
            slot: 1,
            block_id: block_id_1_1,
        }));

        // We can trigger safe_to_notar event again for a different block id
        // In this test you can trigger this any number of times, but the white paper
        // proved we can only get up to 3 different block ids on a slot, and our
        // certificate pool implementation checks that.
        let block_id_1_2 = Hash::new_unique();
        test_context.send_safe_to_notar_event(Block {
            slot: 1,
            block_id: block_id_1_2,
        });
        // No skips this time because we already skipped the rest of the window
        // We should also see notarize fallback for the new block id
        test_context.check_for_vote(&Vote::new_notarization_fallback_vote(Block {
            slot: 1,
            block_id: block_id_1_2,
        }));

        // But getting safe_to_notar for a block id we voted before should be no-op
        test_context.send_safe_to_notar_event(Block {
            slot: 1,
            block_id: block_id_1_1,
        });
        test_context.check_no_vote_or_commitment();
    }

    #[test]
    fn test_received_safe_to_skip() {
        let mut test_context = setup();

        // The safe_to_skip event only fires after we voted notarize for the slot
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank_1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank_1.block_id().unwrap();
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        test_context.check_parent_ready_slot((
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        ));
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1,
        }));
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
        let mut test_context = setup();

        // Produce a full window of blocks
        // Assume the leader for 1-3 is us, send produce window event
        test_context.send_produce_window_event(
            1,
            3,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        // Check that leader_window_info is sent via channel
        let received_leader_window_info =
            test_context.leader_window_info_receiver.try_recv().unwrap();
        assert_eq!(received_leader_window_info.start_slot, 1);
        assert_eq!(received_leader_window_info.end_slot, 3);
        assert_eq!(
            received_leader_window_info.parent_block,
            Block {
                slot: 0,
                block_id: Hash::default()
            }
        );

        // Suddenly I found out I produced block 1 already, send new produce window event
        let block_id_1 = Hash::new_unique();
        test_context.send_produce_window_event(
            2,
            3,
            Block {
                slot: 1,
                block_id: block_id_1,
            },
        );
        let received_leader_window_info =
            test_context.leader_window_info_receiver.try_recv().unwrap();
        assert_eq!(received_leader_window_info.start_slot, 2);
        assert_eq!(received_leader_window_info.end_slot, 3);
        assert_eq!(
            received_leader_window_info.parent_block,
            Block {
                slot: 1,
                block_id: block_id_1
            }
        );
    }

    #[test]
    fn test_received_finalized() {
        let mut test_context = setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        test_context.check_parent_ready_slot((
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        ));
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, 1);
        // Now we got finalized event for slot 1
        test_context.send_finalized_event(
            Block {
                slot: 1,
                block_id: block_id_1,
            },
            true,
        );
        // Listen on drop bank receiver, it should get bank 0
        let dropped_banks = test_context.drop_bank_receiver.try_recv().unwrap();
        assert_eq!(dropped_banks.len(), 1);
        assert_eq!(dropped_banks[0].slot(), 0);
        // The bank forks root should be updated to 1
        assert_eq!(test_context.bank_forks.read().unwrap().root(), 1);
    }

    #[test]
    #[should_panic(expected = "we have a bank hash mismatch")]
    fn test_finalized_block_with_bank_hash_mismatch_panics() {
        let mut test_context = setup();
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank = test_context.create_block_only(1, root_bank);
        let block_id = bank.block_id().unwrap();
        let expected_hash = Hash::new_unique();
        bank.set_expected_bank_hash(expected_hash);

        test_context.send_finalized_event(Block { slot: 1, block_id }, true);
    }

    #[test]
    fn test_finalized_different_block_id_with_bank_hash_mismatch_does_not_panic() {
        let mut test_context = setup();
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank = test_context.create_block_only(1, root_bank);
        let expected_hash = Hash::new_unique();
        bank.set_expected_bank_hash(expected_hash);
        let finalized_block_id = Hash::new_unique();

        test_context.send_finalized_event(
            Block {
                slot: 1,
                block_id: finalized_block_id,
            },
            true,
        );
    }

    #[test]
    fn test_parent_ready_in_middle_of_window() {
        let mut test_context = setup();

        // We just woke up and received finalize for slot 5
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank4 = test_context.create_block_and_send_block_event(4, root_bank);
        let block_id_4 = bank4.block_id().unwrap();

        let bank5 = test_context.create_block_and_send_block_event(5, bank4);
        let block_id_5 = bank5.block_id().unwrap();

        test_context.send_finalized_event(
            Block {
                slot: 5,
                block_id: block_id_5,
            },
            true,
        );

        // We should now have parent ready for slot 5
        test_context.check_parent_ready_slot((
            5,
            Block {
                slot: 4,
                block_id: block_id_4,
            },
        ));

        // We are partitioned off from rest of the network, and suddenly received finalize for
        // slot 9 a little before we finished replay slot 9
        let bank9 = test_context.create_block_only(9, bank5);
        let block_id_9 = bank9.block_id().unwrap();
        test_context.send_finalized_event(
            Block {
                slot: 9,
                block_id: block_id_9,
            },
            true,
        );

        test_context.send_block_event(9, bank9);

        // We should now have parent ready for slot 9
        test_context.check_parent_ready_slot((
            9,
            Block {
                slot: 5,
                block_id: block_id_5,
            },
        ));
    }

    #[test]
    fn test_parent_ready_for_first_slot_of_window() {
        let mut test_context = setup();

        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();

        let bank4 = test_context.create_block_and_send_block_event(4, bank1);
        let block_id_4 = bank4.block_id().unwrap();

        test_context.send_finalized_event(
            Block {
                slot: 4,
                block_id: block_id_4,
            },
            true,
        );

        test_context.check_parent_ready_slot((
            4,
            Block {
                slot: 1,
                block_id: block_id_1,
            },
        ));
    }

    #[test]
    fn test_received_standstill() {
        let mut test_context = setup();

        // Send notarize vote for slot 1 then skip rest of the window
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1,
        }));

        test_context.send_timeout_event(2);
        test_context.check_for_vote(&Vote::new_skip_vote(2));
        test_context.check_for_vote(&Vote::new_skip_vote(3));

        // Send a standstill event with highest finalized at 0, we should refresh all the votes
        test_context.send_standstill_event(0);

        test_context.check_for_votes(&[
            Vote::new_notarization_vote(Block {
                slot: 1,
                block_id: block_id_1,
            }),
            Vote::new_skip_vote(2),
            Vote::new_skip_vote(3),
        ]);

        // Finalize block 1, should deactivate standstill
        test_context.send_finalized_event(
            Block {
                slot: 1,
                block_id: block_id_1,
            },
            true,
        );
        assert!(test_context.local_context.standstill_slot.is_none());

        // Send another standstill event with highest finalized at 1, we should refresh votes for 2 and 3 only
        test_context.send_standstill_event(1);

        test_context.check_for_votes(&[Vote::new_skip_vote(2), Vote::new_skip_vote(3)]);
    }

    #[test]
    fn test_startup_replays_vote_history_to_consensus_pool() {
        let mut test_context = setup();
        let notarize_vote = Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: Hash::new_unique(),
        });
        let skip_vote = Vote::new_skip_vote(2);
        let fallback_vote = Vote::new_skip_fallback_vote(2);
        test_context
            .voting_context
            .vote_history
            .add_vote(notarize_vote);
        test_context.voting_context.vote_history.add_vote(skip_vote);
        test_context
            .voting_context
            .vote_history
            .add_vote(fallback_vote);

        EventHandler::send_vote_history_to_consensus_pool(
            &test_context.local_context.my_pubkey,
            &mut test_context.voting_context,
        )
        .unwrap();

        test_context.check_for_own_votes(&[notarize_vote, skip_vote, fallback_vote]);
        test_context.check_no_own_vote();
        assert!(test_context.bls_ops.is_empty());
        assert_eq!(
            test_context.bls_receiver.try_recv().err(),
            Some(TryRecvError::Empty)
        );
    }

    #[test]
    fn test_received_set_identity() {
        let mut test_context = setup();
        let old_identity = test_context.cluster_info.keypair().insecure_clone();
        let new_identity = Keypair::new();
        let mut files_to_remove = vec![];

        // Before set identity we need to manually create the vote history storage file for new identity
        files_to_remove
            .push(test_context.crate_vote_history_storage_and_switch_identity(&new_identity));

        // Should not send any votes because we set to a different identity
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let _ = test_context.create_block_and_send_block_event(1, root_bank.clone());
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );

        // There should be no votes but we should see commitments for hot spares
        assert_eq!(
            test_context.bls_receiver.try_recv().err(),
            Some(TryRecvError::Empty)
        );
        test_context.check_for_commitment(CommitmentType::Notarize, 1);

        // Now set back to original identity
        files_to_remove
            .push(test_context.crate_vote_history_storage_and_switch_identity(&old_identity));

        // We should now be able to vote again
        let slot = 4;
        let bank4 = test_context.create_block_and_send_block_event(slot, root_bank);
        let block_id_4 = bank4.block_id().unwrap();
        test_context.send_parent_ready_event(
            slot,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot,
            block_id: block_id_4,
        }));
        test_context.check_for_commitment(CommitmentType::Notarize, slot);

        for file in files_to_remove {
            let _ = remove_file(file);
        }
    }

    #[test]
    fn test_set_identity_replays_restored_vote_history_to_consensus_pool() {
        let mut test_context = setup();
        let old_identity = test_context.cluster_info.keypair().insecure_clone();
        let new_identity = Keypair::new();
        let vote_history_storage = test_context.vote_history_storage.clone();

        let new_vote_history = VoteHistory::new(new_identity.pubkey(), 0);
        let saved_vote_history = SavedVoteHistory::new(&new_vote_history, &new_identity).unwrap();
        vote_history_storage
            .store(&SavedVoteHistoryVersions::from(saved_vote_history))
            .unwrap();

        let restored_vote = Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: Hash::new_unique(),
        });
        let mut old_vote_history = VoteHistory::new(old_identity.pubkey(), 0);
        old_vote_history.add_vote(restored_vote);
        let saved_vote_history = SavedVoteHistory::new(&old_vote_history, &old_identity).unwrap();
        vote_history_storage
            .store(&SavedVoteHistoryVersions::from(saved_vote_history))
            .unwrap();

        test_context
            .cluster_info
            .set_keypair(Arc::new(new_identity.insecure_clone()));
        test_context.send_set_identity_event();
        test_context.check_no_own_vote();

        test_context
            .cluster_info
            .set_keypair(Arc::new(old_identity.insecure_clone()));
        test_context.send_set_identity_event();

        assert_eq!(test_context.voting_context.vote_history, old_vote_history);
        test_context.check_for_own_vote(&restored_vote);
        test_context.check_no_own_vote();
        assert!(test_context.bls_ops.is_empty());
    }

    #[test]
    fn test_standstill_slot_tracking() {
        let mut test_context = setup();

        // Initially standstill_slot should be None
        assert!(test_context.local_context.standstill_slot.is_none());

        // Set up some state
        let root_bank = test_context
            .bank_forks
            .read()
            .unwrap()
            .sharable_banks()
            .root();
        let bank1 = test_context.create_block_and_send_block_event(1, root_bank);
        let block_id_1 = bank1.block_id().unwrap();
        test_context.send_parent_ready_event(
            1,
            Block {
                slot: 0,
                block_id: Hash::default(),
            },
        );
        test_context.check_for_vote(&Vote::new_notarization_vote(Block {
            slot: 1,
            block_id: block_id_1,
        }));

        // Send standstill event - should record the standstill slot
        test_context.send_standstill_event(0);

        // The standstill_slot should now be set to the highest finalized
        assert!(test_context.local_context.standstill_slot.is_some());
        let standstill_slot = test_context.local_context.standstill_slot.unwrap();
        // The highest finalized should be 0 since we haven't finalized a slot after genesis
        assert_eq!(standstill_slot, 0);

        // Send another standstill event - should not change the existing standstill_slot
        test_context.send_standstill_event(0);
        assert_eq!(test_context.local_context.standstill_slot, Some(0));

        // Send a finalized event - should reset standstill_slot
        test_context.send_finalized_event(
            Block {
                slot: 1,
                block_id: block_id_1,
            },
            false,
        );
        assert!(test_context.local_context.standstill_slot.is_none());
    }
}
