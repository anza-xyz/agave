use {
    crate::{
        staked_validators_cache::StakedValidatorsCache,
        vote_history_storage::{SavedVoteHistoryVersions, VoteHistoryStorage},
    },
    agave_votor_messages::{certificate::Certificate, consensus_message::ConsensusMessage},
    bytes::Bytes,
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_quic_datagram::endpoint::Datagram,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::sync::mpsc,
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
/// Target number of epochs to keep in the staked validators cache. Due to lazy-lru eviction
/// semantics, the cache may hold up to `2 * STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET` entries
/// before evicting down to this target.
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET: usize = 3;

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

/// Test-only additional listener: the voting service will fan out every
/// consensus message to this `(pubkey, addr)` peer alongside the live
/// staked-validators list. Used by unit tests and local-cluster fixtures
/// to attach a probe endpoint.
#[derive(Clone, Debug)]
pub struct AdditionalListener {
    pub pubkey: Pubkey,
    pub addr: SocketAddr,
}

#[derive(Clone)]
pub struct VotingServiceOverride {
    pub additional_listeners: Vec<AdditionalListener>,
    pub alpenglow_port_override: AlpenglowPortOverride,
}

impl VotingService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bls_receiver: Receiver<BLSOp>,
        cluster_info: Arc<ClusterInfo>,
        vote_history_storage: Arc<dyn VoteHistoryStorage>,
        egress: mpsc::Sender<Datagram>,
        allowlist: Option<Arc<solana_quic_datagram::StakedNodesAllowlist>>,
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

        // Additional-listener pubkeys are test-only sniffers / probes —
        // they aren't in the staked-nodes set but we still want to dial
        // them from this validator's client side. Union them into the
        // allowlist via the cache.
        let extra_admit: HashSet<Pubkey> = additional_listeners.iter().map(|l| l.pubkey).collect();

        let thread_hdl = Builder::new()
            .name("solVotorVoteSvc".to_string())
            .spawn(move || {
                let mut staked_validators_cache = StakedValidatorsCache::new(
                    bank_forks.clone(),
                    Duration::from_secs(STAKED_VALIDATORS_CACHE_TTL_S),
                    STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET,
                    false,
                    alpenglow_port_override,
                    allowlist,
                    extra_admit,
                );

                info!("AlpenglowVotingService has started");
                while let Ok(bls_op) = bls_receiver.recv() {
                    Self::handle_bls_op(
                        &cluster_info,
                        vote_history_storage.as_ref(),
                        bls_op,
                        &egress,
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
        egress: &mpsc::Sender<Datagram>,
        additional_listeners: &[AdditionalListener],
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        let buf = match wincode::serialize(message) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Failed to serialize alpenglow message: {err:?}");
                return;
            }
        };
        let buf = Bytes::from(buf);

        let (staked_peers, _) = staked_validators_cache.get_staked_validators_by_slot(
            slot,
            cluster_info,
            Instant::now(),
        );
        let peers = additional_listeners
            .iter()
            .map(|listener| (listener.pubkey, listener.addr))
            .chain(staked_peers.iter().copied());

        // Drop on full / closed — votor consensus tolerates loss; we
        // never want to backpressure into vote production.
        for (pubkey, addr) in peers {
            let result = egress.try_send(Datagram {
                peer_pubkey: pubkey,
                peer_address: addr,
                message: buf.clone(),
            });
            match result {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("alpenglow egress channel full; dropping vote/cert");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("alpenglow egress channel closed; endpoint shutting down");
                    return;
                }
            }
        }
    }

    fn handle_bls_op(
        cluster_info: &ClusterInfo,
        vote_history_storage: &dyn VoteHistoryStorage,
        bls_op: BLSOp,
        egress: &mpsc::Sender<Datagram>,
        additional_listeners: &[AdditionalListener],
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
                    egress,
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
                    egress,
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
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        crate::{
            datagram_endpoint,
            vote_history_storage::{
                NullVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
            },
        },
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        bytes::Bytes,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
        solana_pubkey::Pubkey as PubkeyAlias,
        solana_quic_datagram::{
            Banlist,
            endpoint::{Datagram, QuicDatagramEndpoint},
        },
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        std::collections::HashSet,
        test_case::test_case,
        tokio::runtime::Runtime,
    };

    /// Generate a keypair whose pubkey is strictly less than `upper`.
    /// The voting-service test relies on the lex-pubkey direction rule:
    /// the client (lower pubkey) dials the listener (higher pubkey).
    fn keypair_below(upper: &PubkeyAlias) -> Keypair {
        loop {
            let k = Keypair::new();
            if &k.pubkey() < upper {
                return k;
            }
        }
    }

    /// Spin up an in-process quic-datagram endpoint with the given keypair
    /// and allowlist. Returns (endpoint, ingress_rx, bound_addr, runtime).
    fn spawn_endpoint<A: solana_quic_datagram::Allowlist>(
        keypair: Keypair,
        allowlist: Arc<A>,
    ) -> (
        QuicDatagramEndpoint,
        crossbeam_channel::Receiver<solana_quic_datagram::endpoint::Datagram>,
        SocketAddr,
        Runtime,
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let socket = bind_to_localhost_unique().expect("bind UDP");
        let addr = socket.local_addr().expect("local addr");
        let (ingress_tx, ingress_rx) = crossbeam_channel::bounded(4096);
        let banlist = Arc::new(Banlist::<Pubkey>::default());
        let endpoint = datagram_endpoint::spawn(
            rt.handle(),
            &keypair,
            socket,
            ingress_tx,
            allowlist,
            banlist,
        )
        .expect("datagram_endpoint::spawn");
        (endpoint, ingress_rx, addr, rt)
    }

    fn create_voting_service(
        bls_receiver: Receiver<BLSOp>,
        listener: AdditionalListener,
        egress: mpsc::Sender<Datagram>,
    ) -> (VotingService, Vec<ValidatorVoteKeypairs>) {
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
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        );

        (
            VotingService::new(
                bls_receiver,
                Arc::new(cluster_info),
                Arc::new(NullVoteHistoryStorage::default()),
                egress,
                None, // no allowlist gating in this unit test
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
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1,
        })),
        slot: 5,
        saved_vote_history: SavedVoteHistoryVersions::Current(SavedVoteHistory::default()),
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(5),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        rank: 1,
    }))]
    #[test_case(BLSOp::PushCertificate {
        certificate: Arc::new(Certificate {
            cert_type: CertificateType::Skip(5),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: Vec::new(),
        }),
    }, ConsensusMessage::Certificate(Certificate {
        cert_type: CertificateType::Skip(5),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        bitmap: Vec::new(),
    }))]
    fn test_send_message(bls_op: BLSOp, expected_message: ConsensusMessage) {
        agave_logger::setup();

        // Listener identity is generated first; the client (lower pubkey)
        // then dials it per the lex-pubkey direction rule. Use AllowAll
        // allowlist for both ends — the test is exercising the egress
        // path, not the allowlist policy.
        let listener_kp = Keypair::new();
        let listener_pubkey = listener_kp.pubkey();
        let client_kp = keypair_below(&listener_pubkey);
        let (endpoint, ingress_rx, listener_addr, _rt) = spawn_endpoint(
            listener_kp,
            Arc::new(solana_quic_datagram::allowlist::AllowAll),
        );

        let (client_endpoint, _client_ingress_rx, _client_addr, _client_rt) = spawn_endpoint(
            client_kp,
            Arc::new(solana_quic_datagram::allowlist::AllowAll),
        );
        let egress = client_endpoint.egress.clone();

        let (bls_sender, bls_receiver) = crossbeam_channel::unbounded();
        let listener = AdditionalListener {
            pubkey: listener_pubkey,
            addr: listener_addr,
        };
        let (_voting_service, _validator_keypairs) =
            create_voting_service(bls_receiver, listener, egress.clone());

        // Warm the QUIC connection. The voting service issues exactly one
        // `try_send` per BLS op; the first send to a fresh peer is consumed
        // as the dial trigger and dropped on the floor. Drive a poll-loop
        // here so that by the time the real op arrives, the slot holds an
        // `Established`. 500ms is generous for a localhost handshake.
        let warmup = Bytes::from_static(b"warmup");
        let warmup_deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let _ = egress.try_send(Datagram {
                peer_pubkey: listener_pubkey,
                peer_address: listener_addr,
                message: warmup.clone(),
            });
            if ingress_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                break;
            }
            assert!(
                Instant::now() < warmup_deadline,
                "warmup datagram did not reach listener within 500ms; dial never completed",
            );
        }

        // Send the BLS op through. The cached `Established` carries it.
        bls_sender.send(bls_op).expect("bls_sender.send");

        // The listener endpoint should receive the serialized
        // ConsensusMessage. Drain ingress until we see a match.
        let deadline = Instant::now() + Duration::from_secs(5);
        let received = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("listener never received the message");
            }
            let recv_result = ingress_rx.recv_timeout(remaining);
            if let Ok(dg) = recv_result {
                if let Ok(msg) = wincode::deserialize::<ConsensusMessage>(&dg.message) {
                    break msg;
                }
            }
        };
        assert_eq!(received, expected_message);

        endpoint.close();
        client_endpoint.close();
    }

    // Silence unused-import warnings for symbols only referenced by name
    // through test_case derivation paths.
    #[allow(unused_imports)]
    use HashSet as _UnusedHashSet;
}
