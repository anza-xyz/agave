//! The `tvu` module implements the Transaction Validation Unit, a multi-stage transaction
//! validation pipeline in software.

use {
    crate::{
        admin_rpc_post_init::{KeyUpdaterType, KeyUpdaters},
        block_creation_loop::ReplayHighestFrozen,
        commitment_service::AggregateCommitmentService,
        completed_data_sets_service::CompletedDataSetsSender,
        cost_update_service::CostUpdateService,
        drop_bank_service::DropBankService,
        repair::{
            block_id_repair_service::BlockIdRepairChannels,
            repair_service::{OutstandingShredRepairs, RepairInfo, RepairServiceChannels},
        },
        replay_stage::{ReplayReceivers, ReplaySenders, ReplayStage, ReplayStageConfig},
        shred_fetch_stage::{SHRED_FETCH_CHANNEL_SIZE, ShredFetchStage},
        window_service::{WindowService, WindowServiceChannels},
    },
    agave_bls_sigverify::{
        bls_sigverifier::{self, SigVerifierChannels, SigVerifierContext},
        generated_cert_types::GeneratedCertTypes,
        metric_types::MAX_IN_FLIGHT_CONSENSUS_EVENTS,
        pubkeys::VerifiedVotorSlotsMessage,
        rewards::RewardInput,
    },
    agave_jemalloc::jemalloc::Arena,
    agave_votor::{
        event::{LatestSwitchRequest, LeaderWindowInfo, VotorEventReceiver, VotorEventSender},
        peer_list_updater::PeerListService,
        slot_clock::SharedAlpenglowSlotClock,
        vote_history::VoteHistory,
        vote_history_storage::VoteHistoryStorage,
        voting_service::{VotingService as BLSVotingService, votor_rate_limit_pps},
        votor::{Votor, VotorConfig},
    },
    agave_votor_messages::consensus_message::Block,
    agave_votor_transport::{PeerList, endpoint::QuicDatagramEndpoint},
    arc_swap::ArcSwap,
    crossbeam_channel::{Receiver, Sender, bounded, unbounded},
    solana_clock::Slot,
    solana_geyser_plugin_manager::block_metadata_notifier_interface::BlockMetadataNotifierArc,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::{
            Blockstore, BlockstoreCleanupService, MAX_COMPLETED_SLOTS_IN_CHANNEL,
            UpdateParentReceiver,
        },
        blockstore_options::BlockstoreCleanupStrategy,
        entry_notifier_service::EntryNotifierSender,
        leader_schedule_cache::LeaderScheduleCache,
        shred::filter::TurbineMode,
    },
    solana_net_utils::{PinnedXdpSender, quic_socket::QuicSocket},
    solana_pubkey::Pubkey,
    solana_rpc::{
        max_slots::MaxSlots, optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
        rpc_subscriptions::RpcSubscriptions, slot_status_notifier::SlotStatusNotifier,
    },
    solana_runtime::{
        bank_forks::BankForks,
        bank_forks_controller::{BankForksCommandReceiver, BankForksController},
        commitment::BlockCommitmentCache,
        snapshot_controller::SnapshotController,
        transaction_execution::TransactionStatusSender,
        validated_block_finalization::ValidatedBlockFinalizationCert,
    },
    solana_streamer::evicting_sender::EvictingSender,
    solana_turbine::{XdpSender as TurbineXdpSender, retransmit_stage::RetransmitStage},
    solana_validator_exit::Exit,
    std::{
        collections::{HashMap, HashSet},
        net::{SocketAddr, UdpSocket},
        num::NonZeroUsize,
        sync::{Arc, RwLock, atomic::AtomicBool},
        thread::{self, JoinHandle},
    },
    tokio::{
        runtime::{Handle, Runtime as TokioRuntime},
        sync::watch,
    },
    tokio_util::sync::CancellationToken,
};

/// Sets the upper bound on the number of batches stored in the retransmit
/// stage ingress channel.
/// Allows for a max of 16k batches of up to 64 packets each
/// (PACKETS_PER_BATCH).
/// This translates to about 1 GB of RAM for packet storage in the worst case.
/// In reality this means about 200K shreds since most batches are not full.
const CHANNEL_SIZE_RETRANSMIT_INGRESS: usize = 16 * 1024;

/// The maximum number of alpenglow packets that can be processed in a single batch
pub(crate) const MAX_ALPENGLOW_PACKET_NUM: usize = 10_000;
/// The maximum number of distinct bls messages that can be sent in a single batch.
/// This is overprovisioned to account for standstill scenarios, where a large amount
/// of votes / certificate need to be refreshed.
const MAX_BLS_MESSAGES_TO_SEND: usize = 1000;

/// Bounds certificates recovered from blockstore and awaiting BLS verification.
const MAX_CERTIFICATES_FROM_BLOCKSTORE: usize = 1_024;

pub struct Tvu {
    fetch_stage: ShredFetchStage,
    shred_sigverify: JoinHandle<()>,
    retransmit_stage: RetransmitStage,
    window_service: WindowService,
    replay_stage: ReplayStage,
    blockstore_cleanup_service: BlockstoreCleanupService,
    cost_update_service: CostUpdateService,
    bls_voting_service: BLSVotingService,
    peer_list_service: PeerListService,
    drop_bank_service: DropBankService,
    bls_sigverifier: JoinHandle<()>,
    votor: Votor,
    commitment_service: AggregateCommitmentService,
    votor_transport_endpoint: QuicDatagramEndpoint,
    // `None` when running on an ambient runtime.
    votor_transport_runtime: Option<TokioRuntime>,
}

pub struct TvuSockets {
    pub fetch: Vec<UdpSocket>,
    pub repair: UdpSocket,
    pub retransmit: Vec<UdpSocket>,
    pub block_id_repair: UdpSocket,
}

pub struct TvuConfig {
    pub blockstore_cleanup_strategy: BlockstoreCleanupStrategy,
    pub shred_version: u16,
    // Validators from which repairs are requested
    pub repair_validators: Option<HashSet<Pubkey>>,
    // Validators which should be given priority when serving repairs
    pub repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
    pub shred_sigverify_threads: NonZeroUsize,
    pub bls_sigverify_threads: NonZeroUsize,
    pub turbine_xdp_sender: Option<TurbineXdpSender>,
    pub repair_xdp_sender: Option<PinnedXdpSender>,
    pub(crate) replay_arena: Option<Arena>,
}

impl Default for TvuConfig {
    fn default() -> Self {
        Self {
            blockstore_cleanup_strategy: BlockstoreCleanupStrategy::None,
            shred_version: 0,
            repair_validators: None,
            repair_whitelist: Arc::new(RwLock::new(HashSet::default())),
            replay_forks_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            replay_transactions_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            shred_sigverify_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            bls_sigverify_threads: NonZeroUsize::new(1).expect("1 is non-zero"),
            turbine_xdp_sender: None,
            repair_xdp_sender: None,
            replay_arena: None,
        }
    }
}

/// Shared state from validator necessary to instantiate votor and related services
pub struct AlpenglowInitializationState {
    pub alpenglow_slot_clock: SharedAlpenglowSlotClock,
    // Shared with block creation loop
    pub leader_window_info_sender: Sender<LeaderWindowInfo>,
    pub optimistic_parent_sender: Sender<LeaderWindowInfo>,
    pub optimistic_parent_receiver: Receiver<LeaderWindowInfo>,
    pub replay_highest_frozen: Arc<ReplayHighestFrozen>,
    pub highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
    pub bank_forks_controller: Arc<dyn BankForksController>,
    pub bank_forks_controller_receiver: BankForksCommandReceiver,

    // Main communication channel
    pub votor_event_sender: VotorEventSender,
    pub votor_event_receiver: VotorEventReceiver,

    pub cancel: CancellationToken,
    pub validator_exit: Arc<RwLock<Exit>>,
    pub key_notifiers: Arc<RwLock<KeyUpdaters>>,

    // server sockets for Alpenglow consensus traffic
    pub votor_server_sockets: Vec<QuicSocket>,
    // client socket for Alpenglow consensus traffic
    pub votor_client_socket: QuicSocket,
    // peers plugged into the votor peer_list regardless of stake
    pub votor_peer_overrides: Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>>,
}

impl Tvu {
    /// This service receives messages from a leader in the network and processes the transactions
    /// on the bank state.
    /// # Arguments
    /// * `cluster_info` - The cluster_info state.
    /// * `sockets` - fetch, repair, and retransmit sockets
    /// * `blockstore` - the ledger itself
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vote_account: &Pubkey,
        authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
        bank_forks: Arc<RwLock<BankForks>>,
        cluster_info: &Arc<ClusterInfo>,
        sockets: TvuSockets,
        blockstore: Arc<Blockstore>,
        ledger_signal_receiver: Receiver<bool>,
        update_parent_receiver: UpdateParentReceiver,
        rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
        vote_history: VoteHistory,
        vote_history_storage: Arc<dyn VoteHistoryStorage>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        exit: Arc<AtomicBool>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        turbine_mode: TurbineMode,
        transaction_status_sender: Option<TransactionStatusSender>,
        entry_notification_sender: Option<EntryNotifierSender>,
        verified_voter_slots_sender: EvictingSender<VerifiedVotorSlotsMessage>,
        verified_voter_slots_receiver: Receiver<VerifiedVotorSlotsMessage>,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
        bank_notification_sender: Option<BankNotificationSenderConfig>,
        tvu_config: TvuConfig,
        max_slots: &Arc<MaxSlots>,
        block_metadata_notifier: Option<BlockMetadataNotifierArc>,
        wait_to_vote_slot: Option<Slot>,
        snapshot_controller: Option<Arc<SnapshotController>>,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
        slot_status_notifier: Option<SlotStatusNotifier>,
        votor_init: AlpenglowInitializationState,
        reward_aggregates_sender: Sender<RewardInput>,
    ) -> Result<Self, String> {
        let TvuSockets {
            repair: repair_socket,
            fetch: fetch_sockets,
            retransmit: retransmit_sockets,
            block_id_repair,
        } = sockets;

        let AlpenglowInitializationState {
            alpenglow_slot_clock,
            leader_window_info_sender,
            optimistic_parent_sender,
            optimistic_parent_receiver,
            replay_highest_frozen,
            highest_parent_ready,
            bank_forks_controller,
            bank_forks_controller_receiver,
            votor_event_sender,
            votor_event_receiver,
            cancel,
            validator_exit,
            key_notifiers,
            votor_server_sockets,
            votor_client_socket,
            votor_peer_overrides,
            highest_finalized,
        } = votor_init;

        let (consensus_message_sender, consensus_message_receiver) =
            bounded(MAX_ALPENGLOW_PACKET_NUM);
        let (consensus_metrics_sender, consensus_metrics_receiver) =
            bounded(MAX_IN_FLIGHT_CONSENSUS_EVENTS);
        let generated_cert_types = Arc::new(GeneratedCertTypes::default());
        let (certificate_sender, certificate_receiver) = bounded(MAX_CERTIFICATES_FROM_BLOCKSTORE);
        blockstore.set_certificate_sender(certificate_sender);

        // Bring up the votor QUIC datagram endpoint.
        let (votor_runtime, votor_rt_handle) = match Handle::try_current() {
            Ok(handle) => (None, handle),
            Err(_) => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    // Using "3+number of sockets" workers here based on load-testing performed.
                    // 3 shared workers handle admitted connections and egress, each server socket
                    // adds 1 core for its endpoint's TLS/handshake work.
                    .worker_threads(3 + votor_server_sockets.len())
                    .thread_name("solVotorQuicRt")
                    .build()
                    .unwrap();
                let handle = rt.handle().clone();
                (Some(rt), handle)
            }
        };
        let (votor_ingress_sender, votor_ingress_receiver) = bounded(MAX_ALPENGLOW_PACKET_NUM);
        // PeerListService populates the votor_peer_list channel immediately on startup,
        // so we can initialize the watch channel with a dummy value here.
        let (votor_peer_list_sender, votor_peer_list_receiver) =
            watch::channel(Arc::new(PeerList {
                peers: HashMap::new(),
                push_enabled: false,
            }));
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let peer_list_service = PeerListService::new(
            cluster_info.clone(),
            votor_peer_list_sender,
            sharable_banks.clone(),
            votor_peer_overrides,
        );
        let (votor_egress, endpoint) = QuicDatagramEndpoint::spawn(
            &votor_rt_handle,
            &cluster_info.keypair(),
            votor_server_sockets,
            votor_client_socket,
            votor_ingress_sender,
            votor_peer_list_receiver,
            *cluster_info.socket_addr_space(),
            votor_rate_limit_pps(),
            cancel,
        )
        .map_err(|e| format!("alpenglow endpoint: {e:?}"))?;
        {
            let mut key_notifiers = key_notifiers.write().unwrap();
            key_notifiers.add(KeyUpdaterType::Votor, endpoint.key_updater());
            // Wake the peer_list refresh loop immediately on a set-identity so we
            // start connecting under the new identity without a refresh-interval delay.
            key_notifiers.add(
                KeyUpdaterType::VotorPeerListService,
                peer_list_service.key_updater(),
            );
        }
        let votor_ban_sender = endpoint.ban_sender();
        let bls_sigverifier = bls_sigverifier::spawn_service(
            exit.clone(),
            SigVerifierContext {
                ban_sender: votor_ban_sender,
                sharable_banks,
                highest_parent_ready: highest_parent_ready.clone(),
                cluster_info: cluster_info.clone(),
                leader_schedule: leader_schedule_cache.clone(),
                num_threads: tvu_config.bls_sigverify_threads.get(),
                generated_cert_types: generated_cert_types.clone(),
            },
            SigVerifierChannels::new(
                votor_ingress_receiver,
                certificate_receiver,
                verified_voter_slots_sender,
                reward_aggregates_sender.clone(),
                consensus_message_sender,
                consensus_metrics_sender.clone(),
            ),
        );

        let (fetch_sender, fetch_receiver) = EvictingSender::new_bounded(SHRED_FETCH_CHANNEL_SIZE);

        let repair_socket = Arc::new(repair_socket);
        let block_id_repair_socket = Arc::new(block_id_repair);
        let fetch_sockets: Vec<Arc<UdpSocket>> = fetch_sockets.into_iter().map(Arc::new).collect();
        let fetch_stage = ShredFetchStage::new(
            fetch_sockets,
            repair_socket.clone(),
            fetch_sender,
            tvu_config.shred_version,
            bank_forks.clone(),
            cluster_info.clone(),
            outstanding_repair_requests.clone(),
            turbine_mode,
            exit.clone(),
        );

        let (verified_sender, verified_receiver) = unbounded();

        let (retransmit_sender, retransmit_receiver) = bounded(CHANNEL_SIZE_RETRANSMIT_INGRESS);

        let shred_sigverify = solana_turbine::sigverify_shreds::spawn_shred_sigverify(
            cluster_info.clone(),
            bank_forks.clone(),
            leader_schedule_cache.clone(),
            fetch_receiver,
            retransmit_sender.clone(),
            verified_sender,
            Arc::new({
                let outstanding_repair_requests = outstanding_repair_requests.clone();
                move |nonce| {
                    outstanding_repair_requests
                        .read()
                        .unwrap()
                        .fetch_metadata_for_nonce(nonce)
                }
            }),
            tvu_config.shred_sigverify_threads,
        );

        let retransmit_stage = RetransmitStage::new(
            bank_forks.clone(),
            leader_schedule_cache.clone(),
            cluster_info.clone(),
            Arc::new(retransmit_sockets),
            retransmit_receiver,
            max_slots.clone(),
            rpc_subscriptions.clone(),
            slot_status_notifier.clone(),
            tvu_config.turbine_xdp_sender,
            votor_event_sender.clone(),
        );

        // Create repair event channel for BlockIdRepairService
        let (repair_event_sender, repair_event_receiver) = bounded(100);

        // Create completed slots channel for BlockIdRepairService
        let (completed_slots_sender, completed_slots_receiver) =
            bounded(MAX_COMPLETED_SLOTS_IN_CHANNEL);
        blockstore.add_completed_slots_signal(completed_slots_sender);

        let block_id_repair_channels = BlockIdRepairChannels {
            repair_event_receiver,
            completed_slots_receiver,
        };

        // Shared latest switch-bank request from Votor to ReplayStage.
        let latest_switch_request = LatestSwitchRequest::default();

        let window_service = {
            let epoch_schedule = bank_forks
                .read()
                .unwrap()
                .working_bank()
                .epoch_schedule()
                .clone();
            let repair_info = RepairInfo {
                bank_forks: bank_forks.clone(),
                epoch_schedule,
                repair_validators: tvu_config.repair_validators,
                repair_whitelist: tvu_config.repair_whitelist,
                cluster_info: cluster_info.clone(),
            };
            let repair_service_channels = RepairServiceChannels::new(verified_voter_slots_receiver);
            let window_service_channels = WindowServiceChannels::new(
                verified_receiver,
                retransmit_sender,
                completed_data_sets_sender,
                repair_service_channels,
                block_id_repair_channels,
            );
            WindowService::new(
                blockstore.clone(),
                repair_socket,
                block_id_repair_socket,
                exit.clone(),
                repair_info,
                window_service_channels,
                tvu_config.shred_version,
                outstanding_repair_requests,
                tvu_config.repair_xdp_sender,
            )
        };

        let (cost_update_sender, cost_update_receiver) = unbounded();
        let (drop_bank_sender, drop_bank_receiver) = unbounded();
        let (bls_sender, bls_receiver) = bounded(MAX_BLS_MESSAGES_TO_SEND);

        let (votor_commitment_sender, commitment_service) = AggregateCommitmentService::new(
            exit.clone(),
            block_commitment_cache.clone(),
            rpc_subscriptions.clone(),
        );
        let (own_votes_sender, own_votes_receiver) =
            EvictingSender::new_bounded(MAX_ALPENGLOW_PACKET_NUM);
        let (footer_certs_sender, footer_certs_receiver) = bounded(MAX_ALPENGLOW_PACKET_NUM);

        let votor_config = VotorConfig {
            exit: exit.clone(),
            validator_exit,
            vote_account: *vote_account,
            wait_to_vote_slot,
            vote_history,
            vote_history_storage: vote_history_storage.clone(),
            generated_cert_types,
            authorized_voter_keypairs: authorized_voter_keypairs.clone(),
            blockstore: blockstore.clone(),
            bank_forks: bank_forks.clone(),
            cluster_info: cluster_info.clone(),
            leader_schedule_cache: leader_schedule_cache.clone(),
            alpenglow_slot_clock,
            consensus_metrics_sender,
            highest_finalized: highest_finalized.clone(),
            bank_forks_controller,
            bls_sender: bls_sender.clone(),
            commitment_sender: votor_commitment_sender.clone(),
            bank_notification_sender: bank_notification_sender.clone(),
            leader_window_info_sender,
            highest_parent_ready: highest_parent_ready.clone(),
            event_sender: votor_event_sender.clone(),
            latest_switch_request: latest_switch_request.clone(),
            own_vote_sender: own_votes_sender.clone(),
            own_reward_aggregates_sender: reward_aggregates_sender.clone(),
            repair_event_sender,
            event_receiver: votor_event_receiver,
            consensus_message_receiver,
            own_votes_receiver,
            footer_certs_receiver,
            consensus_metrics_receiver,
        };
        let votor = Votor::new(votor_config);

        let replay_senders = ReplaySenders {
            rpc_subscriptions,
            slot_status_notifier,
            transaction_status_sender,
            entry_notification_sender,
            bank_notification_sender,
            cost_update_sender,
            drop_bank_sender,
            block_metadata_notifier,
            votor_event_sender,
            footer_certs_sender,
            optimistic_parent_sender,
            votor_commitment_sender,
        };

        let replay_receivers = ReplayReceivers {
            ledger_signal_receiver,
            update_parent_receiver,
            optimistic_parent_receiver,
            bank_forks_controller_receiver,
            latest_switch_request,
        };

        let replay_stage_config = ReplayStageConfig {
            vote_account: *vote_account,
            authorized_voter_keypairs,
            exit: exit.clone(),
            leader_schedule_cache: leader_schedule_cache.clone(),
            replay_forks_threads: tvu_config.replay_forks_threads,
            replay_transactions_threads: tvu_config.replay_transactions_threads,
            blockstore: blockstore.clone(),
            bank_forks: bank_forks.clone(),
            cluster_info: cluster_info.clone(),
            snapshot_controller,
            replay_highest_frozen,
            highest_parent_ready,
            replay_arena: tvu_config.replay_arena,
        };

        let bls_voting_service = BLSVotingService::new(
            bls_receiver,
            cluster_info.clone(),
            votor_egress,
            highest_finalized,
        );

        let cost_update_service = CostUpdateService::new(cost_update_receiver);

        let drop_bank_service = DropBankService::new(drop_bank_receiver);

        let replay_stage = ReplayStage::new(replay_stage_config, replay_senders, replay_receivers)?;

        let blockstore_cleanup_service = BlockstoreCleanupService::new(
            blockstore.clone(),
            tvu_config.blockstore_cleanup_strategy,
            exit.clone(),
        );

        Ok(Tvu {
            fetch_stage,
            shred_sigverify,
            retransmit_stage,
            window_service,
            replay_stage,
            blockstore_cleanup_service,
            cost_update_service,
            bls_voting_service,
            peer_list_service,
            drop_bank_service,
            bls_sigverifier,
            votor,
            commitment_service,
            votor_transport_endpoint: endpoint,
            votor_transport_runtime: votor_runtime,
        })
    }

    pub fn join(mut self) -> thread::Result<()> {
        // Must ensure votor_transport_endpoint is notified before joining bls_voting_service.
        self.votor_transport_endpoint.stop();
        self.retransmit_stage.join()?;
        self.window_service.join()?;
        self.fetch_stage.join()?;
        self.shred_sigverify.join()?;
        self.blockstore_cleanup_service.join()?;
        self.replay_stage.join()?;
        self.cost_update_service.join()?;
        self.bls_voting_service.join()?;
        self.drop_bank_service.join()?;
        if let Some(rt) = self.votor_transport_runtime.take() {
            rt.block_on(self.votor_transport_endpoint.join());
            rt.shutdown_background();
        } else {
            tokio::task::block_in_place(|| {
                Handle::current().block_on(self.votor_transport_endpoint.join())
            });
        }
        // peer_list_service must be joined after votor_transport_endpoint.
        self.peer_list_service.join()?;
        self.bls_sigverifier.join()?;
        self.votor.join()?;
        self.commitment_service.join()?;
        Ok(())
    }
}
