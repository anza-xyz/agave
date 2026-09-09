//! Runs the ingest side of the votor consensus pool.
//!
//! - verified vote and certificate batches from sigverify are sent to the pool;
//! - new `VotorEvent`s are forwarded to the voting/event loop;
//! - newly constructed certificates and standstill refreshes are queued for broadcast;
//! - pending intrawindow `SafeToNotar` blocks are repaired and rechecked;

pub(crate) mod staked_status;
mod stats;

use {
    crate::{
        common::{DELTA_STANDSTILL, blocking_send},
        consensus_pool::{
            ConsensusPool,
            parent_ready_tracker::{BlockProductionParent, ParentReady},
        },
        consensus_pool_service::staked_status::StakedStatus,
        event::{LeaderWindowInfo, RepairEvent, RepairEventSender, VotorEvent, VotorEventSender},
        voting_service::BLSOp,
        votor::ExitOnDrop,
    },
    agave_bls_sigverify::generated_cert_types::GeneratedCertTypes,
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{Block, VoteMessage},
        migration::MigrationStatus,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        vote::Vote,
    },
    crossbeam_channel::{Receiver, RecvError, Sender, TrySendError, select_biased},
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, bank_forks::SharableBanks,
        leader_schedule_utils::last_of_consecutive_leader_slots,
        validated_block_finalization::ValidatedBlockFinalizationCert,
    },
    solana_validator_exit::Exit,
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

pub(crate) enum PoolVote {
    Own(VoteMessage),
    External(VoteAggregate),
}

impl PoolVote {
    pub(crate) fn vote(&self) -> &Vote {
        match self {
            Self::Own(vote_msg) => &vote_msg.vote,
            Self::External(m) => m.vote(),
        }
    }
}

pub(crate) enum PoolMessage {
    Votes(Vec<PoolVote>),
    Certificates(Vec<Certificate>),
}

/// Maximum number of messages to process from a selected message channel before
/// returning to channel selection. This keeps a continuously busy channel from
/// starving the other consensus pool input channel.
const MAX_MESSAGES_PER_RECEIVE: u64 = 128;

/// Number of additional messages to drain from the selected message channel
/// after receiving the first message.
const ADDITIONAL_MESSAGES_PER_RECEIVE: u64 = MAX_MESSAGES_PER_RECEIVE - 1;

/// Inputs for the consensus pool and consensus pool service
pub(crate) struct ConsensusPoolContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) validator_exit: Arc<RwLock<Exit>>,
    pub(crate) migration_status: Arc<MigrationStatus>,
    pub(crate) generated_cert_types: Arc<GeneratedCertTypes>,

    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) sharable_banks: SharableBanks,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub(crate) vote_history_highest_parent_ready: Option<(Slot, Block)>,

    pub(crate) consensus_message_receiver: Receiver<SigVerifiedBatch>,
    pub(crate) footer_certs_receiver: Receiver<SmallVec<[Certificate; 2]>>,
    pub(crate) own_votes_receiver: Receiver<VoteMessage>,

    pub(crate) bls_sender: Sender<BLSOp>,
    pub(crate) event_sender: VotorEventSender,
    pub(crate) repair_event_sender: RepairEventSender,
    pub(crate) staked_status: StakedStatus,

    /// Used to communicate the highest finalization cert the pool has observed to the block creation loop.
    pub(crate) highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
}

impl ConsensusPoolContext {
    fn new_consensus_pool(&self) -> ConsensusPool {
        let initial_parent_ready = self.initial_parent_ready();
        let root_bank = self.sharable_banks.root();
        ConsensusPool::new(
            self.cluster_info.clone(),
            &root_bank,
            self.generated_cert_types.clone(),
            self.migration_status.clone(),
            initial_parent_ready,
        )
    }

    /// Finds the initial parent ready that we should use for instantiating the ConsensusPool or kicking off votor
    /// The max of genesis block, root block, or the restored parent ready from vote history
    fn initial_parent_ready(&self) -> ParentReady {
        let root_bank = self.sharable_banks.root();
        let root_block = root_block(&root_bank);
        let genesis_block = self.migration_status.genesis_block();
        Self::_initial_parent_ready(
            genesis_block,
            root_block,
            self.vote_history_highest_parent_ready,
        )
    }

    /// Pulled the implementation outside to enable testing.
    fn _initial_parent_ready(
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
}

pub(crate) struct ConsensusPoolService {
    t_consensus_pool_service: JoinHandle<()>,
}

impl ConsensusPoolService {
    pub(crate) fn new(mut ctx: ConsensusPoolContext) -> Self {
        let t_consensus_pool_service = Builder::new()
            .name("solVotorPoolSvc".to_string())
            .spawn(move || {
                // Dropped before `ctx`, so the channel senders it owns outlive the shutdown.
                let _exit_on_drop = ExitOnDrop::new(ctx.validator_exit.clone());
                // Unlike the other votor threads, consensus pool starts even before Alpenglow is enabled
                // because it must track the genesis vote.
                let mut consensus_pool = ctx.new_consensus_pool();
                let mut stats = ConsensusPoolServiceStats::new();
                if let Err(channel_name) =
                    Self::main_loop(&mut ctx, &mut consensus_pool, &mut stats)
                {
                    info!(
                        "{}: {channel_name} disconnected. Exiting",
                        ctx.cluster_info.id()
                    );
                }
                consensus_pool.do_report();
                stats.do_report();
                info!("consensus pool service exited.");
            })
            .unwrap();

        Self {
            t_consensus_pool_service,
        }
    }

    fn handle_new_finalized(
        ctx: &ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        new_finalized_slot: Option<Slot>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
    ) {
        // If we have a new finalized slot, update the root and send new certificates
        if new_finalized_slot.is_some() {
            // Reset standstill timer
            *standstill_timer = Instant::now();
            stats.new_finalized_slot += 1;

            *ctx.highest_finalized.write().unwrap() =
                consensus_pool.get_highest_finalization_certs();
        }
        let bank = ctx.sharable_banks.root();
        consensus_pool.maybe_prune(bank.slot());
        stats.prune_old_state_called += 1;
    }

    fn send_certs(
        ctx: &mut ConsensusPoolContext,
        certificates: Vec<Arc<Certificate>>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), &'static str> {
        let num_certs = certificates.len();
        let op = BLSOp::PushCertificates { certificates };
        Self::enqueue_certificates(ctx, op, num_certs, stats)
    }

    fn refresh_certs(
        ctx: &mut ConsensusPoolContext,
        certificates: Vec<Arc<Certificate>>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), &'static str> {
        let num_certs = certificates.len();
        let op = BLSOp::RefreshCertificates { certificates };
        Self::enqueue_certificates(ctx, op, num_certs, stats)
    }

    fn enqueue_certificates(
        ctx: &mut ConsensusPoolContext,
        op: BLSOp,
        num_certs: usize,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), &'static str> {
        if num_certs == 0 {
            return Ok(());
        }
        // If we are not a staked identity (hot spare / RPC / new validator / failed VAT)
        // we should not send out the certificate. A2A quic only accepts connections
        // from staked identities
        if !ctx
            .staked_status
            .is_staked(&ctx.sharable_banks.root(), &ctx.cluster_info)
        {
            stats.certificates_skipped_unstaked += num_certs;
            return Ok(());
        }
        let channel_name = "bls_sender";
        match ctx.bls_sender.try_send(op) {
            Ok(()) => {
                stats.certificates_sent += num_certs;
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                stats.certificates_dropped += num_certs;
                let my_pubkey = ctx.cluster_info.id();
                warn!("{my_pubkey}: channel \"{channel_name}\" is full, dropping msg");
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(channel_name),
        }
    }

    // Main loop for the consensus pool service
    fn main_loop(
        ctx: &mut ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        stats: &mut ConsensusPoolServiceStats,
    ) -> Result<(), &'static str> {
        let mut events = vec![];
        let root_bank = ctx.sharable_banks.root();

        info!("{}: Certificate pool loop starting", ctx.cluster_info.id());
        let mut highest_parent_ready = root_bank.slot();

        // Standstill tracking
        let mut standstill_timer = Instant::now();

        // Kick off parent ready
        let mut kick_off_parent_ready = false;

        // Track pending safe-to-notar blocks for intrawindow slots.
        let mut pending_safe_to_notar = HashSet::new();

        // Ingest votes into consensus pool and notify voting loop of new events
        while !ctx.exit.load(Ordering::Relaxed) {
            // Kick off parent ready event, this either happens:
            // - When we first migrate to alpenglow from TowerBFT - kick off with genesis block
            // - If we startup post alpenglow migration - kick off with root block
            // - If restored vote history is farther ahead, resume from its highest parent-ready
            if !kick_off_parent_ready && ctx.migration_status.is_alpenglow_enabled() {
                let (slot, parent_block) = ctx.initial_parent_ready();
                events.push(VotorEvent::ParentReady { slot, parent_block });
                kick_off_parent_ready = true;
                // Intentionally do not increment `highest_parent_ready` in case
                // this is a cluster restart and we are the very first leader.
            }

            Self::add_produce_block_event(
                &mut highest_parent_ready,
                consensus_pool,
                ctx,
                &mut events,
                stats,
            );

            if standstill_timer.elapsed() > DELTA_STANDSTILL {
                // No reason to pollute channel with Standstill before the
                // migration is complete. We still need standstill to refresh the
                // Genesis cert though.
                if kick_off_parent_ready {
                    events.push(VotorEvent::Standstill(
                        consensus_pool
                            .highest_finalized_slot()
                            .map(|s| s.slot())
                            .unwrap_or(ctx.sharable_banks.root().slot()),
                    ));
                }
                stats.standstill = true;
                standstill_timer = Instant::now();
                Self::refresh_certs(ctx, consensus_pool.get_certs_for_standstill(), stats)?;
            }

            // Process pending safe-to-notar blocks for intrawindow slots
            Self::process_pending_safe_to_notar(
                ctx,
                consensus_pool,
                &mut pending_safe_to_notar,
                &mut events,
                stats,
            )?;
            let my_pubkey = ctx.cluster_info.id();
            for event in events.drain(..) {
                blocking_send(&my_pubkey, &ctx.event_sender, event, "event_sender")?;
            }

            let wait_timeout = if pending_safe_to_notar.is_empty() {
                Duration::from_secs(1)
            } else {
                // If there are pending blocks that are waiting for repair in order to emit
                // SafeToNotar events, use a shorter timeout
                Duration::from_millis(20)
            };

            Self::receive_msgs(
                ctx,
                consensus_pool,
                &mut events,
                &mut standstill_timer,
                stats,
                wait_timeout,
            )?;
            stats.maybe_report();
            consensus_pool.maybe_report();
        }
        Ok(())
    }

    /// Adds a vote to the consensus pool.
    ///
    /// If a new finalization slot was recognized, returns the slot
    fn add_pool_msg(
        root_bank: &Bank,
        my_pubkey: &Pubkey,
        msg: PoolMessage,
        consensus_pool: &mut ConsensusPool,
        votor_events: &mut Vec<VotorEvent>,
        stats: &mut ConsensusPoolServiceStats,
    ) -> (Option<Slot>, Vec<Arc<Certificate>>) {
        let (new_finalized_slot, new_certificates_to_send) =
            consensus_pool.add_pool_msg(root_bank, msg, votor_events);
        let Some(new_finalized_slot) = new_finalized_slot else {
            return (None, new_certificates_to_send);
        };
        trace!("{my_pubkey}: new finalization certificate for {new_finalized_slot}");
        // RPC-facing finalized commitment is updated after votor selects a root.
        stats.standstill = false;
        (Some(new_finalized_slot), new_certificates_to_send)
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
    ) -> Result<(), &'static str> {
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
                Err(TrySendError::Disconnected(_)) => return Err("repair_event_sender"),
            }
        }

        let highest_finalized = consensus_pool
            .highest_finalized_slot()
            .map(|s| s.slot())
            .unwrap_or(0);

        pending_safe_to_notar.retain(|&block| {
            // Discard if slot is at or below highest finalized
            if block.slot <= highest_finalized {
                return false;
            }

            // Check if we've received the full block in blockstore
            let Some((slot_meta, _location)) = ctx
                .blockstore
                .get_slot_meta_for_block_id(block.slot, block.block_id)
                .expect("Blockstore operations must succeed")
            else {
                // Block not yet received, keep waiting
                return true;
            };

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
        self.t_consensus_pool_service.join()
    }

    fn receive_own_votes(
        ctx: &mut ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
        msg: Result<VoteMessage, RecvError>,
    ) -> Result<(), &'static str> {
        let Ok(first) = msg else {
            return Err("own_vote_receiver");
        };

        let mut own_votes_received = 0u64;
        let mut finalized_slot = None;
        let mut certs_to_send = vec![];
        for msg in std::iter::once(first).chain(
            ctx.own_votes_receiver
                .try_iter()
                .take(ADDITIONAL_MESSAGES_PER_RECEIVE as usize),
        ) {
            own_votes_received = own_votes_received.saturating_add(1);
            let pool_msg = PoolMessage::Votes(vec![PoolVote::Own(msg)]);
            let root_bank = ctx.sharable_banks.root();
            let (new_finalized_slot, mut new_certs_to_send) = Self::add_pool_msg(
                &root_bank,
                &ctx.cluster_info.id(),
                pool_msg,
                consensus_pool,
                events,
                stats,
            );
            certs_to_send.append(&mut new_certs_to_send);
            if new_finalized_slot.is_some() {
                finalized_slot = new_finalized_slot;
            }
        }
        Self::handle_new_finalized(ctx, consensus_pool, finalized_slot, standstill_timer, stats);
        Self::send_certs(ctx, certs_to_send, stats)?;
        stats.own_votes_received += own_votes_received;
        if own_votes_received >= MAX_MESSAGES_PER_RECEIVE {
            stats.own_message_receive_limit_reached += 1;
        }
        Ok(())
    }

    fn receive_footer_certs(
        ctx: &mut ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
        msg: Result<SmallVec<[Certificate; 2]>, RecvError>,
    ) -> Result<(), &'static str> {
        let Ok(first) = msg else {
            return Err("footer_certs_receiver");
        };

        let mut footer_certs_received = 0u64;
        let mut finalized_slot = None;
        let mut certs_to_send = vec![];
        for certs in std::iter::once(first).chain(
            ctx.footer_certs_receiver
                .try_iter()
                .take(ADDITIONAL_MESSAGES_PER_RECEIVE as usize),
        ) {
            footer_certs_received = footer_certs_received.saturating_add(1);
            let pool_msg = PoolMessage::Certificates(certs.to_vec());
            let root_bank = ctx.sharable_banks.root();
            let (new_finalized_slot, mut new_certs_to_send) = Self::add_pool_msg(
                &root_bank,
                &ctx.cluster_info.id(),
                pool_msg,
                consensus_pool,
                events,
                stats,
            );
            certs_to_send.append(&mut new_certs_to_send);
            if new_finalized_slot.is_some() {
                finalized_slot = new_finalized_slot;
            }
        }
        Self::handle_new_finalized(ctx, consensus_pool, finalized_slot, standstill_timer, stats);
        Self::send_certs(ctx, certs_to_send, stats)?;
        stats.footer_certs_received += footer_certs_received;
        if footer_certs_received >= MAX_MESSAGES_PER_RECEIVE {
            stats.own_message_receive_limit_reached += 1;
        }
        Ok(())
    }

    fn receive_consensus_msgs(
        ctx: &mut ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
        msg: Result<SigVerifiedBatch, RecvError>,
    ) -> Result<(), &'static str> {
        let Ok(first) = msg else {
            return Err("consensus_message_receiver");
        };

        let mut msgs_received = 0u64;
        let mut finalized_slot = None;
        let mut certs_to_send = vec![];
        for batch in std::iter::once(first).chain(ctx.consensus_message_receiver.try_iter()) {
            let msg = match batch {
                SigVerifiedBatch::Votes(votes) => {
                    stats.vote_aggregates_received += votes.len() as u64;
                    msgs_received = msgs_received.saturating_add(votes.len() as u64);
                    PoolMessage::Votes(votes.into_iter().map(PoolVote::External).collect())
                }
                SigVerifiedBatch::Certificates(certs) => {
                    stats.certs_received += certs.len() as u64;
                    msgs_received = msgs_received.saturating_add(certs.len() as u64);
                    PoolMessage::Certificates(certs)
                }
            };
            let root_bank = ctx.sharable_banks.root();
            let (new_finalized_slot, mut new_certs_to_send) = Self::add_pool_msg(
                &root_bank,
                &ctx.cluster_info.id(),
                msg,
                consensus_pool,
                events,
                stats,
            );
            certs_to_send.append(&mut new_certs_to_send);
            if new_finalized_slot.is_some() {
                finalized_slot = new_finalized_slot;
            }
            if msgs_received >= MAX_MESSAGES_PER_RECEIVE {
                stats.consensus_message_batch_receive_limit_reached += 1;
                break;
            }
        }
        Self::handle_new_finalized(ctx, consensus_pool, finalized_slot, standstill_timer, stats);
        Self::send_certs(ctx, certs_to_send, stats)?;
        Ok(())
    }

    fn receive_msgs(
        ctx: &mut ConsensusPoolContext,
        consensus_pool: &mut ConsensusPool,
        events: &mut Vec<VotorEvent>,
        standstill_timer: &mut Instant,
        stats: &mut ConsensusPoolServiceStats,
        wait_timeout: Duration,
    ) -> Result<(), &'static str> {
        select_biased! {
            recv(ctx.own_votes_receiver) -> msg => {
                Self::receive_own_votes(ctx, consensus_pool, events, standstill_timer, stats, msg)
            }
            recv(ctx.consensus_message_receiver) -> msg => {
                Self::receive_consensus_msgs(ctx, consensus_pool, events, standstill_timer, stats, msg)
            }
            recv(ctx.footer_certs_receiver) -> msg => {
                Self::receive_footer_certs(ctx, consensus_pool, events, standstill_timer, stats,  msg)
            }
            default(wait_timeout) => Ok(()),
        }
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::tests::{get_cluster_info, new_vote_aggregate},
        agave_votor_messages::{
            certificate::CertificateType,
            consensus_message::{BLS_KEYPAIR_DERIVE_SEED, VoteMessage},
            vote::Vote,
            wire::get_vote_payload_to_sign,
        },
        crossbeam_channel::{bounded, unbounded},
        smallvec::smallvec,
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, keypair::Keypair as BLSKeypair,
            signature::Signature as BLSSignature,
        },
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::get_tmp_ledger_path_auto_delete,
        solana_runtime::{
            bank_forks::BankForks,
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        },
        std::sync::Arc,
    };

    pub struct TestContext {
        pub consensus_pool: ConsensusPool,
        pub ctx: ConsensusPoolContext,
        pub bls_receiver: Receiver<BLSOp>,
        pub consensus_message_sender: Sender<SigVerifiedBatch>,
        pub own_votes_sender: Sender<VoteMessage>,
        pub footer_certs_sender: Sender<SmallVec<[Certificate; 2]>>,
        pub event_receiver: Receiver<VotorEvent>,
        pub _repair_event_receiver: Receiver<RepairEvent>,
        pub validator_keypairs: Vec<ValidatorVoteKeypairs>,
    }

    impl Default for TestContext {
        fn default() -> Self {
            let (bls_sender, bls_receiver) = bounded(1024);
            // Create 10 node validatorvotekeypairs vec
            let validator_keypairs = (0..10)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            // Make stake monotonic decreasing so rank is deterministic
            let stake = (0..validator_keypairs.len())
                .rev()
                .map(|i| (i.saturating_add(5).saturating_mul(100)) as u64)
                .collect::<Vec<_>>();
            let genesis =
                create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, stake);
            let my_keypair = validator_keypairs[0].node_keypair.insecure_clone();
            let bank0 = Bank::new_for_tests(&genesis.genesis_config);
            let bank_forks = BankForks::new_rw_arc(bank0);

            let ledger_path = get_tmp_ledger_path_auto_delete!();
            let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            let leader_schedule_cache =
                Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

            let cluster_info = get_cluster_info(my_keypair.insecure_clone());
            let generated_cert_types = Arc::new(GeneratedCertTypes::default());
            let migration_status = Arc::new(MigrationStatus::post_migration_status());
            let (consensus_message_sender, consensus_message_receiver) = unbounded();
            let (own_votes_sender, own_votes_receiver) = unbounded();
            let (footer_certs_sender, footer_certs_receiver) = unbounded();
            let (event_sender, event_receiver) = unbounded();
            let (repair_event_sender, repair_event_receiver) = unbounded();

            let root_bank = sharable_banks.root();
            let ctx = ConsensusPoolContext {
                exit: Arc::new(AtomicBool::new(false)),
                validator_exit: Arc::default(),
                migration_status,
                generated_cert_types,
                cluster_info: cluster_info.clone(),
                blockstore,
                sharable_banks,
                leader_schedule_cache,
                vote_history_highest_parent_ready: None,
                consensus_message_receiver,
                own_votes_receiver,
                footer_certs_receiver,
                bls_sender,
                event_sender,
                repair_event_sender,
                highest_finalized: Arc::new(RwLock::new(None)),
                staked_status: StakedStatus::new(&root_bank, &cluster_info),
            };
            let consensus_pool = ctx.new_consensus_pool();

            TestContext {
                consensus_pool,
                ctx,
                bls_receiver,
                consensus_message_sender,
                own_votes_sender,
                footer_certs_sender,
                event_receiver,
                _repair_event_receiver: repair_event_receiver,
                validator_keypairs,
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
        let root_bank = ctx.ctx.sharable_banks.root();
        let rank_map = root_bank.get_rank_map(notarize_vote.slot()).unwrap();

        // Process votes from validators 0-7
        for my_rank in 0..8 {
            let vote_keypair = &ctx.validator_keypairs[my_rank].vote_keypair;
            let bls_keypair =
                BLSKeypair::derive_from_signer(vote_keypair, BLS_KEYPAIR_DERIVE_SEED).unwrap();
            let vote_serialized =
                get_vote_payload_to_sign(notarize_vote, ctx.ctx.cluster_info.my_shred_version());
            let stake = rank_map.get_pubkey_stake_entry(my_rank).unwrap().stake;
            let vote_msg = VoteMessage {
                vote: notarize_vote,
                signature: bls_keypair.sign(&vote_serialized).into(),
                rank: my_rank as u16,
                stake,
            };
            let pool_msg = if my_rank == 0 {
                PoolMessage::Votes(vec![PoolVote::Own(vote_msg)])
            } else {
                PoolMessage::Votes(vec![PoolVote::External(new_vote_aggregate(
                    &root_bank, vote_msg,
                ))])
            };

            let mut stats = ConsensusPoolServiceStats::new();
            let (new_finalized_slot, new_certificates_to_send) = ConsensusPoolService::add_pool_msg(
                &root_bank,
                &ctx.ctx.cluster_info.id(),
                pool_msg,
                &mut ctx.consensus_pool,
                &mut events,
                &mut stats,
            );

            let mut standstill_timer = Instant::now();

            // Send certificates if any were produced
            if !new_certificates_to_send.is_empty() || new_finalized_slot.is_some() {
                ConsensusPoolService::handle_new_finalized(
                    &ctx.ctx,
                    &mut ctx.consensus_pool,
                    new_finalized_slot,
                    &mut standstill_timer,
                    &mut stats,
                );
                ConsensusPoolService::send_certs(
                    &mut ctx.ctx,
                    new_certificates_to_send,
                    &mut stats,
                )
                .unwrap();
            }
        }

        // Verify that we received certificates via the bls channel
        let BLSOp::PushCertificates { certificates } = ctx.bls_receiver.recv().unwrap() else {
            panic!("invalid type");
        };
        // A Notarize certificate is stronger than a NotarizeFallback certificate,
        // so the pool only emits the former when both thresholds are reached by
        // the same aggregate.
        assert_eq!(certificates.len(), 1);
        assert_eq!(certificates[0].cert_type.slot(), target_slot);
        assert!(matches!(
            certificates[0].cert_type,
            CertificateType::Notarize(_)
        ));

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
        let (new_finalized_slot, new_certificates_to_send) = ConsensusPoolService::add_pool_msg(
            &root_bank,
            &ctx.ctx.cluster_info.id(),
            PoolMessage::Certificates(vec![skip_certificate]),
            &mut ctx.consensus_pool,
            &mut events,
            &mut stats,
        );

        let mut standstill_timer = Instant::now();

        ConsensusPoolService::handle_new_finalized(
            &ctx.ctx,
            &mut ctx.consensus_pool,
            new_finalized_slot,
            &mut standstill_timer,
            &mut stats,
        );
        ConsensusPoolService::send_certs(&mut ctx.ctx, new_certificates_to_send, &mut stats)
            .unwrap();

        // Verify skip certificate was forwarded
        let mut found_skip = false;
        while let Ok(event) = ctx.bls_receiver.try_recv() {
            if let BLSOp::PushCertificates { certificates } = event {
                assert_eq!(certificates.len(), 1);
                if matches!(certificates[0].cert_type, CertificateType::Skip(slot) if slot == target_slot)
                {
                    found_skip = true;
                }
            }
        }
        assert!(found_skip, "Should have received the skip certificate");
    }

    #[test]
    fn test_receive_own_votes_limits_messages_per_call() {
        let mut ctx = TestContext::default();
        let vote = Vote::new_unique_notar(1);
        let root_bank = ctx.ctx.sharable_banks.root();
        let rank_map = root_bank.get_rank_map(vote.slot()).unwrap();
        let stake = rank_map.get_pubkey_stake_entry(0).unwrap().stake;
        let bls_keypair = BLSKeypair::derive_from_signer(
            &ctx.validator_keypairs[0].vote_keypair,
            BLS_KEYPAIR_DERIVE_SEED,
        )
        .unwrap();
        let vote_message = VoteMessage {
            vote,
            signature: bls_keypair
                .sign(&get_vote_payload_to_sign(
                    vote,
                    ctx.ctx.cluster_info.my_shred_version(),
                ))
                .into(),
            rank: 0,
            stake,
        };

        for _ in 0..MAX_MESSAGES_PER_RECEIVE + 1 {
            ctx.own_votes_sender.send(vote_message.clone()).unwrap();
        }

        let mut events = vec![];
        let mut standstill_timer = Instant::now();
        let mut stats = ConsensusPoolServiceStats::new();
        let msg = ctx.ctx.own_votes_receiver.recv();

        ConsensusPoolService::receive_own_votes(
            &mut ctx.ctx,
            &mut ctx.consensus_pool,
            &mut events,
            &mut standstill_timer,
            &mut stats,
            msg,
        )
        .unwrap();

        assert_eq!(ctx.ctx.own_votes_receiver.len(), 1);
        assert_eq!(stats.own_votes_received.0, MAX_MESSAGES_PER_RECEIVE);
        assert_eq!(stats.own_message_receive_limit_reached.0, 1);
    }

    #[test]
    fn test_receive_consensus_msgs_limits_messages_per_call() {
        let mut ctx = TestContext::default();

        for slot in 0..MAX_MESSAGES_PER_RECEIVE + 1 {
            ctx.consensus_message_sender
                .send(SigVerifiedBatch::Certificates(vec![Certificate {
                    cert_type: CertificateType::Skip(slot),
                    signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                    bitmap: vec![],
                }]))
                .unwrap();
        }

        let mut events = vec![];
        let mut standstill_timer = Instant::now();
        let mut stats = ConsensusPoolServiceStats::new();
        let msg = ctx.ctx.consensus_message_receiver.recv();

        ConsensusPoolService::receive_consensus_msgs(
            &mut ctx.ctx,
            &mut ctx.consensus_pool,
            &mut events,
            &mut standstill_timer,
            &mut stats,
            msg,
        )
        .unwrap();

        assert_eq!(ctx.ctx.consensus_message_receiver.len(), 1);
        assert_eq!(stats.certs_received.0, MAX_MESSAGES_PER_RECEIVE);
        assert_eq!(stats.consensus_message_batch_receive_limit_reached.0, 1);
    }

    #[test]
    fn test_receive_consensus_msgs_stops_after_crossing_message_limit() {
        for batch_sizes in [
            vec![MAX_MESSAGES_PER_RECEIVE + 1, 1],
            vec![MAX_MESSAGES_PER_RECEIVE - 28, 29, 1],
        ] {
            let mut ctx = TestContext::default();
            let mut slot = 0;
            for batch_size in batch_sizes {
                let certs = (slot..slot + batch_size)
                    .map(|slot| Certificate {
                        cert_type: CertificateType::Skip(slot),
                        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                        bitmap: vec![],
                    })
                    .collect();
                ctx.consensus_message_sender
                    .send(SigVerifiedBatch::Certificates(certs))
                    .unwrap();
                slot += batch_size;
            }

            let mut events = vec![];
            let mut standstill_timer = Instant::now();
            let mut stats = ConsensusPoolServiceStats::new();
            let msg = ctx.ctx.consensus_message_receiver.recv();

            ConsensusPoolService::receive_consensus_msgs(
                &mut ctx.ctx,
                &mut ctx.consensus_pool,
                &mut events,
                &mut standstill_timer,
                &mut stats,
                msg,
            )
            .unwrap();

            assert_eq!(ctx.ctx.consensus_message_receiver.len(), 1);
            assert_eq!(stats.certs_received.0, MAX_MESSAGES_PER_RECEIVE + 1);
            assert_eq!(stats.consensus_message_batch_receive_limit_reached.0, 1);
        }
    }

    #[test]
    fn test_receive_footer_certs_limits_messages_per_call() {
        let mut ctx = TestContext::default();

        for slot in 0..MAX_MESSAGES_PER_RECEIVE + 1 {
            ctx.footer_certs_sender
                .send(smallvec![Certificate {
                    cert_type: CertificateType::Skip(slot),
                    signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                    bitmap: vec![],
                }])
                .unwrap();
        }

        let mut events = vec![];
        let mut standstill_timer = Instant::now();
        let mut stats = ConsensusPoolServiceStats::new();

        let msg = ctx.ctx.footer_certs_receiver.recv();

        ConsensusPoolService::receive_footer_certs(
            &mut ctx.ctx,
            &mut ctx.consensus_pool,
            &mut events,
            &mut standstill_timer,
            &mut stats,
            msg,
        )
        .unwrap();

        assert_eq!(ctx.ctx.footer_certs_receiver.len(), 1);
        assert_eq!(stats.footer_certs_received.0, MAX_MESSAGES_PER_RECEIVE);
        assert_eq!(stats.own_message_receive_limit_reached.0, 1);
    }

    #[test]
    fn test_send_produce_block_event() {
        let mut ctx = TestContext::default();

        // Find when is the next leader slot for me (validator 0)
        let next_leader_slot = ctx
            .ctx
            .leader_schedule_cache
            .next_leader_slot(
                &ctx.ctx.cluster_info.id(),
                0,
                &ctx.ctx.sharable_banks.root(),
                None,
                1000000,
            )
            .expect("Should find a leader slot");

        let root_bank = ctx.ctx.sharable_banks.root();
        let mut events = vec![];

        // Send skip certificates for all slots up to the next leader slot
        let mut stats = ConsensusPoolServiceStats::new();
        for slot in 1..next_leader_slot.0 {
            let skip_certificate = Certificate {
                cert_type: CertificateType::Skip(slot),
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            };

            ConsensusPoolService::add_pool_msg(
                &root_bank,
                &ctx.ctx.cluster_info.id(),
                PoolMessage::Certificates(vec![skip_certificate]),
                &mut ctx.consensus_pool,
                &mut events,
                &mut stats,
            );
        }

        // Now call add_produce_block_event to generate ProduceWindow event
        let mut highest_parent_ready = root_bank.slot();

        // Add a ParentReady event for the slot before our leader slot
        events.push(VotorEvent::ParentReady {
            slot: next_leader_slot.0,
            parent_block: Block::new_unique(next_leader_slot.0 - 1),
        });

        ConsensusPoolService::add_produce_block_event(
            &mut highest_parent_ready,
            &ctx.consensus_pool,
            &mut ctx.ctx,
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
        let mut ctx = TestContext::default();

        let root_bank = ctx.ctx.sharable_banks.root();
        let next_leader_slot = ctx
            .ctx
            .leader_schedule_cache
            .next_leader_slot(&ctx.ctx.cluster_info.id(), 0, &root_bank, None, 1000000)
            .expect("Should find a leader slot")
            .0;
        let restored_parent_ready = (
            next_leader_slot,
            Block::new_unique(next_leader_slot.checked_sub(1).unwrap()),
        );
        ctx.ctx.vote_history_highest_parent_ready = Some(restored_parent_ready);
        let mut consensus_pool = ctx.ctx.new_consensus_pool();
        let exit = ctx.ctx.exit.clone();

        let handle = thread::spawn(move || {
            let mut stats = ConsensusPoolServiceStats::new();
            let _ = ConsensusPoolService::main_loop(&mut ctx.ctx, &mut consensus_pool, &mut stats);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_parent_ready = false;
        let mut saw_produce_window = false;
        while Instant::now() < deadline && !saw_produce_window {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = ctx.event_receiver.recv_timeout(timeout) else {
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
        handle.join().unwrap();

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
        let genesis_block = Some(Block::new_unique(10));
        let root_block = Block::new_unique(12);
        assert_eq!(
            ConsensusPoolContext::_initial_parent_ready(genesis_block, root_block, None),
            (13, root_block)
        );

        let restored = (16, Block::new_unique(15));
        assert_eq!(
            ConsensusPoolContext::_initial_parent_ready(genesis_block, root_block, Some(restored)),
            restored
        );

        let stale = (12, Block::new_unique(11));
        assert_eq!(
            ConsensusPoolContext::_initial_parent_ready(genesis_block, root_block, Some(stale)),
            (13, root_block)
        );
    }

    #[test]
    fn test_send_certificates() {
        let mut ctx = TestContext::default();

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
        ConsensusPoolService::send_certs(&mut ctx.ctx, certificates.clone(), &mut stats).unwrap();
        assert_eq!(stats.certificates_sent.0, 2);

        // Verify certificates were received
        let cert1 = ctx.bls_receiver.try_recv().unwrap();
        let BLSOp::PushCertificates { certificates } = cert1 else {
            panic!("invalid type");
        };
        assert_eq!(certificates.len(), 2);
        assert!(matches!(
            certificates[0].cert_type,
            CertificateType::Skip(1)
        ));
        assert!(matches!(
            certificates[1].cert_type,
            CertificateType::Skip(2)
        ));
    }

    #[test]
    fn test_send_certificates_refresh() {
        let mut ctx = TestContext::default();

        let certificates = vec![Arc::new(Certificate {
            cert_type: CertificateType::Skip(1),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        })];

        let mut stats = ConsensusPoolServiceStats::new();
        ConsensusPoolService::refresh_certs(&mut ctx.ctx, certificates.clone(), &mut stats)
            .unwrap();
        assert_eq!(stats.certificates_sent.0, 1);

        let BLSOp::RefreshCertificates { certificates } = ctx.bls_receiver.try_recv().unwrap()
        else {
            panic!("invalid type");
        };
        assert_eq!(certificates.len(), 1);
        assert!(matches!(
            certificates[0].cert_type,
            CertificateType::Skip(1)
        ));
    }

    #[test]
    fn test_send_certificates_skips_unstaked_identity() {
        let mut ctx = TestContext::default();
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
        let unstaked_identity = Keypair::new();
        let cluster_info = get_cluster_info(unstaked_identity);
        ctx.ctx.cluster_info = cluster_info;
        let mut stats = ConsensusPoolServiceStats::new();
        ConsensusPoolService::send_certs(&mut ctx.ctx, certificates, &mut stats).unwrap();
        assert_eq!(stats.certificates_sent.0, 0);
        assert_eq!(stats.certificates_skipped_unstaked.0, 2);
        assert!(ctx.bls_receiver.try_recv().is_err());
    }

    #[test]
    fn test_send_certificates_channel_disconnected() {
        let mut ctx = TestContext::default();
        drop(ctx.bls_receiver); // Disconnect channel

        let certificates = vec![Arc::new(Certificate {
            cert_type: CertificateType::Skip(1),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        })];

        let mut stats = ConsensusPoolServiceStats::new();
        let result = ConsensusPoolService::send_certs(&mut ctx.ctx, certificates, &mut stats);
        result.unwrap_err();
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
        ConsensusPoolService::handle_new_finalized(
            &ctx.ctx,
            &mut ctx.consensus_pool,
            Some(5), // new finalized slot
            &mut standstill_timer,
            &mut stats,
        );
        ConsensusPoolService::send_certs(&mut ctx.ctx, certificates, &mut stats).unwrap();

        assert_eq!(stats.new_finalized_slot.0, 1);
        assert_eq!(stats.prune_old_state_called.0, 1);
        assert_eq!(stats.certificates_sent.0, 1);

        // Verify certificate was sent
        let received = ctx.bls_receiver.try_recv();
        assert!(received.is_ok());
    }
}
