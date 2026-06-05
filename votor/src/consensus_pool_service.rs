//! Service in charge of ingesting new messages into the certificate pool
//! and notifying votor of new events that occur

mod stats;

use {
    crate::{
        common::DELTA_STANDSTILL,
        consensus_pool::{
            AddVoteError, ConsensusPool,
            parent_ready_tracker::{BlockProductionParent, ParentReady},
        },
        event::{LeaderWindowInfo, RepairEvent, RepairEventSender, VotorEvent, VotorEventSender},
        generated_cert_types::GeneratedCertTypes,
        voting_service::BLSOp,
    },
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{Block, ConsensusMessage, SigVerifiedBatch},
        migration::MigrationStatus,
    },
    crossbeam_channel::{Receiver, Sender, TrySendError, select},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, bank_forks::SharableBanks,
        leader_schedule_utils::last_of_consecutive_leader_slots,
        validated_block_finalization::ValidatedBlockFinalizationCert,
    },
    stats::ConsensusPoolServiceStats,
    std::{
        collections::HashSet,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Inputs for the certificate pool thread
pub(crate) struct ConsensusPoolContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) migration_status: Arc<MigrationStatus>,
    pub(crate) generated_cert_types: Arc<GeneratedCertTypes>,

    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) my_vote_pubkey: Pubkey,
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) sharable_banks: SharableBanks,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub(crate) vote_history_highest_parent_ready: Option<(Slot, Block)>,

    pub(crate) consensus_message_receiver: Receiver<SigVerifiedBatch>,

    pub(crate) bls_sender: Sender<BLSOp>,
    pub(crate) event_sender: VotorEventSender,
    pub(crate) repair_event_sender: RepairEventSender,

    /// Used to communicate the highest finalization cert the pool has observed to the block creation loop.
    pub(crate) highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
}

pub(crate) struct ConsensusPoolService {
    t_ingest: JoinHandle<()>,
}

impl ConsensusPoolService {
    pub(crate) fn new(ctx: ConsensusPoolContext) -> Self {
        let t_ingest = Builder::new()
            .name("solCertPoolIngest".to_string())
            .spawn(move || {
                if let Err(e) = Self::consensus_pool_ingest_loop(ctx) {
                    info!("Certificate pool service exited: {e:?}. Shutting down");
                }
            })
            .unwrap();

        Self { t_ingest }
    }

    fn maybe_update_root_and_send_new_certificates(
        consensus_pool: &mut ConsensusPool,
        sharable_banks: &SharableBanks,
        my_pubkey: &Pubkey,
        bls_sender: &Sender<BLSOp>,
        new_finalized_slot: Option<Slot>,
        new_certificates_to_send: Vec<Arc<Certificate>>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
        highest_finalized: &RwLock<Option<ValidatedBlockFinalizationCert>>,
    ) -> Result<(), AddVoteError> {
        // If we have a new finalized slot, update the root and send new certificates
        if new_finalized_slot.is_some() {
            // Reset standstill timer
            *standstill_timer = Instant::now();
            stats.new_finalized_slot += 1;

            *highest_finalized.write().unwrap() = consensus_pool.get_highest_finalization_certs();
        }
        let bank = sharable_banks.root();
        consensus_pool.maybe_prune(bank.slot());
        stats.prune_old_state_called += 1;
        // Send new certificates to peers
        Self::send_certificates(
            sharable_banks,
            my_pubkey,
            bls_sender,
            new_certificates_to_send,
            stats,
        )
    }

    fn send_certificates(
        sharable_banks: &SharableBanks,
        my_pubkey: &Pubkey,
        bls_sender: &Sender<BLSOp>,
        certificates_to_send: Vec<Arc<Certificate>>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), AddVoteError> {
        let num_certs = certificates_to_send.len();
        if num_certs == 0 {
            return Ok(());
        }
        // If we are not a staked identity (hot spare / RPC / new validator / failed VAT)
        // we should not send out the certificate. A2A quic only accepts connections
        // from staked identities
        if !Self::is_current_identity_staked(sharable_banks, my_pubkey) {
            stats.certificates_skipped_unstaked += num_certs;
            return Ok(());
        }
        Self::enqueue_certificates(bls_sender, certificates_to_send, stats)
    }

    fn is_current_identity_staked(sharable_banks: &SharableBanks, my_pubkey: &Pubkey) -> bool {
        sharable_banks
            .root()
            .current_epoch_staked_nodes()
            .get(my_pubkey)
            .is_some_and(|stake| *stake > 0)
    }

    fn enqueue_certificates(
        bls_sender: &Sender<BLSOp>,
        certificates_to_send: Vec<Arc<Certificate>>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), AddVoteError> {
        let num_certs = certificates_to_send.len();
        for (i, certificate) in certificates_to_send.into_iter().enumerate() {
            // The buffer should normally be large enough, so we don't handle
            // certificate re-send here.
            match bls_sender.try_send(BLSOp::PushCertificate { certificate }) {
                Ok(_) => {
                    stats.certificates_sent += 1;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(AddVoteError::ChannelDisconnected(
                        "VotingService".to_string(),
                    ));
                }
                Err(TrySendError::Full(_)) => {
                    stats.certificates_dropped += num_certs.saturating_sub(i);
                    return Err(AddVoteError::VotingServiceQueueFull);
                }
            }
        }
        Ok(())
    }

    fn process_batch(
        ctx: &mut ConsensusPoolContext,
        msgs: impl Iterator<Item = ConsensusMessage>,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), AddVoteError> {
        for msg in msgs {
            match Self::process_consensus_message(
                ctx,
                msg,
                consensus_pool,
                events,
                standstill_timer,
                stats,
            ) {
                Ok(()) => {}
                Err(AddVoteError::ChannelDisconnected(channel_name)) => {
                    return Err(AddVoteError::ChannelDisconnected(channel_name));
                }
                Err(e) => {
                    // This is a non critical error, a duplicate vote for example
                    trace!(
                        "{}: unable to push vote into pool {}",
                        ctx.cluster_info.id(),
                        e
                    );
                    stats.add_message_failed += 1;
                }
            }
        }
        Ok(())
    }

    fn process_consensus_message(
        ctx: &mut ConsensusPoolContext,
        message: ConsensusMessage,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), AddVoteError> {
        match message {
            ConsensusMessage::Certificate(_) => {
                stats.received_certificates += 1;
            }
            ConsensusMessage::Vote(_) => {
                stats.received_votes += 1;
            }
        }
        let root_bank = ctx.sharable_banks.root();
        let (new_finalized_slot, new_certificates_to_send) = Self::add_message(
            &root_bank,
            &ctx.cluster_info.id(),
            &ctx.my_vote_pubkey,
            message,
            consensus_pool,
            events,
            stats,
        )?;
        Self::maybe_update_root_and_send_new_certificates(
            consensus_pool,
            &ctx.sharable_banks,
            &ctx.cluster_info.id(),
            &ctx.bls_sender,
            new_finalized_slot,
            new_certificates_to_send,
            standstill_timer,
            stats,
            &ctx.highest_finalized,
        )
    }

    fn handle_channel_disconnected(
        ctx: &mut ConsensusPoolContext,
        channel_name: &str,
    ) -> Result<(), ()> {
        info!(
            "{}: {} disconnected. Exiting",
            ctx.cluster_info.id(),
            channel_name
        );
        ctx.exit.store(true, Ordering::Relaxed);
        Err(())
    }

    /// Finds the initial parent ready that we should use for instantiating the ConsensusPool or kicking off votor
    /// The max of genesis block, root block, or the restored parent ready from vote history
    fn initial_parent_ready(
        genesis_block: Option<Block>,
        root_block: Block,
        vote_history_highest_parent_ready: Option<(Slot, Block)>,
    ) -> ParentReady {
        let Some(genesis_block) = genesis_block else {
            // Alpenglow is not yet enabled, start with just the root
            return (root_block.slot.checked_add(1).unwrap(), root_block);
        };

        let initial_block = genesis_block.max(root_block);
        let initial_parent_ready = (initial_block.slot.checked_add(1).unwrap(), initial_block);

        if let Some(restored @ (restored_slot, _)) = vote_history_highest_parent_ready
            && restored_slot > initial_parent_ready.0
        {
            restored
        } else {
            initial_parent_ready
        }
    }

    fn root_block(root_bank: &Bank) -> Block {
        Block {
            slot: root_bank.slot(),
            block_id: root_bank
                .block_id()
                // Once SIMD-0333 is active we can hard unwrap here
                .unwrap_or_default(),
        }
    }

    // Main loop for the certificate pool service, it only exits when any channel is disconnected
    fn consensus_pool_ingest_loop(mut ctx: ConsensusPoolContext) -> Result<(), ()> {
        let mut events = vec![];
        let root_bank = ctx.sharable_banks.root();

        let initial_parent_ready = Self::initial_parent_ready(
            ctx.migration_status.genesis_block(),
            Self::root_block(&root_bank),
            ctx.vote_history_highest_parent_ready,
        );

        // Unlike the other votor threads, consensus pool starts even before Alpenglow is enabled
        // because it must track the genesis vote.
        let mut consensus_pool = ConsensusPool::new(
            ctx.cluster_info.clone(),
            &root_bank,
            ctx.generated_cert_types.clone(),
            ctx.migration_status.clone(),
            initial_parent_ready,
        );

        info!("{}: Certificate pool loop starting", ctx.cluster_info.id());
        let mut stats = ConsensusPoolServiceStats::new();
        let mut highest_parent_ready = root_bank.slot();

        // Standstill tracking
        let mut standstill_timer = Instant::now();

        // Kick off parent ready
        let mut kick_off_parent_ready = false;

        // Track pending safe-to-notar blocks for intrawindow slots.
        let mut pending_safe_to_notar = HashSet::new();

        // Ingest votes into certificate pool and notify voting loop of new events
        while !ctx.exit.load(Ordering::Relaxed) {
            // Kick off parent ready event, this either happens:
            // - When we first migrate to alpenglow from TowerBFT - kick off with genesis block
            // - If we startup post alpenglow migration - kick off with root block
            // - If restored vote history is farther ahead, resume from its highest parent-ready
            if !kick_off_parent_ready && ctx.migration_status.is_alpenglow_enabled() {
                let root_bank = ctx.sharable_banks.root();
                let (slot, parent_block) = Self::initial_parent_ready(
                    ctx.migration_status.genesis_block(),
                    Self::root_block(&root_bank),
                    ctx.vote_history_highest_parent_ready,
                );
                events.push(VotorEvent::ParentReady { slot, parent_block });
                kick_off_parent_ready = true;
                // Intentionally do not increment `highest_parent_ready` in case
                // this is a cluster restart and we are the very first leader.
            }

            Self::add_produce_block_event(
                &mut highest_parent_ready,
                &consensus_pool,
                &mut ctx,
                &mut events,
                &mut stats,
            );

            if standstill_timer.elapsed() > DELTA_STANDSTILL {
                // No reason to pollute channel with Standstill before the
                // migration is complete. We still need standstill to refresh the
                // Genesis cert though.
                if kick_off_parent_ready {
                    events.push(VotorEvent::Standstill(
                        consensus_pool
                            .highest_finalized_slot()
                            .unwrap_or(ctx.sharable_banks.root().slot()),
                    ));
                }
                stats.standstill = true;
                standstill_timer = Instant::now();
                match Self::send_certificates(
                    &ctx.sharable_banks,
                    &ctx.cluster_info.id(),
                    &ctx.bls_sender,
                    consensus_pool.get_certs_for_standstill(),
                    &mut stats,
                ) {
                    Ok(()) => (),
                    Err(AddVoteError::ChannelDisconnected(channel_name)) => {
                        return Self::handle_channel_disconnected(&mut ctx, channel_name.as_str());
                    }
                    Err(e) => {
                        trace!(
                            "{}: unable to push standstill certificates into pool {e}",
                            ctx.cluster_info.id()
                        );
                    }
                }
            }

            // Process pending safe-to-notar blocks for intrawindow slots
            Self::process_pending_safe_to_notar(
                &ctx,
                &mut consensus_pool,
                &mut pending_safe_to_notar,
                &mut events,
                &mut stats,
            )?;

            if events
                .drain(..)
                .try_for_each(|event| ctx.event_sender.send(event))
                .is_err()
            {
                return Self::handle_channel_disconnected(&mut ctx, "event_sender");
            }

            let wait_timeout = if pending_safe_to_notar.is_empty() {
                Duration::from_secs(1)
            } else {
                // If there are pending blocks that are waiting for repair in order to emit
                // SafeToNotar events, use a shorter timeout
                Duration::from_millis(20)
            };

            let batches: Vec<SigVerifiedBatch> = select! {
                recv(ctx.consensus_message_receiver) -> msg => {
                    let Ok(first) = msg else {
                        return Self::handle_channel_disconnected(&mut ctx, "consensus_message_receiver");
                    };
                    std::iter::once(first).chain(ctx.consensus_message_receiver.try_iter()).collect()
                },
                default(wait_timeout) => continue
            };

            for batch in batches {
                let ret = match batch {
                    SigVerifiedBatch::Votes(votes) => Self::process_batch(
                        &mut ctx,
                        votes.into_iter().map(ConsensusMessage::Vote),
                        &mut consensus_pool,
                        &mut events,
                        &mut standstill_timer,
                        &mut stats,
                    ),
                    SigVerifiedBatch::Certificates(certs) => Self::process_batch(
                        &mut ctx,
                        certs.into_iter().map(ConsensusMessage::Certificate),
                        &mut consensus_pool,
                        &mut events,
                        &mut standstill_timer,
                        &mut stats,
                    ),
                };
                match ret {
                    Ok(()) => {}
                    Err(AddVoteError::ChannelDisconnected(channel_name)) => {
                        return Self::handle_channel_disconnected(&mut ctx, channel_name.as_str());
                    }
                    Err(_) => {
                        // error was handled in process_batch.
                    }
                }
            }
            stats.maybe_report();
            consensus_pool.maybe_report();
        }
        Ok(())
    }

    /// Adds a vote to the certificate pool.
    ///
    /// If a new finalization slot was recognized, returns the slot
    fn add_message(
        root_bank: &Bank,
        my_pubkey: &Pubkey,
        my_vote_pubkey: &Pubkey,
        message: ConsensusMessage,
        consensus_pool: &mut ConsensusPool,
        votor_events: &mut Vec<VotorEvent>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(Option<Slot>, Vec<Arc<Certificate>>), AddVoteError> {
        let (new_finalized_slot, new_certificates_to_send) =
            consensus_pool.add_message(root_bank, my_vote_pubkey, message, votor_events)?;
        let Some(new_finalized_slot) = new_finalized_slot else {
            return Ok((None, new_certificates_to_send));
        };
        trace!("{my_pubkey}: new finalization certificate for {new_finalized_slot}");
        // RPC-facing finalized commitment is updated after votor selects a root.
        stats.standstill = false;
        Ok((Some(new_finalized_slot), new_certificates_to_send))
    }

    fn add_produce_block_event(
        highest_parent_ready: &mut Slot,
        consensus_pool: &ConsensusPool,
        ctx: &mut ConsensusPoolContext,
        events: &mut Vec<VotorEvent>,
        stats: &mut ConsensusPoolServiceStats,
    ) {
        let Some(new_highest_parent_ready) = events
            .iter()
            .filter_map(|event| match event {
                VotorEvent::ParentReady { slot, .. } => Some(slot),
                _ => None,
            })
            .max()
            .copied()
        else {
            return;
        };

        if new_highest_parent_ready <= *highest_parent_ready {
            return;
        }
        *highest_parent_ready = new_highest_parent_ready;

        let root_bank = ctx.sharable_banks.root();
        let Some(slot_leader) = ctx
            .leader_schedule_cache
            .slot_leader_at(*highest_parent_ready, Some(&root_bank))
        else {
            error!(
                "my_pubkey={}: unable to compute leader: \
                 highest_parent_ready={highest_parent_ready} root_bank_slot={}.  Exiting",
                ctx.cluster_info.id(),
                root_bank.slot()
            );
            ctx.exit.store(true, Ordering::Relaxed);
            return;
        };

        if slot_leader.id != ctx.cluster_info.id() {
            return;
        }

        let start_slot = *highest_parent_ready;
        let end_slot = last_of_consecutive_leader_slots(start_slot);

        match consensus_pool
            .parent_ready_tracker
            .block_production_parent(start_slot)
        {
            BlockProductionParent::MissedWindow => {
                warn!(
                    "{}: Leader slot {start_slot} has already been certified, skipping production \
                     of {start_slot}-{end_slot}",
                    ctx.cluster_info.id()
                );
                stats.parent_ready_missed_window += 1;
            }
            BlockProductionParent::ParentNotReady => {
                unreachable!(
                    "Must have block production parent: {:#?}",
                    consensus_pool.parent_ready_tracker
                )
            }
            BlockProductionParent::Parent(parent_block) => {
                events.push(VotorEvent::ProduceWindow(LeaderWindowInfo {
                    start_slot,
                    end_slot,
                    parent_block,
                    block_timer: Instant::now(),
                }));
                stats.parent_ready_produce_window += 1;
            }
        }
    }

    /// Process pending safe-to-notar blocks for intrawindow slots.
    ///
    /// For each pending block:
    /// 1. If it's new send a repair request
    /// 2. If the slot is <= highest_finalized_slot, discard it
    /// 3. Check if the block has been received in blockstore
    /// 4. If received, verify the parent has a NotarizeFallback certificate
    /// 5. If verified, emit SafeToNotar event and remove from pending
    fn process_pending_safe_to_notar(
        ctx: &ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        pending_safe_to_notar: &mut HashSet<Block>,
        events: &mut Vec<VotorEvent>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), ()> {
        // First, collect new pending blocks from the consensus pool and send them for repair
        for block in consensus_pool.take_pending_safe_to_notar() {
            if pending_safe_to_notar.contains(&block) {
                continue;
            }
            match ctx
                .repair_event_sender
                .try_send(RepairEvent::FetchBlock { block })
            {
                Ok(()) => {
                    stats.pending_safe_to_notar_repair_sent += 1;
                    pending_safe_to_notar.insert(block);
                }
                Err(TrySendError::Full(event)) => {
                    error!(
                        "Repair event channel for event={event:?} is full. Will try event in next \
                         iteration."
                    );
                    consensus_pool.add_to_pending_safe_to_notar(block);
                }
                Err(TrySendError::Disconnected(_)) => return Err(()),
            }
        }

        let highest_finalized = consensus_pool.highest_finalized_slot().unwrap_or(0);

        pending_safe_to_notar.retain(|&block| {
            // Discard if slot is at or below highest finalized
            if block.slot <= highest_finalized {
                return false;
            }

            // Check if we've received the full block in blockstore
            let Some(location) = ctx
                .blockstore
                .get_block_location(block.slot, block.block_id)
                .expect("Blockstore operations must succeed")
            else {
                // Block not yet received, keep waiting
                return true;
            };

            // Block has been received, get the parent meta
            let slot_meta = ctx
                .blockstore
                .meta_from_location(block.slot, location)
                .expect("Blockstore operations must succeed")
                .expect("SlotMeta must exist if block location is present");

            let parent_block = Block {
                slot: slot_meta
                    .parent_slot
                    .expect("parent slot must exist for full blocks"),
                block_id: slot_meta.parent_block_id,
            };

            // Check if the parent has a NotarizeFallback certificate (or stronger)
            if consensus_pool.block_has_notar_fallback_or_stronger(parent_block) {
                // All conditions met - emit SafeToNotar event
                events.push(VotorEvent::SafeToNotar(block));
                stats.pending_safe_to_notar_resolved += 1;
                return false;
            }

            // Parent doesn't have the certificate yet, keep waiting
            true
        });

        Ok(())
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_ingest.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::tests::get_cluster_info,
        agave_votor_messages::{
            certificate::CertificateType,
            consensus_message::{BLS_KEYPAIR_DERIVE_SEED, VoteMessage},
            vote::Vote,
        },
        crossbeam_channel::unbounded,
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, keypair::Keypair as BLSKeypair,
            signature::Signature as BLSSignature,
        },
        solana_gossip::cluster_info::ClusterInfo,
        solana_hash::Hash,
        solana_ledger::get_tmp_ledger_path_auto_delete,
        solana_runtime::{
            bank_forks::{BankForks, SharableBanks},
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        std::sync::Arc,
    };

    struct TestContext {
        consensus_pool: ConsensusPool,
        bls_sender: Sender<BLSOp>,
        bls_receiver: Receiver<BLSOp>,
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
        my_pubkey: Pubkey,
        my_vote_pubkey: Pubkey,
        blockstore: Arc<Blockstore>,
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
    }

    impl Default for TestContext {
        fn default() -> Self {
            let (bls_sender, bls_receiver) = crossbeam_channel::unbounded();
            // Create 10 node validatorvotekeypairs vec
            let validator_keypairs = (0..10)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            // Make stake monotonic decreasing so rank is deterministic
            let stake = (0..validator_keypairs.len())
                .rev()
                .map(|i| (i.saturating_add(5).saturating_mul(100)) as u64)
                .collect::<Vec<_>>();
            let genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                stake,
            );
            let my_keypair = validator_keypairs[0].node_keypair.insecure_clone();
            let my_pubkey = my_keypair.pubkey();
            let bank0 = Bank::new_for_tests(&genesis.genesis_config);
            let bank_forks = BankForks::new_rw_arc(bank0);

            let ledger_path = get_tmp_ledger_path_auto_delete!();
            let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            let leader_schedule_cache =
                Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

            let root_bank = sharable_banks.root();
            let cluster_info = get_cluster_info(my_keypair.insecure_clone());
            let root_block = Block {
                slot: root_bank.slot(),
                block_id: root_bank.block_id().unwrap_or_default(),
            };
            let initial_parent_ready = (root_bank.slot().checked_add(1).unwrap(), root_block);
            let consensus_pool = ConsensusPool::new(
                cluster_info.clone(),
                &root_bank,
                Arc::new(GeneratedCertTypes::default()),
                Arc::new(MigrationStatus::post_migration_status()),
                initial_parent_ready,
            );
            let my_vote_pubkey = Pubkey::new_unique();

            TestContext {
                consensus_pool,
                bls_sender,
                bls_receiver,
                validator_keypairs,
                leader_schedule_cache,
                sharable_banks,
                my_pubkey,
                my_vote_pubkey,
                blockstore,
                exit: Arc::new(AtomicBool::new(false)),
                cluster_info,
                highest_finalized: Arc::new(RwLock::new(None)),
            }
        }
    }

    /// Test the full consensus message flow:
    /// 1. Validators 0-7 send notarize votes for slot 2. After processing all
    ///    votes, we expect a notarize/finalize certificate to be produced and
    ///    forwarded via the BLS channel and a finalized event to be emitted.
    /// 2. A skip certificate is then sent for slot 3 and we verify it is
    ///    immediately forwarded via the BLS channel.
    #[test]
    fn test_receive_and_send_consensus_message() {
        agave_logger::setup();
        let mut ctx = TestContext::default();

        // validator 0 to 7 send Notarize on slot 2
        let block_id = Hash::new_unique();
        let target_slot = 2;
        let notarize_vote = Vote::new_notarization_vote(Block {
            slot: target_slot,
            block_id,
        });

        let mut events = vec![];
        let root_bank = ctx.sharable_banks.root();

        // Process votes from validators 0-7
        for my_rank in 0..8 {
            let vote_keypair = &ctx.validator_keypairs[my_rank].vote_keypair;
            let bls_keypair =
                BLSKeypair::derive_from_signer(vote_keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap();
            let vote_serialized = wincode::serialize(&notarize_vote).unwrap();
            let message = ConsensusMessage::Vote(VoteMessage {
                vote: notarize_vote,
                signature: bls_keypair.sign(&vote_serialized).into(),
                rank: my_rank as u16,
            });

            let mut stats = ConsensusPoolServiceStats::new();
            let result = ConsensusPoolService::add_message(
                &root_bank,
                &ctx.my_pubkey,
                &ctx.my_vote_pubkey,
                message,
                &mut ctx.consensus_pool,
                &mut events,
                &mut stats,
            );
            assert!(result.is_ok());

            let (new_finalized_slot, new_certificates_to_send) = result.unwrap();
            let mut standstill_timer = Instant::now();

            // Send certificates if any were produced
            if !new_certificates_to_send.is_empty() || new_finalized_slot.is_some() {
                ConsensusPoolService::maybe_update_root_and_send_new_certificates(
                    &mut ctx.consensus_pool,
                    &ctx.sharable_banks,
                    &ctx.my_pubkey,
                    &ctx.bls_sender,
                    new_finalized_slot,
                    new_certificates_to_send,
                    &mut standstill_timer,
                    &mut stats,
                    &ctx.highest_finalized,
                )
                .unwrap();
            }
        }

        // Verify that we received certificates via the bls channel
        let mut found_certificate = false;
        while let Ok(event) = ctx.bls_receiver.try_recv() {
            if let BLSOp::PushCertificate { certificate } = event {
                assert_eq!(certificate.cert_type.slot(), target_slot);
                assert!(
                    matches!(certificate.cert_type, CertificateType::Notarize(_))
                        || matches!(certificate.cert_type, CertificateType::FinalizeFast(_))
                        || matches!(certificate.cert_type, CertificateType::NotarizeFallback(_,))
                );
                found_certificate = true;
            }
        }
        assert!(found_certificate, "Should have received a certificate");

        // Verify that we received a finalized slot event
        let finalized_event = events.iter().find(|event| match event {
            VotorEvent::Finalized(block, is_fast_finalized) => {
                block.slot == target_slot && block.block_id == block_id && *is_fast_finalized
            }
            _ => false,
        });
        assert!(
            finalized_event.is_some(),
            "Should have received a finalized event"
        );

        // Now send a Skip certificate on slot 3, should be forwarded immediately
        let target_slot = 3;
        let skip_certificate = Certificate {
            cert_type: CertificateType::Skip(target_slot),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        };
        events.clear();

        let mut stats = ConsensusPoolServiceStats::new();
        let result = ConsensusPoolService::add_message(
            &root_bank,
            &ctx.my_pubkey,
            &ctx.my_vote_pubkey,
            ConsensusMessage::Certificate(skip_certificate),
            &mut ctx.consensus_pool,
            &mut events,
            &mut stats,
        );
        assert!(result.is_ok());

        let (new_finalized_slot, new_certificates_to_send) = result.unwrap();
        let mut standstill_timer = Instant::now();

        ConsensusPoolService::maybe_update_root_and_send_new_certificates(
            &mut ctx.consensus_pool,
            &ctx.sharable_banks,
            &ctx.my_pubkey,
            &ctx.bls_sender,
            new_finalized_slot,
            new_certificates_to_send,
            &mut standstill_timer,
            &mut stats,
            &ctx.highest_finalized,
        )
        .unwrap();

        // Verify skip certificate was forwarded
        let mut found_skip = false;
        while let Ok(event) = ctx.bls_receiver.try_recv() {
            if let BLSOp::PushCertificate { certificate } = event {
                if matches!(certificate.cert_type, CertificateType::Skip(slot) if slot == target_slot)
                {
                    found_skip = true;
                }
            }
        }
        assert!(found_skip, "Should have received the skip certificate");
    }

    #[test]
    fn test_send_produce_block_event() {
        let mut ctx = TestContext::default();
        let (repair_event_sender, _repair_event_receiver) = unbounded();

        // Find when is the next leader slot for me (validator 0)
        let next_leader_slot = ctx
            .leader_schedule_cache
            .next_leader_slot(&ctx.my_pubkey, 0, &ctx.sharable_banks.root(), None, 1000000)
            .expect("Should find a leader slot");

        let root_bank = ctx.sharable_banks.root();
        let mut events = vec![];

        // Send skip certificates for all slots up to the next leader slot
        let mut stats = ConsensusPoolServiceStats::new();
        for slot in 1..next_leader_slot.0 {
            let skip_certificate = Certificate {
                cert_type: CertificateType::Skip(slot),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            };

            let result = ConsensusPoolService::add_message(
                &root_bank,
                &ctx.my_pubkey,
                &ctx.my_vote_pubkey,
                ConsensusMessage::Certificate(skip_certificate),
                &mut ctx.consensus_pool,
                &mut events,
                &mut stats,
            );
            assert!(result.is_ok());
        }

        // Now call add_produce_block_event to generate ProduceWindow event
        let mut highest_parent_ready = root_bank.slot();
        let mut pool_ctx = ConsensusPoolContext {
            exit: ctx.exit.clone(),
            migration_status: Arc::new(MigrationStatus::post_migration_status()),
            generated_cert_types: Arc::new(GeneratedCertTypes::default()),
            cluster_info: ctx.cluster_info.clone(),
            my_vote_pubkey: ctx.my_vote_pubkey,
            blockstore: ctx.blockstore.clone(),
            sharable_banks: ctx.sharable_banks.clone(),
            leader_schedule_cache: ctx.leader_schedule_cache.clone(),
            vote_history_highest_parent_ready: None,
            consensus_message_receiver: crossbeam_channel::unbounded().1,
            bls_sender: ctx.bls_sender.clone(),
            event_sender: crossbeam_channel::unbounded().0,
            highest_finalized: ctx.highest_finalized.clone(),
            repair_event_sender,
        };

        // Add a ParentReady event for the slot before our leader slot
        events.push(VotorEvent::ParentReady {
            slot: next_leader_slot.0,
            parent_block: Block {
                slot: next_leader_slot.0 - 1,
                block_id: Hash::new_unique(),
            },
        });

        ConsensusPoolService::add_produce_block_event(
            &mut highest_parent_ready,
            &ctx.consensus_pool,
            &mut pool_ctx,
            &mut events,
            &mut stats,
        );

        // Verify that we received a ProduceWindow event
        let produce_event = events.iter().find(|event| {
            matches!(
                event,
                VotorEvent::ProduceWindow(LeaderWindowInfo { start_slot, .. })
                    if *start_slot == next_leader_slot.0
            )
        });
        assert!(
            produce_event.is_some(),
            "Should have received a ProduceWindow event"
        );
    }

    #[test]
    fn test_can_produce_window_immediately_on_restart() {
        let ctx = TestContext::default();
        let (repair_event_sender, _repair_event_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let (consensus_message_sender, consensus_message_receiver) = unbounded();

        let root_bank = ctx.sharable_banks.root();
        let next_leader_slot = ctx
            .leader_schedule_cache
            .next_leader_slot(&ctx.my_pubkey, 0, &root_bank, None, 1000000)
            .expect("Should find a leader slot")
            .0;
        let restored_parent_ready = (
            next_leader_slot,
            Block {
                slot: next_leader_slot.checked_sub(1).unwrap(),
                block_id: Hash::new_unique(),
            },
        );
        let exit = ctx.exit.clone();

        let pool_ctx = ConsensusPoolContext {
            exit: exit.clone(),
            migration_status: Arc::new(MigrationStatus::post_migration_status()),
            generated_cert_types: Arc::new(GeneratedCertTypes::default()),
            cluster_info: ctx.cluster_info.clone(),
            my_vote_pubkey: ctx.my_vote_pubkey,
            blockstore: ctx.blockstore.clone(),
            sharable_banks: ctx.sharable_banks.clone(),
            leader_schedule_cache: ctx.leader_schedule_cache.clone(),
            vote_history_highest_parent_ready: Some(restored_parent_ready),
            consensus_message_receiver,
            bls_sender: ctx.bls_sender.clone(),
            event_sender,
            highest_finalized: ctx.highest_finalized.clone(),
            repair_event_sender,
        };

        let handle =
            thread::spawn(move || ConsensusPoolService::consensus_pool_ingest_loop(pool_ctx));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_parent_ready = false;
        let mut saw_produce_window = false;
        while Instant::now() < deadline && !saw_produce_window {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = event_receiver.recv_timeout(timeout) else {
                break;
            };

            match event {
                VotorEvent::ParentReady { slot, parent_block } => {
                    saw_parent_ready |= (slot, parent_block) == restored_parent_ready;
                }
                VotorEvent::ProduceWindow(LeaderWindowInfo { start_slot, .. }) => {
                    saw_produce_window |= start_slot == restored_parent_ready.0;
                }
                _ => {}
            }
        }

        exit.store(true, Ordering::Relaxed);
        drop(consensus_message_sender);
        let _ = handle.join().unwrap();

        assert!(
            saw_parent_ready,
            "Should have received kick-off ParentReady"
        );
        assert!(
            saw_produce_window,
            "Should have received ProduceWindow for kick-off ParentReady"
        );
    }

    #[test]
    fn test_kick_off_parent_ready_uses_restored_vote_history() {
        let genesis_block = Some(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let root_block = Block {
            slot: 12,
            block_id: Hash::new_unique(),
        };
        assert_eq!(
            ConsensusPoolService::initial_parent_ready(genesis_block, root_block, None),
            (13, root_block)
        );

        let restored = (
            16,
            Block {
                slot: 15,
                block_id: Hash::new_unique(),
            },
        );
        assert_eq!(
            ConsensusPoolService::initial_parent_ready(genesis_block, root_block, Some(restored)),
            restored
        );

        let stale = (
            12,
            Block {
                slot: 11,
                block_id: Hash::new_unique(),
            },
        );
        assert_eq!(
            ConsensusPoolService::initial_parent_ready(genesis_block, root_block, Some(stale)),
            (13, root_block)
        );
    }

    #[test]
    fn test_send_certificates() {
        let ctx = TestContext::default();

        let certificates = vec![
            Arc::new(Certificate {
                cert_type: CertificateType::Skip(1),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            }),
            Arc::new(Certificate {
                cert_type: CertificateType::Skip(2),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            }),
        ];

        let mut stats = ConsensusPoolServiceStats::new();
        let result = ConsensusPoolService::send_certificates(
            &ctx.sharable_banks,
            &ctx.my_pubkey,
            &ctx.bls_sender,
            certificates.clone(),
            &mut stats,
        );
        assert!(result.is_ok());
        assert_eq!(stats.certificates_sent.0, 2);

        // Verify certificates were received
        let cert1 = ctx.bls_receiver.try_recv();
        assert!(cert1.is_ok());
        if let BLSOp::PushCertificate { certificate } = cert1.unwrap() {
            assert!(matches!(certificate.cert_type, CertificateType::Skip(1)));
        }

        let cert2 = ctx.bls_receiver.try_recv();
        assert!(cert2.is_ok());
        if let BLSOp::PushCertificate { certificate } = cert2.unwrap() {
            assert!(matches!(certificate.cert_type, CertificateType::Skip(2)));
        }
    }

    #[test]
    fn test_send_certificates_skips_unstaked_identity() {
        let ctx = TestContext::default();
        let certificates = vec![
            Arc::new(Certificate {
                cert_type: CertificateType::Skip(1),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            }),
            Arc::new(Certificate {
                cert_type: CertificateType::Skip(2),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            }),
        ];
        let unstaked_identity = Pubkey::new_unique();
        let mut stats = ConsensusPoolServiceStats::new();

        let result = ConsensusPoolService::send_certificates(
            &ctx.sharable_banks,
            &unstaked_identity,
            &ctx.bls_sender,
            certificates,
            &mut stats,
        );

        assert!(result.is_ok());
        assert_eq!(stats.certificates_sent.0, 0);
        assert_eq!(stats.certificates_skipped_unstaked.0, 2);
        assert!(ctx.bls_receiver.try_recv().is_err());
    }

    #[test]
    fn test_send_certificates_channel_disconnected() {
        let ctx = TestContext::default();
        drop(ctx.bls_receiver); // Disconnect channel

        let certificates = vec![Arc::new(Certificate {
            cert_type: CertificateType::Skip(1),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        })];

        let mut stats = ConsensusPoolServiceStats::new();
        let result = ConsensusPoolService::send_certificates(
            &ctx.sharable_banks,
            &ctx.my_pubkey,
            &ctx.bls_sender,
            certificates,
            &mut stats,
        );

        assert!(matches!(result, Err(AddVoteError::ChannelDisconnected(_))));
    }

    #[test]
    fn test_maybe_update_root_and_send_new_certificates() {
        let mut ctx = TestContext::default();

        let certificates = vec![Arc::new(Certificate {
            cert_type: CertificateType::Skip(1),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        })];

        let mut stats = ConsensusPoolServiceStats::new();
        let mut standstill_timer = Instant::now();

        // Test with new_finalized_slot = Some
        let result = ConsensusPoolService::maybe_update_root_and_send_new_certificates(
            &mut ctx.consensus_pool,
            &ctx.sharable_banks,
            &ctx.my_pubkey,
            &ctx.bls_sender,
            Some(5), // new finalized slot
            certificates,
            &mut standstill_timer,
            &mut stats,
            &ctx.highest_finalized,
        );

        assert!(result.is_ok());
        assert_eq!(stats.new_finalized_slot.0, 1);
        assert_eq!(stats.prune_old_state_called.0, 1);
        assert_eq!(stats.certificates_sent.0, 1);

        // Verify certificate was sent
        let received = ctx.bls_receiver.try_recv();
        assert!(received.is_ok());
    }
}
