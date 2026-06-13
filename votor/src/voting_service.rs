use {
    crate::{
        staked_validators_cache::StakedValidatorsCache,
        vote_history_storage::{SavedVoteHistoryVersions, VoteHistoryStorage},
    },
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{ConsensusMessage, VoteMessage},
    },
    bytes::Bytes,
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_quic_datagram::{allowlist::StakedNodesAllowlist, endpoint::Datagram},
    solana_runtime::bank_forks::BankForks,
    std::{
        process,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::sync::{mpsc, mpsc::error::TrySendError},
};
#[cfg(feature = "dev-context-only-utils")]
use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, net::SocketAddr},
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
/// Target number of epochs to keep in the staked validators cache. Due to lazy-lru eviction
/// semantics, the cache may hold up to `2 * STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET` entries
/// before evicting down to this target.
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET: usize = 3;

#[derive(Debug)]
pub enum BLSOp {
    PushVote {
        vote: Arc<VoteMessage>,
        saved_vote_history: SavedVoteHistoryVersions,
    },
    PushCertificates {
        certificates: Vec<Arc<Certificate>>,
    },
    RefreshVotes {
        votes: Vec<Arc<VoteMessage>>,
    },
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

/// Test-only knob plumbed through [`ValidatorConfig`] so local-cluster tests can
/// override the (pubkey -> socket) set used to address votor peers.
#[derive(Clone)]
pub struct VotingServiceOverride {
    #[cfg(feature = "dev-context-only-utils")]
    pub override_listeners: Arc<ArcSwap<HashMap<Pubkey, SocketAddr>>>,
}

impl VotingService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bls_receiver: Receiver<BLSOp>,
        cluster_info: Arc<ClusterInfo>,
        vote_history_storage: Arc<dyn VoteHistoryStorage>,
        egress: mpsc::Sender<Datagram>,
        allowlist: Arc<StakedNodesAllowlist>,
        bank_forks: Arc<RwLock<BankForks>>,
        #[cfg(feature = "dev-context-only-utils")] test_override: Option<VotingServiceOverride>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solVotorVoteSvc".to_string())
            .spawn(move || {
                let mut staked_validators_cache = StakedValidatorsCache::new(
                    bank_forks.read().unwrap().sharable_banks(),
                    Duration::from_secs(STAKED_VALIDATORS_CACHE_TTL_S),
                    STAKED_VALIDATORS_CACHE_NUM_EPOCH_TARGET,
                    false,
                    Some(allowlist),
                    #[cfg(feature = "dev-context-only-utils")]
                    test_override
                        .map(|v| v.override_listeners)
                        .unwrap_or_default(),
                );

                info!("AlpenglowVotingService has started");
                while let Ok(bls_op) = bls_receiver.recv() {
                    Self::handle_bls_op(
                        &cluster_info,
                        vote_history_storage.as_ref(),
                        bls_op,
                        &egress,
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

        // Drop on full / closed — votor consensus tolerates loss; we
        // never want to backpressure into vote production.
        for (pubkey, addr) in staked_peers {
            let result = egress.try_send(Datagram {
                peer_pubkey: *pubkey,
                peer_address: *addr,
                message: buf.clone(),
            });
            match result {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    warn!("alpenglow egress channel full; dropping vote/cert");
                }
                Err(TrySendError::Closed(_)) => {
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
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        match bls_op {
            BLSOp::PushVote {
                vote,
                saved_vote_history,
            } => {
                let mut measure = Measure::start("alpenglow vote history save");
                if let Err(err) = vote_history_storage.store(&saved_vote_history) {
                    error!("Unable to save vote history to storage: {err:?}");
                    process::exit(1);
                }
                measure.stop();
                trace!("{measure}");
                let slot = vote.vote.slot();
                let msg = ConsensusMessage::Vote(Arc::unwrap_or_clone(vote));
                Self::broadcast_consensus_message(
                    slot,
                    cluster_info,
                    &msg,
                    egress,
                    staked_validators_cache,
                );
            }
            BLSOp::PushCertificates { certificates } => {
                for certificate in certificates {
                    let slot = certificate.cert_type.slot();
                    let message = ConsensusMessage::Certificate(Arc::unwrap_or_clone(certificate));
                    Self::broadcast_consensus_message(
                        slot,
                        cluster_info,
                        &message,
                        egress,
                        staked_validators_cache,
                    );
                }
            }
            BLSOp::RefreshVotes { votes } => {
                for vote in votes {
                    let slot = vote.vote.slot();
                    let msg = ConsensusMessage::Vote(Arc::unwrap_or_clone(vote));
                    Self::broadcast_consensus_message(
                        slot,
                        cluster_info,
                        &msg,
                        egress,
                        staked_validators_cache,
                    );
                }
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
        crate::vote_history_storage::{
            NullVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
        },
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        bytes::Bytes,
        crossbeam_channel::{Receiver, bounded, unbounded},
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_net_utils::{SocketAddrSpace, banlist::Banlist, sockets::bind_to_localhost_unique},
        solana_quic_datagram::{
            allowlist::{AllowAll, Allowlist},
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
        test_case::test_case,
        tokio::runtime::{Builder, Runtime},
    };

    /// Spin up an in-process quic-datagram endpoint with the given keypair
    /// and allowlist. Returns (endpoint, ingress_rx, bound_addr, runtime).
    fn spawn_endpoint<A: Allowlist>(
        keypair: Keypair,
        allowlist: Arc<A>,
    ) -> (
        QuicDatagramEndpoint,
        Receiver<Datagram>,
        SocketAddr,
        Runtime,
    ) {
        let rt = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let socket = bind_to_localhost_unique().expect("bind UDP");
        let addr = socket.local_addr().expect("local addr");
        let (ingress_tx, ingress_rx) = bounded(4096);
        let banlist = Arc::new(Banlist::<Pubkey>::default());
        let endpoint = QuicDatagramEndpoint::new(
            rt.handle(),
            &keypair,
            socket,
            ingress_tx,
            allowlist,
            banlist,
        )
        .expect("QuicDatagramEndpoint::new");
        (endpoint, ingress_rx, addr, rt)
    }

    fn create_voting_service(
        bls_receiver: Receiver<BLSOp>,
        spy_listener: (Pubkey, SocketAddr),
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
                Arc::new(StakedNodesAllowlist::new(HashMap::new())),
                bank_forks,
                Some(VotingServiceOverride {
                    override_listeners: Arc::new(ArcSwap::from_pointee(HashMap::from_iter([
                        spy_listener,
                    ]))),
                }),
            ),
            validator_keypairs,
        )
    }

    #[test_case(BLSOp::PushVote {
        vote: Arc::new(VoteMessage {
            vote: Vote::new_skip_vote(5),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1,
        }),
        saved_vote_history: SavedVoteHistoryVersions::Current(SavedVoteHistory::default()),
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

        let listener_kp = Keypair::new();
        let listener_pubkey = listener_kp.pubkey();
        let (endpoint, ingress_rx, listener_addr, _rt) =
            spawn_endpoint(listener_kp, Arc::new(AllowAll));

        let (client_endpoint, _client_ingress_rx, _client_addr, _client_rt) =
            spawn_endpoint(Keypair::new(), Arc::new(AllowAll));
        let egress = client_endpoint.egress.clone();

        let (bls_sender, bls_receiver) = unbounded();
        let (_voting_service, _validator_keypairs) = create_voting_service(
            bls_receiver,
            (listener_pubkey, listener_addr),
            egress.clone(),
        );

        // Trigger all connections to be established with disposable packets
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
}
