//! The `tpu` module implements the Transaction Processing Unit, a
//! multi-stage transaction processing pipeline in software.

use etcd_client::Client;
pub use solana_sdk::net::DEFAULT_TPU_COALESCE;
// allow multiple connections for NAT and any open/close overlap
#[deprecated(
    since = "2.2.0",
    note = "Use solana_streamer::quic::DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER instead"
)]
pub use solana_streamer::quic::DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER as MAX_QUIC_CONNECTIONS_PER_PEER;
use {
    crate::{
        banking_stage::BankingStage,
        banking_trace::{Channels, TracerThread},
        cluster_info_vote_listener::{
            ClusterInfoVoteListener, DuplicateConfirmedSlotsSender, GossipVerifiedVoteHashSender,
            VerifiedVoteSender, VoteTracker,
        },
        fetch_stage::FetchStage,
        forwarding_stage::spawn_with_connection_cache,
        sigverify::TransactionSigVerifier,
        sigverify_stage::SigVerifyStage,
        staked_nodes_updater_service::StakedNodesUpdaterService,
        tpu_entry_notifier::TpuEntryNotifier,
        validator::{BlockProductionMethod, GeneratorConfig, TransactionStructure},
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
    solana_poh::poh_recorder::{PohRecorder, WorkingBankEntry},
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
    tokio::runtime::Runtime as TokioRuntime,
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
}

pub struct Tpu {
    fetch_stage: FetchStage,
    sigverify_stage: SigVerifyStage,
    vote_sigverify_stage: SigVerifyStage,
    banking_stage: BankingStage,
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

/// [`ForwardingClientOption`] enum represents the available client types for TPU
/// communication:
/// * [`ConnectionCacheClient`]: Uses a shared [`ConnectionCache`] to manage
///       connections efficiently.
/// * [`TpuClientNextClient`]: Relies on the `tpu-client-next` crate and
///       requires a reference to a [`Keypair`].
pub enum ForwardingClientOption<'a> {
    ConnectionCache(Arc<ConnectionCache>),
    TpuClientNext((&'a Keypair, TokioRuntime::Handle)),
}

impl Tpu {
    #[deprecated(since = "2.3.0", note = "Use new_with_client instead.")]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
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
        let client = ForwardingClientOption::ConnectionCache(connection_cache.clone());
        Self::new_with_client(
            cluster_info,
            poh_recorder,
            entry_receiver,
            retransmit_slots_receiver,
            sockets,
            subscriptions,
            transaction_status_sender,
            entry_notification_sender,
            blockstore,
            broadcast_type,
            exit,
            shred_version,
            vote_tracker,
            bank_forks,
            verified_vote_sender,
            gossip_verified_vote_hash_sender,
            replay_vote_receiver,
            replay_vote_sender,
            bank_notification_sender,
            tpu_coalesce,
            duplicate_confirmed_slot_sender,
            client,
            turbine_quic_endpoint_sender,
            keypair,
            log_messages_bytes_limit,
            staked_nodes,
            shared_staked_nodes_overrides,
            banking_tracer_channels,
            tracer_thread_hdl,
            tpu_enable_udp,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
            prioritization_fee_cache,
            block_production_method,
            transaction_struct,
            enable_block_production_forwarding,
            _generator_config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_client(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
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
        client: ForwardingClientOption,
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

        let (forward_stage_sender, forward_stage_receiver) = bounded(1024);
        let sigverify_stage = {
            let verifier = TransactionSigVerifier::new(
                non_vote_sender,
                enable_block_production_forwarding.then(|| forward_stage_sender.clone()),
            );
            SigVerifyStage::new(packet_receiver, verifier, "solSigVerTpu", "tpu-verifier")
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

        let forwarding_stage = match client {
            ForwardingClientOption::ConnectionCache(connection_cache) => {
                spawn_with_connection_cache(
                    forward_stage_receiver,
                    connection_cache.clone(),
                    RootBankCache::new(bank_forks.clone()),
                    (cluster_info.clone(), poh_recorder.clone()),
                    DataBudget::default(),
                )
            }
            ForwardingClientOption::TpuClientNext((identity_keypair, tpu_runtime_handle)) => {
                let my_tpu_address = config
                    .cluster_info
                    .my_contact_info()
                    .tpu(Protocol::QUIC)
                    .ok_or(format!(
                        "Invalid {:?} socket address for TPU",
                        Protocol::QUIC
                    ))?;
                let client = TpuClientNextClient::new(
                    tpu_runtime_handle,
                    my_tpu_address,
                    config.send_transaction_service_config.tpu_peers.clone(),
                    leader_info,
                    config.send_transaction_service_config.leader_forward_count,
                    Some(identity_keypair),
                );
                spawn_with_connection_cache(
                    forward_stage_receiver,
                    connection_cache.clone(),
                    RootBankCache::new(bank_forks.clone()),
                    (cluster_info.clone(), poh_recorder.clone()),
                    DataBudget::default(),
                )
            }
        };

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

        (
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
            },
            vec![key_updater, forwards_key_updater, vote_streamer_key_updater],
        )
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
                    "banking tracer thread returned error after successful thread join: {:?}",
                    tracer_result
                );
            }
        }
        Ok(())
    }
}
