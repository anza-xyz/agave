use {
    crate::{
        quic_client::VotorQuicClient,
        staked_validators_cache::StakedValidatorsCache,
        vote_history_storage::{SavedVoteHistoryVersions, VoteHistoryStorage},
    },
    agave_votor_messages::consensus_message::{Certificate, ConsensusMessage},
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_CAP: usize = 5;

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
        mut quic_client: VotorQuicClient,
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
                        &mut quic_client,
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
        quic_client: &mut VotorQuicClient,
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
        quic_client.send_message_to_peers(buf, peers);
    }

    fn handle_bls_op(
        cluster_info: &ClusterInfo,
        vote_history_storage: &dyn VoteHistoryStorage,
        bls_op: BLSOp,
        quic_client: &mut VotorQuicClient,
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
                    quic_client,
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
                    quic_client,
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
        solana_tpu_client_next::connection_workers_scheduler::{BindTarget, StakeIdentity},
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
        let keypair = validator_keypairs[0].node_keypair.insecure_clone();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair.insecure_clone()),
            SocketAddrSpace::Unspecified,
        );

        let cancel = CancellationToken::new();
        let bind = BindTarget::Socket(bind_to_localhost_unique().unwrap());
        let (quic_sender, _) =
            VotorQuicClient::new(runtime_handle, bind, StakeIdentity::new(&keypair), cancel)
                .unwrap();
        (
            VotingService::new(
                bls_receiver,
                Arc::new(cluster_info),
                Arc::new(NullVoteHistoryStorage::default()),
                quic_sender,
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
        let runtime = VotorQuicClient::spawn_runtime().unwrap();

        agave_logger::setup();
        let (bls_sender, bls_receiver) = crossbeam_channel::bounded(100);
        // Create listener thread on a random port we allocated and return SocketAddr to create VotingService

        // Bind to a random UDP port
        let socket = bind_to_localhost_unique().unwrap();
        let listener_addr = socket.local_addr().unwrap();

        // Create VotingService with the listener address
        let (_, validator_keypairs) =
            create_voting_service(bls_receiver, listener_addr, runtime.handle().clone());

        // Start a quic streamer to terminate connections
        let (sender, receiver) = crossbeam_channel::bounded(100);
        let stakes = validator_keypairs
            .iter()
            .map(|x| (x.node_keypair.pubkey(), 100))
            .collect();
        let staked_nodes: Arc<RwLock<StakedNodes>> = Arc::new(RwLock::new(StakedNodes::new(
            Arc::new(stakes),
            HashMap::default(), // overrides
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
        thread::sleep(Duration::from_secs(1));

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
