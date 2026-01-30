use {
    crate::{
        staked_validators_cache::StakedValidatorsCache,
        vote_history_storage::{SavedVoteHistoryVersions, VoteHistoryStorage},
    },
    agave_votor_messages::consensus_message::{Certificate, ConsensusMessage},
    bincode::serialize,
    crossbeam_channel::Receiver,
    quinn::Endpoint,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::MAX_ALPENGLOW_VOTE_ACCOUNTS, bank_forks::BankForks},
    solana_tpu_client_next::{
        connection_workers_scheduler::{setup_endpoint, BindTarget, StakeIdentity},
        transaction_batch::TransactionBatch,
        workers_cache::{shutdown_worker, WorkersCache},
        ConnectionWorkersSchedulerError, SendTransactionStats,
    },
    std::{
        collections::HashMap,
        io,
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::runtime::Runtime,
    tokio_util::sync::CancellationToken,
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_CAP: usize = 5;

/// Channel size for the tpu-client-next workers.
/// This essentially buffers messages which are not yet sent on the wire.
/// Keeping this small ensures that if some network-layer backlog accumulates,
/// we get errors sooner.
const WORKER_CHANNEL_SIZE: usize = 8;

/// How many times to attempt to reconnect to a given validator before giving up.
const MAX_RECONNECT_ATTEMPTS: usize = 3;

/// QUIC connection setup timeout. Needs to be long enough to accommodate
/// longest RTT link on the internet + possible packet loss.
const QUIC_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(1000);

/// Reporting interval for stats reported by tpu-client-next
const QUIC_STATS_REPORTING_INTERVAL: Duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);

/// Number of threads to use for the QUIC runtime sending BLS messages.
const QUIC_RUNTIME_THREADS: usize = 4;

#[derive(Debug)]
pub enum BLSOp {
    PushVote {
        message: Arc<ConsensusMessage>,
        slot: Slot,
        saved_vote_history: SavedVoteHistoryVersions,
    },
    PushCertificate {
        certificate: Arc<Certificate>,
    },
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

/// Override for Alpenglow ports to allow testing with different ports
/// The last_modified is used to determine if the override has changed so
/// StakedValidatorsCache can refresh its cache.
/// Inside the map, the key is the validator's vote pubkey and the value
/// is the overridden socket address.
/// For example, if you want validator A to send messages for validator B's
/// Alpenglow port to a new_address, you would insert an entry into the A's
/// map like this: (B will not get the message as a result):
/// `override_map.insert(validator_b_pubkey, new_address);`
#[derive(Clone, Default)]
pub struct AlpenglowPortOverride {
    inner: Arc<RwLock<AlpenglowPortOverrideInner>>,
}

#[derive(Clone)]
struct AlpenglowPortOverrideInner {
    override_map: HashMap<Pubkey, SocketAddr>,
    last_modified: Instant,
}

impl Default for AlpenglowPortOverrideInner {
    fn default() -> Self {
        Self {
            override_map: HashMap::new(),
            last_modified: Instant::now(),
        }
    }
}

impl AlpenglowPortOverride {
    pub fn update_override(&self, new_override: HashMap<Pubkey, SocketAddr>) {
        let mut inner = self.inner.write().unwrap();
        inner.override_map = new_override;
        inner.last_modified = Instant::now();
    }

    pub fn has_new_override(&self, previous: Instant) -> bool {
        self.inner.read().unwrap().last_modified != previous
    }

    pub fn last_modified(&self) -> Instant {
        self.inner.read().unwrap().last_modified
    }

    pub fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.override_map.clear();
        inner.last_modified = Instant::now();
    }

    pub fn get_override_map(&self) -> HashMap<Pubkey, SocketAddr> {
        self.inner.read().unwrap().override_map.clone()
    }
}

#[derive(Clone)]
pub struct VotingServiceOverride {
    pub additional_listeners: Vec<SocketAddr>,
    pub alpenglow_port_override: AlpenglowPortOverride,
}

impl VotingService {
    pub fn new(
        bls_receiver: Receiver<BLSOp>,
        cluster_info: Arc<ClusterInfo>,
        vote_history_storage: Arc<dyn VoteHistoryStorage>,
        mut quic_sender: VotorQuicSender,
        bank_forks: Arc<RwLock<BankForks>>,
        test_override: Option<VotingServiceOverride>,
    ) -> Self {
        let (additional_listeners, alpenglow_port_override) = match test_override {
            None => (Vec::new(), None),
            Some(VotingServiceOverride {
                additional_listeners,
                alpenglow_port_override,
            }) => (additional_listeners, Some(alpenglow_port_override)),
        };

        let thread_hdl = Builder::new()
            .name("solVotorVoteSvc".to_string())
            .spawn(move || {
                let mut staked_validators_cache = StakedValidatorsCache::new(
                    bank_forks.clone(),
                    Duration::from_secs(STAKED_VALIDATORS_CACHE_TTL_S),
                    STAKED_VALIDATORS_CACHE_NUM_EPOCH_CAP,
                    false,
                    alpenglow_port_override,
                );

                info!("AlpenglowVotingService has started");
                while let Ok(bls_op) = bls_receiver.recv() {
                    Self::handle_bls_op(
                        &cluster_info,
                        vote_history_storage.as_ref(),
                        bls_op,
                        &mut quic_sender,
                        &additional_listeners,
                        &mut staked_validators_cache,
                    );
                }
                info!("AlpenglowVotingService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn broadcast_consensus_message(
        slot: Slot,
        cluster_info: &ClusterInfo,
        message: &ConsensusMessage,
        quic_sender: &mut VotorQuicSender,
        additional_listeners: &[SocketAddr],
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        let buf = match serialize(message) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Failed to serialize alpenglow message: {err:?}");
                return;
            }
        };

        let (staked_validator_alpenglow_sockets, _) = staked_validators_cache
            .get_staked_validators_by_slot(slot, cluster_info, Instant::now());
        let peers = additional_listeners
            .iter()
            .chain(staked_validator_alpenglow_sockets.iter())
            .copied();
        quic_sender.send_message_to_peers(buf, peers);
    }

    fn handle_bls_op(
        cluster_info: &ClusterInfo,
        vote_history_storage: &dyn VoteHistoryStorage,
        bls_op: BLSOp,
        quic_sender: &mut VotorQuicSender,
        additional_listeners: &[SocketAddr],
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        match bls_op {
            BLSOp::PushVote {
                message,
                slot,
                saved_vote_history,
            } => {
                let mut measure = Measure::start("alpenglow vote history save");
                if let Err(err) = vote_history_storage.store(&saved_vote_history) {
                    error!("Unable to save vote history to storage: {err:?}");
                    std::process::exit(1);
                }
                measure.stop();
                trace!("{measure}");

                Self::broadcast_consensus_message(
                    slot,
                    cluster_info,
                    &message,
                    quic_sender,
                    additional_listeners,
                    staked_validators_cache,
                );
            }
            BLSOp::PushCertificate { certificate } => {
                let vote_slot = certificate.cert_type.slot();
                let message = ConsensusMessage::Certificate((*certificate).clone());
                Self::broadcast_consensus_message(
                    vote_slot,
                    cluster_info,
                    &message,
                    quic_sender,
                    additional_listeners,
                    staked_validators_cache,
                );
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

/// QUIC sender for Votor based on tpu-client-next crate
pub struct VotorQuicSender {
    workers: WorkersCache,
    endpoint: Endpoint,
    stats: Arc<SendTransactionStats>,
    runtime_handle: tokio::runtime::Handle,
}

impl VotorQuicSender {
    /// Spawns a runtime configured for vote sending
    pub fn spawn_runtime() -> io::Result<Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(QUIC_RUNTIME_THREADS)
            .enable_all()
            .build()
    }

    pub fn new(
        runtime_handle: tokio::runtime::Handle,
        bind: BindTarget,
        stake_identity: StakeIdentity,
        cancel: CancellationToken,
    ) -> Result<Self, ConnectionWorkersSchedulerError> {
        let tokio_guard = runtime_handle.enter();
        let endpoint = setup_endpoint(bind, Some(stake_identity))?;
        let workers = WorkersCache::new(MAX_ALPENGLOW_VOTE_ACCOUNTS * 2, cancel.clone());

        let stats = Arc::new(SendTransactionStats::default());
        runtime_handle.spawn(stats.clone().report_to_influxdb(
            "VotorSender",
            QUIC_STATS_REPORTING_INTERVAL,
            cancel,
        ));
        drop(tokio_guard);
        Ok(Self {
            workers,
            endpoint,
            stats,
            runtime_handle,
        })
    }

    /// Broadcasts the provided buffer to the peers
    pub fn send_message_to_peers(&mut self, buf: Vec<u8>, peers: impl Iterator<Item = SocketAddr>) {
        // clone on TransactionBatch is cheap (compared to cloning the buf)
        let txs_batch = TransactionBatch::new(vec![buf]);
        let tokio_guard = self.runtime_handle.enter();
        for peer in peers {
            debug!("Sending message to peer: {peer}");
            if let Some(old_worker) = self.workers.ensure_worker(
                peer,
                &self.endpoint,
                WORKER_CHANNEL_SIZE,
                true,
                MAX_RECONNECT_ATTEMPTS,
                QUIC_HANDSHAKE_TIMEOUT,
                self.stats.clone(),
            ) {
                shutdown_worker(old_worker)
            }
            std::thread::sleep(Duration::from_millis(100));
            if let Err(e) = self
                .workers
                .try_send_transactions_to_address(&peer, txs_batch.clone())
            {
                warn!("Failed to send alpenglow message to {peer}: {e:?}");
            }
        }
        drop(tokio_guard);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::vote_history_storage::{
            NullVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
        },
        agave_votor_messages::{
            consensus_message::{Certificate, CertificateType, ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        solana_bls_signatures::Signature as BLSSignature,
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_net_utils::{sockets::bind_to_localhost_unique, SocketAddrSpace},
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_streamer::{
            nonblocking::simple_qos::SimpleQosConfig,
            quic::{spawn_simple_qos_server, QuicStreamerConfig, SpawnServerResult},
            streamer::StakedNodes,
        },
        std::{
            net::SocketAddr,
            sync::{Arc, RwLock},
        },
        test_case::test_case,
        tokio_util::sync::CancellationToken,
    };

    fn create_voting_service(
        bls_receiver: Receiver<BLSOp>,
        listener: SocketAddr,
        runtime_handle: tokio::runtime::Handle,
    ) -> (VotingService, Vec<ValidatorVoteKeypairs>) {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair.insecure_clone()),
            SocketAddrSpace::Unspecified,
        );

        let cancel = CancellationToken::new();
        let bind = BindTarget::Socket(bind_to_localhost_unique().unwrap());
        (
            VotingService::new(
                bls_receiver,
                Arc::new(cluster_info),
                Arc::new(NullVoteHistoryStorage::default()),
                VotorQuicSender::new(runtime_handle, bind, StakeIdentity::new(&keypair), cancel)
                    .unwrap(),
                bank_forks,
                Some(VotingServiceOverride {
                    additional_listeners: vec![listener],
                    alpenglow_port_override: AlpenglowPortOverride::default(),
                }),
            ),
            validator_keypairs,
        )
    }

    #[test_case(BLSOp::PushVote {
        message: Arc::new(ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_skip_vote(5),
            signature: BLSSignature::default(),
            rank: 1,
        })),
        slot: 5,
        saved_vote_history: SavedVoteHistoryVersions::Current(SavedVoteHistory::default()),
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(5),
        signature: BLSSignature::default(),
        rank: 1,
    }))]
    #[test_case(BLSOp::PushCertificate {
        certificate: Arc::new(Certificate {
            cert_type: CertificateType::Skip(5),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        }),
    }, ConsensusMessage::Certificate(Certificate {
        cert_type: CertificateType::Skip(5),
        signature: BLSSignature::default(),
        bitmap: Vec::new(),
    }))]
    fn test_send_message(bls_op: BLSOp, expected_message: ConsensusMessage) {
        let runtime = VotorQuicSender::spawn_runtime().unwrap();

        agave_logger::setup();
        let (bls_sender, bls_receiver) = crossbeam_channel::unbounded();
        // Create listener thread on a random port we allocated and return SocketAddr to create VotingService

        // Bind to a random UDP port
        let socket = bind_to_localhost_unique().unwrap();
        let listener_addr = socket.local_addr().unwrap();

        // Create VotingService with the listener address
        let (_, validator_keypairs) =
            create_voting_service(bls_receiver, listener_addr, runtime.handle().clone());

        // Start a quic streamer to terminate connections
        let (sender, receiver) = crossbeam_channel::unbounded();
        let stakes = validator_keypairs
            .iter()
            .map(|x| (x.node_keypair.pubkey(), 100))
            .collect();
        let staked_nodes: Arc<RwLock<StakedNodes>> = Arc::new(RwLock::new(StakedNodes::new(
            Arc::new(stakes),
            HashMap::<Pubkey, u64>::default(), // overrides
        )));
        let cancel = CancellationToken::new();
        let SpawnServerResult {
            endpoints: _,
            thread: quic_server_thread,
            key_updater: _,
        } = spawn_simple_qos_server(
            "AlpenglowLocalClusterTest",
            "voting_service_test",
            [socket],
            &Keypair::new(),
            sender,
            staked_nodes,
            QuicStreamerConfig::default_for_tests(),
            SimpleQosConfig::default(),
            cancel.clone(),
        )
        .unwrap();
        // make sure the server is up and running before sending packets
        thread::sleep(Duration::from_secs(2));

        // Send a BLS message via the VotingService
        assert!(bls_sender.send(bls_op).is_ok());

        let packets = receiver.recv().unwrap();
        let packet = packets.first().expect("No packets received");
        let received_message = packet
            .deserialize_slice::<ConsensusMessage, _>(..)
            .unwrap_or_else(|err| {
                panic!(
                    "Failed to deserialize BLSMessage: {:?} {:?}",
                    size_of::<ConsensusMessage>(),
                    err
                )
            });
        assert_eq!(received_message, expected_message);
        cancel.cancel();
        quic_server_thread.join().unwrap();
    }
}
