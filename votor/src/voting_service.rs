use {
    crate::staked_validators_cache::StakedValidatorsCache,
    agave_votor_messages::{
        certificate::Certificate, consensus_message::VoteMessage,
        wire::VersionedWireConsensusMessage,
    },
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_client::connection_cache::ConnectionCache,
    solana_clock::Slot,
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank_forks::BankForks, validated_block_finalization::ValidatedBlockFinalizationCert,
    },
    solana_transaction_error::TransportError,
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        net::SocketAddr,
        ops::Bound::{Excluded, Included, Unbounded},
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
/// Target number of epochs to keep in the staked validators cache. Due to lazy-lru eviction
/// semantics, the cache may hold up to `2 * STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET` entries
/// before evicting down to this target.
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET: usize = 3;

/// The maximum amount of packets per second we expect from an honest node
pub const VOTOR_RATE_LIMIT_PPS: u64 = 50;

/// Max new packets per second in steady state:
/// - Notarize + Finalize votes
/// - NotarizeFallback + Notarize + FastFinalize + Finalize certificates
///
/// 200ms slots, 6 packets * 5 slots per second = 30 PPS
const NEW_PACKETS_PER_SECOND: usize = 30;

/// The amount of packets we should send per second from the standstill queue
const STANDSTILL_REFRESH_BATCH_SIZE: usize = VOTOR_RATE_LIMIT_PPS as usize - NEW_PACKETS_PER_SECOND;

/// How often we should refresh messages from the standstill queue
const STANDSTILL_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum BLSOp {
    PushVote { vote: Arc<VoteMessage> },
    PushCertificates { certificates: Vec<Arc<Certificate>> },
    RefreshVotes { votes: Vec<Arc<VoteMessage>> },
    RefreshCertificates { certificates: Vec<Arc<Certificate>> },
}

#[derive(Debug)]
/// Maintains a map of messages since the last finalization if we are in standstill.
/// This queue is used to refresh the next `STANDSTILL_REFRESH_BATCH_SIZE` messages
/// every `STANDSTILL_REFRESH_INTERVAL`.
///
/// When we are not in standstill the queue will be cleared as it's prune by the highest
/// finalized slot.
struct StandstillRefreshQueue {
    messages: BTreeMap<Slot, HashSet<VersionedWireConsensusMessage>>,
    last_refresh: Instant,
    cursor: Slot,
}

impl Default for StandstillRefreshQueue {
    fn default() -> Self {
        Self {
            messages: BTreeMap::default(),
            last_refresh: Instant::now(),
            cursor: 0,
        }
    }
}

impl StandstillRefreshQueue {
    fn insert(&mut self, message: VersionedWireConsensusMessage) {
        let slot = message.slot();
        self.messages.entry(slot).or_default().insert(message);
    }

    /// Prune any state less than or equal to the highest finalized slot.
    fn prune(&mut self, highest_finalized_slot: Slot) {
        self.messages
            .retain(|slot, _| *slot > highest_finalized_slot);
        if self.cursor <= highest_finalized_slot {
            self.cursor = highest_finalized_slot;
        }
    }

    fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Calls `handle_message` for up to the next `limit` messages from the queue,
    /// updating the cursor as necessary.
    fn for_next_n_messages(
        &mut self,
        limit: usize,
        mut handle_message: impl FnMut(&VersionedWireConsensusMessage),
    ) -> usize {
        if limit == 0 || self.is_empty() {
            return 0;
        }

        let mut processed = 0usize;
        let starting_cursor = self.cursor;
        let mut reached_end = true;
        for (slot, slot_messages) in self.messages.range((Excluded(self.cursor), Unbounded)) {
            if limit.saturating_sub(processed) < slot_messages.len() {
                // We cannot process this batch as it would put us over the limit
                if processed == 0 {
                    // However this is the very first batch! This should never happen
                    // as the maximum number of possible votes & certificates for a slot is < 20.
                    // But here we are, to avoid stalling the queue completely allow us to progress
                    // at the risk of being rate limited
                    error!(
                        "First slot batch in the standstill queue exceeds votor rate limit: \
                         {slot_messages:?}"
                    );
                } else {
                    reached_end = false;
                    break;
                }
            }

            processed = processed.saturating_add(slot_messages.len());
            self.cursor = *slot;
            for message in slot_messages {
                handle_message(message);
            }
        }

        if reached_end && processed < limit && starting_cursor != 0 {
            self.cursor = 0;
            for (slot, slot_messages) in self
                .messages
                .range((Excluded(0), Included(starting_cursor)))
            {
                if limit.saturating_sub(processed) < slot_messages.len() {
                    break;
                }

                processed = processed.saturating_add(slot_messages.len());
                self.cursor = *slot;
                for message in slot_messages {
                    handle_message(message);
                }
            }
        }

        processed
    }

    /// Whether enough time has passed to refresh messages in the queue
    fn should_refresh(&self) -> bool {
        self.last_refresh.elapsed() >= STANDSTILL_REFRESH_INTERVAL
    }

    /// Reset the refresh timer
    fn reset_refresh_timer(&mut self) {
        self.last_refresh = Instant::now();
    }

    #[cfg(test)]
    fn next_messages(&mut self, limit: usize) -> Vec<(Slot, VersionedWireConsensusMessage)> {
        let mut messages = Vec::new();
        self.for_next_n_messages(limit, |message| {
            messages.push((message.slot(), message.clone()));
        });
        messages
    }
}

fn send_message(
    buf: Vec<u8>,
    socket: &SocketAddr,
    connection_cache: &ConnectionCache,
) -> Result<(), TransportError> {
    let client = connection_cache.get_connection(socket);

    client.send_data_async(Arc::new(buf))
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
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
        test_override: Option<VotingServiceOverride>,
    ) -> Self {
        let (additional_listeners, alpenglow_port_override) = match test_override {
            None => (Vec::new(), None),
            Some(VotingServiceOverride {
                additional_listeners,
                alpenglow_port_override,
            }) => (additional_listeners, Some(alpenglow_port_override)),
        };
        let mut standstill_queue = StandstillRefreshQueue::default();

        let thread_hdl = Builder::new()
            .name("solVotorVoteSvc".to_string())
            .spawn(move || {
                let mut staked_validators_cache = StakedValidatorsCache::new(
                    bank_forks.clone(),
                    Duration::from_secs(STAKED_VALIDATORS_CACHE_TTL_S),
                    STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET,
                    false,
                    alpenglow_port_override,
                );

                info!("AlpenglowVotingService has started");
                loop {
                    Self::maybe_handle_standstill_queue(
                        &mut standstill_queue,
                        highest_finalized.as_ref(),
                        &cluster_info,
                        &connection_cache,
                        &additional_listeners,
                        &mut staked_validators_cache,
                    );

                    let bls_op = match bls_receiver.recv_timeout(STANDSTILL_REFRESH_INTERVAL) {
                        Ok(bls_op) => bls_op,
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => continue,
                    };
                    Self::handle_bls_op(
                        &cluster_info,
                        bls_op,
                        &connection_cache,
                        &additional_listeners,
                        &mut staked_validators_cache,
                        &mut standstill_queue,
                    );
                }
                info!("AlpenglowVotingService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    /// If more than 1 second has passed, prune and send out messages from the queue
    fn maybe_handle_standstill_queue(
        standstill_queue: &mut StandstillRefreshQueue,
        highest_finalized: &RwLock<Option<ValidatedBlockFinalizationCert>>,
        cluster_info: &ClusterInfo,
        connection_cache: &ConnectionCache,
        additional_listeners: &[SocketAddr],
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        if !standstill_queue.should_refresh() {
            return;
        }

        if standstill_queue.is_empty() {
            standstill_queue.reset_refresh_timer();
            return;
        }

        let mut num_sent_messages = 0usize;

        let highest_finalized_slot_and_certs = {
            let highest_finalized = highest_finalized.read().unwrap();
            highest_finalized.as_ref().map(|highest_finalized| {
                (
                    highest_finalized.slot(),
                    highest_finalized.clone_certificates(),
                )
            })
        };

        if let Some((highest_finalized_slot, certificates)) = highest_finalized_slot_and_certs {
            standstill_queue.prune(highest_finalized_slot.slot());

            // Refresh the latest finalization (either Finalize + Notarize or FastFinalize)
            for certificate in certificates {
                let message = VersionedWireConsensusMessage::new_from_cert(
                    certificate,
                    cluster_info.my_shred_version(),
                );
                Self::broadcast_consensus_message(
                    cluster_info,
                    &message,
                    connection_cache,
                    additional_listeners,
                    staked_validators_cache,
                );
                num_sent_messages = num_sent_messages.saturating_add(1);
            }
        }

        // Refresh the next messages from the queue while adhering to the budget
        let remaining_budget = STANDSTILL_REFRESH_BATCH_SIZE.saturating_sub(num_sent_messages);
        standstill_queue.for_next_n_messages(remaining_budget, |message| {
            Self::broadcast_consensus_message(
                cluster_info,
                message,
                connection_cache,
                additional_listeners,
                staked_validators_cache,
            );
        });

        standstill_queue.reset_refresh_timer();
    }

    fn broadcast_consensus_message(
        cluster_info: &ClusterInfo,
        message: &VersionedWireConsensusMessage,
        connection_cache: &ConnectionCache,
        additional_listeners: &[SocketAddr],
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        let buf = match wincode::serialize(message) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Failed to serialize alpenglow message: {err:?}");
                return;
            }
        };

        let (staked_validator_alpenglow_sockets, _) = staked_validators_cache
            .get_staked_validators_by_slot(message.slot(), cluster_info, Instant::now());
        let sockets = additional_listeners
            .iter()
            .chain(staked_validator_alpenglow_sockets.iter());

        // We use send_message in a loop right now because we worry that sending packets too fast
        // will cause a packet spike and overwhelm the network. If we later find out that this is
        // not an issue, we can optimize this by using multi_targret_send or similar methods.
        for socket in sockets {
            if let Err(e) = send_message(buf.clone(), socket, connection_cache) {
                warn!("Failed to send alpenglow message to {socket}: {e:?}");
            }
        }
    }

    fn handle_bls_op(
        cluster_info: &ClusterInfo,
        bls_op: BLSOp,
        connection_cache: &ConnectionCache,
        additional_listeners: &[SocketAddr],
        staked_validators_cache: &mut StakedValidatorsCache,
        standstill_queue: &mut StandstillRefreshQueue,
    ) {
        match bls_op {
            BLSOp::PushVote { vote, .. } => {
                let msg = VersionedWireConsensusMessage::new_from_vote(
                    Arc::unwrap_or_clone(vote),
                    cluster_info.my_shred_version(),
                );
                Self::broadcast_consensus_message(
                    cluster_info,
                    &msg,
                    connection_cache,
                    additional_listeners,
                    staked_validators_cache,
                );
            }
            BLSOp::PushCertificates { certificates } => {
                for certificate in certificates {
                    let msg = VersionedWireConsensusMessage::new_from_cert(
                        Arc::unwrap_or_clone(certificate),
                        cluster_info.my_shred_version(),
                    );
                    Self::broadcast_consensus_message(
                        cluster_info,
                        &msg,
                        connection_cache,
                        additional_listeners,
                        staked_validators_cache,
                    );
                }
            }
            BLSOp::RefreshVotes { votes } => {
                for vote in votes {
                    let msg = VersionedWireConsensusMessage::new_from_vote(
                        Arc::unwrap_or_clone(vote),
                        cluster_info.my_shred_version(),
                    );
                    standstill_queue.insert(msg);
                }
            }
            BLSOp::RefreshCertificates { certificates } => {
                for certificate in certificates {
                    let message = VersionedWireConsensusMessage::new_from_cert(
                        Arc::unwrap_or_clone(certificate),
                        cluster_info.my_shred_version(),
                    );
                    standstill_queue.insert(message);
                }
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
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        crossbeam_channel::bounded,
        rand::Rng,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
        solana_perf::packet::packet_config,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        solana_streamer::{
            nonblocking::swqos::SwQosConfig,
            quic::{QuicStreamerConfig, SpawnServerResult, spawn_stake_weighted_qos_server},
            streamer::StakedNodes,
        },
        std::{
            net::SocketAddr,
            sync::{Arc, RwLock},
        },
        test_case::test_case,
        tokio_util::sync::CancellationToken,
    };

    fn test_vote_message(
        vote: Vote,
        rank: u16,
        shred_verion: u16,
    ) -> VersionedWireConsensusMessage {
        VersionedWireConsensusMessage::new_from_vote(
            VoteMessage {
                vote,
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                rank,
            },
            shred_verion,
        )
    }

    fn test_certificate_message(
        cert_type: CertificateType,
        my_shred_version: u16,
    ) -> VersionedWireConsensusMessage {
        VersionedWireConsensusMessage::new_from_cert(
            Certificate {
                cert_type,
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: Vec::new(),
            },
            my_shred_version,
        )
    }

    #[test]
    fn test_standstill_refresh_queue_cycles_by_slot() {
        let shred_verion = rand::rng().random();
        let mut queue = StandstillRefreshQueue::default();
        let vote_5 = test_vote_message(Vote::new_skip_vote(5), 1, shred_verion);
        let vote_6 = test_vote_message(Vote::new_skip_vote(6), 1, shred_verion);
        let cert_6 = test_certificate_message(CertificateType::Finalize(6), shred_verion);

        queue.insert(vote_6.clone());
        queue.insert(cert_6.clone());
        queue.insert(vote_5.clone());

        assert_eq!(queue.next_messages(2), vec![(5, vote_5.clone())]);
        let slot_6_messages = queue.next_messages(2);
        assert_eq!(slot_6_messages.len(), 2);
        assert!(slot_6_messages.contains(&(6, vote_6.clone())));
        assert!(slot_6_messages.contains(&(6, cert_6.clone())));
        assert_eq!(queue.next_messages(2), vec![(5, vote_5)]);
    }

    #[test]
    fn test_standstill_refresh_queue_deduplicates_identical_messages() {
        let shred_verion = rand::rng().random();
        let mut queue = StandstillRefreshQueue::default();
        let message = test_vote_message(Vote::new_skip_vote(8), 1, shred_verion);

        queue.insert(message.clone());
        queue.insert(message.clone());

        assert_eq!(queue.next_messages(10), vec![(8, message)]);
    }

    #[test]
    fn test_standstill_refresh_queue_wraps_to_fill_budget() {
        let mut queue = StandstillRefreshQueue::default();
        let shred_verion = rand::rng().random();
        let vote_3 = test_vote_message(Vote::new_skip_vote(3), 1, shred_verion);
        let vote_4 = test_vote_message(Vote::new_skip_vote(4), 1, shred_verion);
        let vote_6 = test_vote_message(Vote::new_skip_vote(6), 1, shred_verion);

        queue.insert(vote_3.clone());
        queue.insert(vote_4.clone());
        queue.insert(vote_6.clone());
        queue.cursor = 5;

        assert_eq!(
            queue.next_messages(10),
            vec![(6, vote_6), (3, vote_3), (4, vote_4)]
        );
    }

    #[test]
    fn test_standstill_refresh_queue_prunes_finalized_messages() {
        let shred_verion = rand::rng().random();
        let mut queue = StandstillRefreshQueue::default();
        let vote_5 = test_vote_message(Vote::new_skip_vote(5), 1, shred_verion);
        let cert_6 = test_certificate_message(CertificateType::Finalize(6), shred_verion);
        let vote_6 = test_vote_message(Vote::new_skip_vote(6), 1, shred_verion);
        let vote_7 = test_vote_message(Vote::new_skip_vote(7), 1, shred_verion);

        queue.insert(vote_5);
        queue.insert(cert_6);
        queue.insert(vote_6);
        queue.insert(vote_7.clone());

        queue.prune(6);
        assert_eq!(queue.next_messages(10), vec![(7, vote_7)]);
    }

    fn create_voting_service(
        bls_receiver: Receiver<BLSOp>,
        listener: SocketAddr,
    ) -> (VotingService, Vec<ValidatorVoteKeypairs>, Arc<ClusterInfo>) {
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
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));

        (
            VotingService::new(
                bls_receiver,
                cluster_info.clone(),
                Arc::new(ConnectionCache::new_quic(
                    "TestAlpenglowConnectionCache",
                    10,
                )),
                bank_forks,
                Arc::new(RwLock::new(None)),
                Some(VotingServiceOverride {
                    additional_listeners: vec![listener],
                    alpenglow_port_override: AlpenglowPortOverride::default(),
                }),
            ),
            validator_keypairs,
            cluster_info,
        )
    }

    #[test_case(BLSOp::PushVote {
        vote: Arc::new(VoteMessage {
            vote: Vote::new_skip_vote(5),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1,
        }),
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(5),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        rank: 1,
    }))]
    #[test_case(BLSOp::PushCertificates {
        certificates: vec![Arc::new(Certificate {
                cert_type: CertificateType::Skip(5),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: Vec::new(),
        })],
    }, ConsensusMessage::Certificate(Certificate {
        cert_type: CertificateType::Skip(5),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        bitmap: Vec::new(),
    }))]
    #[test_case(BLSOp::RefreshVotes {
        votes: vec![Arc::new(VoteMessage {
            vote: Vote::new_skip_vote(6),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1,
        })],
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(6),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        rank: 1,
    }))]
    fn test_send_message(bls_op: BLSOp, expected_message: ConsensusMessage) {
        agave_logger::setup();
        let (bls_sender, bls_receiver) = bounded(1024);
        // Create listener thread on a random port we allocated and return SocketAddr to create VotingService

        // Bind to a random UDP port
        let socket = bind_to_localhost_unique().unwrap();
        let listener_addr = socket.local_addr().unwrap();

        // Create VotingService with the listener address
        let (_, validator_keypairs, cluster_info) =
            create_voting_service(bls_receiver, listener_addr);

        // Send a BLS message via the VotingService
        assert!(bls_sender.send(bls_op).is_ok());

        // Start a quick streamer to handle quick control packets
        let (sender, receiver) = bounded(1024);
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
        } = spawn_stake_weighted_qos_server(
            "AlpenglowLocalClusterTest",
            "voting_service_test",
            [socket.into()],
            &Keypair::new(),
            sender,
            staked_nodes,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
            cancel.clone(),
        )
        .unwrap();

        let packets = receiver.recv().unwrap();
        let packet = packets.first().expect("No packets received");
        let received_message =
            VersionedWireConsensusMessage::deserialize_with_expected_shred_version(
                packet.data(..).unwrap(),
                packet_config(),
                cluster_info.my_shred_version(),
            )
            .unwrap();
        assert_eq!(
            VersionedWireConsensusMessage::new(expected_message, cluster_info.my_shred_version()),
            received_message
        );
        cancel.cancel();
        quic_server_thread.join().unwrap();
    }
}
