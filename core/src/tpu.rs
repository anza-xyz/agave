//! The `tpu` module implements the Transaction Processing Unit, a
//! multi-stage transaction processing pipeline in software.

use {
    crate::{
        admin_rpc_post_init::{KeyUpdaterType, KeyUpdaters},
        banking_stage::{
            BankingControlMsg, BankingStage, BankingStageHandle,
            transaction_scheduler::scheduler_controller::SchedulerConfig,
        },
        banking_trace::{Channels, TracerThread},
        cluster_info_vote_listener::{
            ClusterInfoVoteListener, DuplicateConfirmedSlotsSender, GossipVerifiedVoteHashSender,
            VerifiedVoterSlotsSender, VoteTracker,
        },
        fetch_stage::FetchStage,
        forwarding_stage::{
            ForwardAddressGetter, ForwardingClientConfig, SpawnForwardingStageResult,
            spawn_forwarding_stage,
        },
        sigverify::TransactionSigVerifier,
        sigverify_stage::SigVerifyStage,
        staked_nodes_updater_service::StakedNodesUpdaterService,
        tpu_entry_notifier::TpuEntryNotifier,
        validator::{BlockProductionMethod, GeneratorConfig},
    },
    agave_votor::event::VotorEventSender,
    crossbeam_channel::{Receiver, bounded, unbounded},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore, blockstore_processor::TransactionStatusSender,
        entry_notifier_service::EntryNotifierSender,
    },
    solana_perf::data_budget::DataBudget,
    solana_poh::{
        poh_recorder::{PohRecorder, WorkingBankEntry},
        transaction_recorder::TransactionRecorder,
    },
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks,
        prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::{ReplayVoteReceiver, ReplayVoteSender},
    },
    solana_streamer::{
        quic::{
            SimpleQosQuicStreamerConfig, SpawnServerResult, SwQosQuicStreamerConfig,
            spawn_simple_qos_server, spawn_stake_wighted_qos_server,
        },
        streamer::StakedNodes,
    },
    solana_turbine::{
        broadcast_stage::{BroadcastStage, BroadcastStageType},
        xdp::XdpSender,
    },
    std::{
        collections::HashMap,
        net::UdpSocket,
        num::NonZeroUsize,
        path::PathBuf,
        sync::{Arc, RwLock, atomic::AtomicBool},
        thread::{self, JoinHandle},
    },
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
};

pub struct TpuSockets {
    pub vote: Vec<UdpSocket>,
    pub broadcast: Vec<UdpSocket>,
    pub transactions_quic: Vec<UdpSocket>,
    pub transactions_forwards_quic: Vec<UdpSocket>,
    pub vote_quic: Vec<UdpSocket>,
    /// Client-side socket for the forwarding votes.
    pub vote_forwarding_client: UdpSocket,
}

// Conservatively allow 20 TPS per validator.
pub const MAX_VOTES_PER_SECOND: u64 = 20;

/// Size of the channel between streamer and TPU sigverify stage. The values have been selected to
/// be conservative max of obsersed on mnb during high-load events.
const TPU_CHANNEL_SIZE: usize = 50_000;

pub struct Tpu {
    fetch_stage: FetchStage,
    sigverify_stage: SigVerifyStage,
    vote_sigverify_stage: SigVerifyStage,
    banking_stage: BankingStageHandle,
    forwarding_stage: JoinHandle<()>,
    cluster_info_vote_listener: ClusterInfoVoteListener,
    broadcast_stage: BroadcastStage,
    tpu_quic_t: thread::JoinHandle<()>,
    tpu_forwards_quic_t: thread::JoinHandle<()>,
    tpu_entry_notifier: Option<TpuEntryNotifier>,
    staked_nodes_updater_service: StakedNodesUpdaterService,
    tracer_thread_hdl: TracerThread,
    tpu_vote_quic_t: thread::JoinHandle<()>,
}

impl Tpu {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_client(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        transaction_recorder: TransactionRecorder,
        entry_receiver: Receiver<WorkingBankEntry>,
        retransmit_slots_receiver: Receiver<Slot>,
        sockets: TpuSockets,
        subscriptions: Option<Arc<RpcSubscriptions>>,
        transaction_status_sender: Option<TransactionStatusSender>,
        entry_notification_sender: Option<EntryNotifierSender>,
        blockstore: Arc<Blockstore>,
        broadcast_type: &BroadcastStageType,
        xdp_sender: Option<XdpSender>,
        exit: Arc<AtomicBool>,
        shred_version: u16,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        verified_voter_slots_sender: VerifiedVoterSlotsSender,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        replay_vote_receiver: ReplayVoteReceiver,
        replay_vote_sender: ReplayVoteSender,
        bank_notification_sender: Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: DuplicateConfirmedSlotsSender,
        tpu_forwaring_client_config: ForwardingClientConfig,
        keypair: &Keypair,
        log_messages_bytes_limit: Option<usize>,
        staked_nodes: &Arc<RwLock<StakedNodes>>,
        shared_staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
        banking_tracer_channels: Channels,
        tracer_thread_hdl: TracerThread,
        tpu_quic_server_config: SwQosQuicStreamerConfig,
        tpu_fwd_quic_server_config: SwQosQuicStreamerConfig,
        vote_quic_server_config: SimpleQosQuicStreamerConfig,
        prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
        tpu_sigverify_threads: NonZeroUsize,
        block_production_method: BlockProductionMethod,
        block_production_num_workers: NonZeroUsize,
        block_production_scheduler_config: SchedulerConfig,
        enable_block_production_forwarding: bool,
        _generator_config: Option<GeneratorConfig>, /* vestigial code for replay invalidator */
        key_notifiers: Arc<RwLock<KeyUpdaters>>,
        banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
        scheduler_bindings: Option<(PathBuf, mpsc::Sender<BankingControlMsg>)>,
        cancel: CancellationToken,
        votor_event_sender: VotorEventSender,
    ) -> Self {
        let TpuSockets {
            vote: tpu_vote_sockets,
            broadcast: broadcast_sockets,
            transactions_quic: transactions_quic_sockets,
            transactions_forwards_quic: transactions_forwards_quic_sockets,
            vote_quic: tpu_vote_quic_sockets,
            vote_forwarding_client: vote_forwarding_client_socket,
        } = sockets;

        let (packet_sender, packet_receiver) = bounded(TPU_CHANNEL_SIZE);
        let (vote_packet_sender, vote_packet_receiver) = unbounded();
        let (forwarded_packet_sender, forwarded_packet_receiver) = unbounded();
        let fetch_stage = FetchStage::new_with_sender(
            tpu_vote_sockets,
            exit.clone(),
            &packet_sender,
            &vote_packet_sender,
            forwarded_packet_receiver,
            poh_recorder,
            None, // coalesce
        );

        let staked_nodes_updater_service = StakedNodesUpdaterService::new(
            exit.clone(),
            bank_forks.clone(),
            staked_nodes.clone(),
            shared_staked_nodes_overrides,
        );

        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer_channels;

        // Streamer for Votes:
        let SpawnServerResult {
            endpoints: _,
            thread: tpu_vote_quic_t,
            key_updater: vote_streamer_key_updater,
        } = spawn_simple_qos_server(
            "solQuicTVo",
            "quic_streamer_tpu_vote",
            tpu_vote_quic_sockets,
            keypair,
            vote_packet_sender,
            staked_nodes.clone(),
            vote_quic_server_config.quic_streamer_config,
            vote_quic_server_config.qos_config,
            cancel.clone(),
        )
        .unwrap();

        // Streamer for TPU
        let SpawnServerResult {
            endpoints: _,
            thread: tpu_quic_t,
            key_updater,
        } = spawn_stake_wighted_qos_server(
            "solQuicTpu",
            "quic_streamer_tpu",
            transactions_quic_sockets,
            keypair,
            packet_sender,
            staked_nodes.clone(),
            tpu_quic_server_config.quic_streamer_config,
            tpu_quic_server_config.qos_config,
            cancel.clone(),
        )
        .unwrap();

        // Streamer for TPU forward
        let SpawnServerResult {
            endpoints: _,
            thread: tpu_forwards_quic_t,
            key_updater: forwards_key_updater,
        } = spawn_stake_wighted_qos_server(
            "solQuicTpuFwd",
            "quic_streamer_tpu_forwards",
            transactions_forwards_quic_sockets,
            keypair,
            forwarded_packet_sender,
            staked_nodes.clone(),
            tpu_fwd_quic_server_config.quic_streamer_config,
            tpu_fwd_quic_server_config.qos_config,
            cancel,
        )
        .unwrap();

        let (forward_stage_sender, forward_stage_receiver) = bounded(1024);

        let sigverify_threadpool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(tpu_sigverify_threads.get())
                .thread_name(|i| format!("solSigVerify{i:02}"))
                .build()
                .expect("new rayon threadpool"),
        );

        let sigverify_stage = {
            let verifier = TransactionSigVerifier::new(
                sigverify_threadpool.clone(),
                non_vote_sender,
                enable_block_production_forwarding.then(|| forward_stage_sender.clone()),
            );
            SigVerifyStage::new(packet_receiver, verifier, "solSigVerTpu", "tpu-verifier")
        };

        let vote_sigverify_stage = {
            let verifier = TransactionSigVerifier::new_reject_non_vote(
                sigverify_threadpool.clone(),
                tpu_vote_sender,
                Some(forward_stage_sender),
            );
            SigVerifyStage::new(
                vote_packet_receiver,
                verifier,
                "solSigVerTpuVot",
                "tpu-vote-verifier",
            )
        };

        let cluster_info_vote_listener = ClusterInfoVoteListener::new(
            exit.clone(),
            cluster_info.clone(),
            sigverify_threadpool,
            gossip_vote_sender,
            vote_tracker,
            bank_forks.clone(),
            subscriptions,
            verified_voter_slots_sender,
            gossip_verified_vote_hash_sender,
            replay_vote_receiver,
            blockstore.clone(),
            bank_notification_sender,
            duplicate_confirmed_slot_sender,
        );

        let banking_stage = BankingStage::new_num_threads(
            block_production_method,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            banking_control_receiver,
            block_production_num_workers,
            block_production_scheduler_config,
            transaction_status_sender,
            replay_vote_sender,
            log_messages_bytes_limit,
            bank_forks.clone(),
            prioritization_fee_cache,
        );

        #[cfg(unix)]
        if let Some((path, banking_control_sender)) = scheduler_bindings {
            super::scheduler_bindings_server::spawn(&path, banking_control_sender);
        }
        #[cfg(not(unix))]
        assert!(scheduler_bindings.is_none());

        let SpawnForwardingStageResult {
            join_handle: forwarding_stage,
            client_updater,
        } = spawn_forwarding_stage(
            forward_stage_receiver,
            tpu_forwaring_client_config,
            vote_forwarding_client_socket,
            bank_forks.read().unwrap().sharable_banks(),
            ForwardAddressGetter::new(cluster_info.clone(), poh_recorder.clone()),
            DataBudget::default(),
        );

        let (entry_receiver, tpu_entry_notifier) =
            if let Some(entry_notification_sender) = entry_notification_sender {
                let (broadcast_entry_sender, broadcast_entry_receiver) = unbounded();
                let tpu_entry_notifier = TpuEntryNotifier::new(
                    entry_receiver,
                    entry_notification_sender,
                    broadcast_entry_sender,
                    exit.clone(),
                );
                (broadcast_entry_receiver, Some(tpu_entry_notifier))
            } else {
                (entry_receiver, None)
            };

        let broadcast_stage = broadcast_type.new_broadcast_stage(
            broadcast_sockets,
            cluster_info.clone(),
            entry_receiver,
            retransmit_slots_receiver,
            exit,
            blockstore,
            bank_forks,
            shred_version,
            xdp_sender,
            votor_event_sender,
        );

        let mut key_notifiers = key_notifiers.write().unwrap();
        key_notifiers.add(KeyUpdaterType::Tpu, key_updater);
        key_notifiers.add(KeyUpdaterType::TpuForwards, forwards_key_updater);
        key_notifiers.add(KeyUpdaterType::TpuVote, vote_streamer_key_updater);
        key_notifiers.add(KeyUpdaterType::Forward, client_updater);

        Self {
            fetch_stage,
            sigverify_stage,
            vote_sigverify_stage,
            banking_stage,
            forwarding_stage,
            cluster_info_vote_listener,
            broadcast_stage,
            tpu_quic_t,
            tpu_forwards_quic_t,
            tpu_entry_notifier,
            staked_nodes_updater_service,
            tracer_thread_hdl,
            tpu_vote_quic_t,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        let results = vec![
            self.fetch_stage.join(),
            self.sigverify_stage.join(),
            self.vote_sigverify_stage.join(),
            self.cluster_info_vote_listener.join(),
            self.banking_stage.join(),
            self.forwarding_stage.join(),
            self.staked_nodes_updater_service.join(),
            self.tpu_quic_t.join(),
            self.tpu_forwards_quic_t.join(),
            self.tpu_vote_quic_t.join(),
        ];
        let broadcast_result = self.broadcast_stage.join();
        for result in results {
            result?;
        }
        if let Some(tpu_entry_notifier) = self.tpu_entry_notifier {
            tpu_entry_notifier.join()?;
        }
        let _ = broadcast_result?;
        if let Some(tracer_thread_hdl) = self.tracer_thread_hdl {
            if let Err(tracer_result) = tracer_thread_hdl.join()? {
                error!(
                    "banking tracer thread returned error after successful thread join: \
                     {tracer_result:?}"
                );
            }
        }
        Ok(())
    }
}
