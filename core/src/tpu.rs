//! The `tpu` module implements the Transaction Processing Unit, a
//! multi-stage transaction processing pipeline in software.

// allow multiple connections for NAT and any open/close overlap
#[deprecated(
    since = "2.2.0",
    note = "Use solana_streamer::quic::DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER instead"
)]
pub use solana_streamer::quic::DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER as MAX_QUIC_CONNECTIONS_PER_PEER;
pub use {crate::forwarding_stage::ForwardingClientOption, solana_sdk::net::DEFAULT_TPU_COALESCE};
use {
    crate::{
        banking_stage::BankingStage,
        banking_trace::{Channels, TracerThread},
        cluster_info_vote_listener::{
            ClusterInfoVoteListener, DuplicateConfirmedSlotsSender, GossipVerifiedVoteHashSender,
            VerifiedVoteSender, VoteTracker,
        },
        fetch_stage::FetchStage,
        forwarding_stage::{spawn_forwarding_stage, ForwardAddressGetter},
        sigverify::TransactionSigVerifier,
        sigverify_stage::SigVerifyStage,
        staked_nodes_updater_service::StakedNodesUpdaterService,
        tpu_entry_notifier::TpuEntryNotifier,
        validator::{BlockProductionMethod, GeneratorConfig, TransactionStructure},
        vortexor_receiver_adapter::VortexorReceiverAdapter,
    },
    bytes::Bytes,
    crossbeam_channel::{bounded, unbounded, Receiver},
    solana_client::connection_cache::ConnectionCache,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        blockstore::Blockstore, blockstore_processor::TransactionStatusSender,
        entry_notifier_service::EntryNotifierSender,
    },
    solana_perf::data_budget::DataBudget,
    solana_poh::{
        poh_recorder::{PohRecorder, WorkingBankEntry},
        transaction_recorder::TransactionRecorder,
    },
    solana_rpc::{
        optimistically_confirmed_bank_tracker::BankNotificationSender,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks,
        prioritization_fee_cache::PrioritizationFeeCache,
        root_bank_cache::RootBankCache,
        vote_sender_types::{ReplayVoteReceiver, ReplayVoteSender},
    },
    solana_sdk::{clock::Slot, pubkey::Pubkey, quic::NotifyKeyUpdate, signature::Keypair},
    solana_streamer::{
        quic::{spawn_server_multi, QuicServerParams, SpawnServerResult},
        streamer::StakedNodes,
    },
    solana_turbine::broadcast_stage::{BroadcastStage, BroadcastStageType},
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        sync::{atomic::AtomicBool, Arc, RwLock},
        thread::{self, JoinHandle},
        time::Duration,
    },
    tokio::sync::mpsc::Sender as AsyncSender,
};

pub struct TpuSockets {
    pub transactions: Vec<UdpSocket>,
    pub transaction_forwards: Vec<UdpSocket>,
    pub vote: Vec<UdpSocket>,
    pub broadcast: Vec<UdpSocket>,
    pub transactions_quic: Vec<UdpSocket>,
    pub transactions_forwards_quic: Vec<UdpSocket>,
    pub vote_quic: Vec<UdpSocket>,
    /// Client-side socket for the forwarding votes.
    pub vote_forwards_client: UdpSocket,
    pub vortexor_receivers: Option<Vec<UdpSocket>>,
}

/// The `SigVerifier` enum is used to determine whether to use a local or remote signature verifier.
enum SigVerifier {
    Local(SigVerifyStage),
    Remote(VortexorReceiverAdapter),
}

impl SigVerifier {
    fn join(self) -> thread::Result<()> {
        match self {
            SigVerifier::Local(sig_verify_stage) => sig_verify_stage.join(),
            SigVerifier::Remote(vortexor_receiver_adapter) => vortexor_receiver_adapter.join(),
        }
    }
}

pub struct Tpu {
    fetch_stage: FetchStage,
    sig_verifier: SigVerifier,
    vote_sigverify_stage: SigVerifyStage,
    banking_stage: BankingStage,
    forwarding_stage: JoinHandle<()>,
    cluster_info_vote_listener: ClusterInfoVoteListener,
    broadcast_stage: BroadcastStage,
    tpu_quic_t: Option<thread::JoinHandle<()>>,
    tpu_forwards_quic_t: Option<thread::JoinHandle<()>>,
    tpu_entry_notifier: Option<TpuEntryNotifier>,
    staked_nodes_updater_service: StakedNodesUpdaterService,
    tracer_thread_hdl: TracerThread,
    tpu_vote_quic_t: thread::JoinHandle<()>,
}

impl Tpu {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        transaction_recorder: TransactionRecorder,
        entry_receiver: Receiver<WorkingBankEntry>,
        retransmit_slots_receiver: Receiver<Slot>,
        sockets: TpuSockets,
        subscriptions: &Arc<RpcSubscriptions>,
        transaction_status_sender: Option<TransactionStatusSender>,
        entry_notification_sender: Option<EntryNotifierSender>,
        blockstore: Arc<Blockstore>,
        broadcast_type: &BroadcastStageType,
        exit: Arc<AtomicBool>,
        shred_version: u16,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        verified_vote_sender: VerifiedVoteSender,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        replay_vote_receiver: ReplayVoteReceiver,
        replay_vote_sender: ReplayVoteSender,
        bank_notification_sender: Option<BankNotificationSender>,
        tpu_coalesce: Duration,
        duplicate_confirmed_slot_sender: DuplicateConfirmedSlotsSender,
        connection_cache: &Arc<ConnectionCache>,
        turbine_quic_endpoint_sender: AsyncSender<(SocketAddr, Bytes)>,
        keypair: &Keypair,
        log_messages_bytes_limit: Option<usize>,
        staked_nodes: &Arc<RwLock<StakedNodes>>,
        shared_staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
        banking_tracer_channels: Channels,
        tracer_thread_hdl: TracerThread,
        tpu_enable_udp: bool,
        tpu_quic_server_config: QuicServerParams,
        tpu_fwd_quic_server_config: QuicServerParams,
        vote_quic_server_config: QuicServerParams,
        prioritization_fee_cache: &Arc<PrioritizationFeeCache>,
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
        enable_block_production_forwarding: bool,
        _generator_config: Option<GeneratorConfig>, /* vestigial code for replay invalidator */
    ) -> (Self, Vec<Arc<dyn NotifyKeyUpdate + Sync + Send>>) {
        let TpuSockets {
            transactions: transactions_sockets,
            transaction_forwards: tpu_forwards_sockets,
            vote: tpu_vote_sockets,
            broadcast: broadcast_sockets,
            transactions_quic: transactions_quic_sockets,
            transactions_forwards_quic: transactions_forwards_quic_sockets,
            vote_quic: tpu_vote_quic_sockets,
            vote_forwards_client: vote_forwards_client_socket,
            vortexor_receivers,
        } = sockets;

        let (packet_sender, packet_receiver) = unbounded();
        let (vote_packet_sender, vote_packet_receiver) = unbounded();
        let (forwarded_packet_sender, forwarded_packet_receiver) = unbounded();
        let fetch_stage = FetchStage::new_with_sender(
            transactions_sockets,
            tpu_forwards_sockets,
            tpu_vote_sockets,
            exit.clone(),
            &packet_sender,
            &vote_packet_sender,
            &forwarded_packet_sender,
            forwarded_packet_receiver,
            poh_recorder,
            Some(tpu_coalesce),
            Some(bank_forks.read().unwrap().get_vote_only_mode_signal()),
            tpu_enable_udp,
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
        } = spawn_server_multi(
            "solQuicTVo",
            "quic_streamer_tpu_vote",
            tpu_vote_quic_sockets,
            keypair,
            vote_packet_sender.clone(),
            exit.clone(),
            staked_nodes.clone(),
            vote_quic_server_config,
        )
        .unwrap();

        let (tpu_quic_t, key_updater) = if vortexor_receivers.is_none() {
            // Streamer for TPU
            let SpawnServerResult {
                endpoints: _,
                thread: tpu_quic_t,
                key_updater,
            } = spawn_server_multi(
                "solQuicTpu",
                "quic_streamer_tpu",
                transactions_quic_sockets,
                keypair,
                packet_sender,
                exit.clone(),
                staked_nodes.clone(),
                tpu_quic_server_config,
            )
            .unwrap();
            (Some(tpu_quic_t), Some(key_updater))
        } else {
            (None, None)
        };

        let (tpu_forwards_quic_t, forwards_key_updater) = if vortexor_receivers.is_none() {
            // Streamer for TPU forward
            let SpawnServerResult {
                endpoints: _,
                thread: tpu_forwards_quic_t,
                key_updater: forwards_key_updater,
            } = spawn_server_multi(
                "solQuicTpuFwd",
                "quic_streamer_tpu_forwards",
                transactions_forwards_quic_sockets,
                keypair,
                forwarded_packet_sender,
                exit.clone(),
                staked_nodes.clone(),
                tpu_fwd_quic_server_config,
            )
            .unwrap();
            (Some(tpu_forwards_quic_t), Some(forwards_key_updater))
        } else {
            (None, None)
        };

        let (forward_stage_sender, forward_stage_receiver) = bounded(1024);
        let sig_verifier = if let Some(vortexor_receivers) = vortexor_receivers {
            info!("starting vortexor adapter");
            let sockets = vortexor_receivers.into_iter().map(Arc::new).collect();
            let adapter = VortexorReceiverAdapter::new(
                sockets,
                Duration::from_millis(5),
                tpu_coalesce,
                non_vote_sender,
                enable_block_production_forwarding.then(|| forward_stage_sender.clone()),
                exit.clone(),
            );
            SigVerifier::Remote(adapter)
        } else {
            info!("starting regular sigverify stage");
            let verifier = TransactionSigVerifier::new(
                non_vote_sender,
                enable_block_production_forwarding.then(|| forward_stage_sender.clone()),
            );
            SigVerifier::Local(SigVerifyStage::new(
                packet_receiver,
                verifier,
                "solSigVerTpu",
                "tpu-verifier",
            ))
        };

        let vote_sigverify_stage = {
            let verifier = TransactionSigVerifier::new_reject_non_vote(
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
            gossip_vote_sender,
            vote_tracker,
            bank_forks.clone(),
            subscriptions.clone(),
            verified_vote_sender,
            gossip_verified_vote_hash_sender,
            replay_vote_receiver,
            blockstore.clone(),
            bank_notification_sender,
            duplicate_confirmed_slot_sender,
        );

        let banking_stage = BankingStage::new(
            block_production_method,
            transaction_struct,
            cluster_info,
            poh_recorder,
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            transaction_status_sender,
            replay_vote_sender,
            log_messages_bytes_limit,
            bank_forks.clone(),
            prioritization_fee_cache,
        );

        let client = ForwardingClientOption::ConnectionCache(connection_cache.clone());

        let forwarding_stage = spawn_forwarding_stage(
            forward_stage_receiver,
            client,
            vote_forwards_client_socket,
            RootBankCache::new(bank_forks.clone()),
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
            turbine_quic_endpoint_sender,
        );

        let mut key_updaters: Vec<Arc<dyn NotifyKeyUpdate + Send + Sync>> = Vec::new();
        if let Some(key_updater) = key_updater {
            key_updaters.push(key_updater);
        }
        if let Some(forwards_key_updater) = forwards_key_updater {
            key_updaters.push(forwards_key_updater);
        }
        key_updaters.push(vote_streamer_key_updater);
        (
            Self {
                fetch_stage,
                sig_verifier,
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
            },
            key_updaters,
        )
    }

    pub fn join(self) -> thread::Result<()> {
        let results = vec![
            self.fetch_stage.join(),
            self.sig_verifier.join(),
            self.vote_sigverify_stage.join(),
            self.cluster_info_vote_listener.join(),
            self.banking_stage.join(),
            self.forwarding_stage.join(),
            self.staked_nodes_updater_service.join(),
            self.tpu_quic_t.map_or(Ok(()), |t| t.join()),
            self.tpu_forwards_quic_t.map_or(Ok(()), |t| t.join()),
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
                    "banking tracer thread returned error after successful thread join: {:?}",
                    tracer_result
                );
            }
        }
        Ok(())
    }
}
