use {
    agave_votor_messages::{
        certificate::Certificate, consensus_message::VoteMessage,
        wire::VersionedWireConsensusMessage,
    },
    bytes::Bytes,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_runtime::validated_block_finalization::ValidatedBlockFinalizationCert,
    std::{
        collections::{BTreeMap, HashSet},
        ops::Bound::{Excluded, Included, Unbounded},
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::sync::{mpsc, mpsc::error::TrySendError},
};

/// The maximum amount of packets per second we expect from an honest node
pub const VOTOR_RATE_LIMIT_PPS: usize = 50;

/// Max new packets per second in steady state:
/// - Notarize + Finalize votes
/// - NotarizeFallback + Notarize + FastFinalize + Finalize certificates
///
/// 200ms slots, 6 packets * 5 slots per second = 30 PPS
const NEW_PACKETS_PER_SECOND: usize = 30;

/// The amount of packets we should send per second from the standstill queue
const STANDSTILL_REFRESH_BATCH_SIZE: usize = VOTOR_RATE_LIMIT_PPS - NEW_PACKETS_PER_SECOND;

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

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

impl VotingService {
    pub fn new(
        bls_receiver: Receiver<BLSOp>,
        cluster_info: Arc<ClusterInfo>,
        egress: mpsc::Sender<Bytes>,
        highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
    ) -> Self {
        let mut standstill_queue = StandstillRefreshQueue::default();
        let thread_hdl = Builder::new()
            .name("solVotorVoteSvc".to_string())
            .spawn(move || {
                info!("AlpenglowVotingService has started");
                loop {
                    Self::maybe_handle_standstill_queue(
                        &mut standstill_queue,
                        highest_finalized.as_ref(),
                        &cluster_info,
                        &egress,
                    );

                    let bls_op = match bls_receiver.recv_timeout(STANDSTILL_REFRESH_INTERVAL) {
                        Ok(bls_op) => bls_op,
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => continue,
                    };
                    Self::handle_bls_op(&cluster_info, bls_op, &egress, &mut standstill_queue);
                }
                info!("AlpenglowVotingService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    /// If more than 1 second has passed, prune and send out messages from the queue.
    fn maybe_handle_standstill_queue(
        standstill_queue: &mut StandstillRefreshQueue,
        highest_finalized: &RwLock<Option<ValidatedBlockFinalizationCert>>,
        cluster_info: &ClusterInfo,
        egress: &mpsc::Sender<Bytes>,
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
            highest_finalized
                .as_ref()
                .map(|hf| (hf.slot(), hf.clone_certificates()))
        };

        if let Some((highest_finalized_slot, certificates)) = highest_finalized_slot_and_certs {
            standstill_queue.prune(highest_finalized_slot.slot());

            // Refresh the latest finalization (either Finalize + Notarize or FastFinalize)
            for certificate in certificates {
                let message = VersionedWireConsensusMessage::new_from_cert(
                    certificate,
                    cluster_info.my_shred_version(),
                );
                Self::broadcast_consensus_message(&message, egress);
                num_sent_messages = num_sent_messages.saturating_add(1);
            }
        }

        // Refresh the next messages from the queue while adhering to the budget
        let remaining_budget = STANDSTILL_REFRESH_BATCH_SIZE.saturating_sub(num_sent_messages);
        standstill_queue.for_next_n_messages(remaining_budget, |message| {
            Self::broadcast_consensus_message(message, egress);
        });

        standstill_queue.reset_refresh_timer();
    }

    /// Serialize a consensus message and hand it to the endpoint. The
    /// endpoint fans it out to every connected peer.
    fn broadcast_consensus_message(
        message: &VersionedWireConsensusMessage,
        egress: &mpsc::Sender<Bytes>,
    ) {
        let buf = match wincode::serialize(message) {
            Ok(buf) => Bytes::from(buf),
            Err(err) => {
                error!("Failed to serialize alpenglow message: {err:?}");
                return;
            }
        };

        // Drop on full / closed channel: votor consensus tolerates loss, we
        // never want to backpressure into consensus, but will shout loudly.
        match egress.try_send(buf) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                error!("alpenglow transport egress channel full; dropping votes/certs!");
            }
            Err(TrySendError::Closed(_)) => {
                // this is intentional - we can tear down the transport before votor.
                warn!("alpenglow egress channel closed; must be shutting down");
            }
        }
    }

    fn handle_bls_op(
        cluster_info: &ClusterInfo,
        bls_op: BLSOp,
        egress: &mpsc::Sender<Bytes>,
        standstill_queue: &mut StandstillRefreshQueue,
    ) {
        match bls_op {
            BLSOp::PushVote { vote } => {
                let msg = VersionedWireConsensusMessage::new_from_vote(
                    Arc::unwrap_or_clone(vote),
                    cluster_info.my_shred_version(),
                );
                Self::broadcast_consensus_message(&msg, egress);
            }
            BLSOp::PushCertificates { certificates } => {
                for certificate in certificates {
                    let msg = VersionedWireConsensusMessage::new_from_cert(
                        Arc::unwrap_or_clone(certificate),
                        cluster_info.my_shred_version(),
                    );
                    Self::broadcast_consensus_message(&msg, egress);
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
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        crate::peer_list_updater::PeerListService,
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{ConsensusMessage, VoteMessage},
            migration::MigrationStatus,
            vote::Vote,
        },
        agave_votor_transport::{
            PeerList, PeerListReceiver, PeerListSender,
            endpoint::{Datagram, QuicDatagramEndpoint},
        },
        arc_swap::ArcSwap,
        bytes::Bytes,
        crossbeam_channel::{Receiver, bounded, unbounded},
        rand::Rng,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_gossip::contact_info::ContactInfo,
        solana_keypair::Keypair,
        solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
        solana_perf::packet::packet_config,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        std::{
            collections::HashMap,
            net::SocketAddr,
            num::NonZero,
            sync::{Arc, RwLock},
        },
        test_case::test_case,
        tokio::{
            runtime::{Builder, Runtime},
            sync::watch,
        },
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
                stake: NonZero::new(123).unwrap(),
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

    /// Spin up a votor transport endpoint with the given keypair.
    /// Inbound admission follows `peer_list_receiver`.
    fn spawn_endpoint(
        keypair: Keypair,
        peer_list_receiver: PeerListReceiver,
    ) -> (
        QuicDatagramEndpoint,
        mpsc::Sender<Bytes>,
        Receiver<Datagram>,
        SocketAddr,
        Runtime,
    ) {
        let rt = Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let socket = bind_to_localhost_unique().expect("bind UDP");
        let addr = socket.local_addr().expect("local addr");
        let client_socket = bind_to_localhost_unique().expect("bind client UDP");
        let (ingress_sender, ingress_receiver) = bounded(4096);
        let (egress, endpoint) = QuicDatagramEndpoint::spawn(
            rt.handle(),
            &keypair,
            vec![socket],
            client_socket,
            ingress_sender,
            peer_list_receiver,
            VOTOR_RATE_LIMIT_PPS,
            CancellationToken::new(),
        )
        .expect("QuicDatagramEndpoint::spawn");
        (endpoint, egress, ingress_receiver, addr, rt)
    }

    fn create_voting_service(
        bls_receiver: Receiver<BLSOp>,
        spy_listener: (Pubkey, SocketAddr),
        egress: mpsc::Sender<Bytes>,
        peer_list: PeerListSender,
    ) -> (
        VotingService,
        PeerListService,
        Vec<ValidatorVoteKeypairs>,
        Arc<ClusterInfo>,
    ) {
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
        // The sending node must itself be staked, otherwise the peer_list
        // refresh publishes an empty set and never connects to the listener.
        let keypair = validator_keypairs[0].node_keypair.insecure_clone();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));

        let peer_list_service = PeerListService::new(
            cluster_info.clone(),
            peer_list,
            bank_forks.read().unwrap().sharable_banks(),
            Arc::new(ArcSwap::from_pointee(HashMap::from([(
                spy_listener.0,
                Some(spy_listener.1),
            )]))),
            Arc::new(MigrationStatus::post_migration_status()),
        );
        (
            VotingService::new(
                bls_receiver,
                cluster_info.clone(),
                egress,
                Arc::new(RwLock::new(None)),
            ),
            peer_list_service,
            validator_keypairs,
            cluster_info,
        )
    }

    #[test_case(BLSOp::PushVote {
        vote: Arc::new(VoteMessage {
            vote: Vote::new_skip_vote(5),
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1,
            stake:NonZero::new(123).unwrap()
        }),
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(5),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        rank: 1,
        stake:NonZero::new(123).unwrap()
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
        stake:NonZero::new(123).unwrap()
        })],
    }, ConsensusMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(6),
        signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
        rank: 1,
        stake:NonZero::new(123).unwrap()
    }))]
    fn test_send_message(bls_op: BLSOp, expected_message: ConsensusMessage) {
        agave_logger::setup();

        let listener_kp = Keypair::new();
        let listener_pubkey = listener_kp.pubkey();
        let client_kp = Keypair::new();
        let client_pubkey = client_kp.pubkey();

        // Listener must admit the client, but never pushes to it.
        let (_spy_peer_list_sender, spy_peer_list_receiver) = watch::channel(Arc::new(PeerList {
            peers: HashMap::from([(client_pubkey, None)]),
            push_enabled: false,
        }));
        let (endpoint, _egress, ingress_rx, listener_addr, rt) =
            spawn_endpoint(listener_kp, spy_peer_list_receiver);

        // Seed the client's peer_list empty; create_voting_service installs an override
        // that injects the listener.
        let (peer_list_sender, peer_list_receiver) = watch::channel(Arc::new(PeerList {
            peers: HashMap::new(),
            push_enabled: false,
        }));
        let (client_endpoint, egress, _client_ingress_rx, _client_addr, client_rt) =
            spawn_endpoint(client_kp, peer_list_receiver);

        let (bls_sender, bls_receiver) = unbounded();
        let (_voting_service, peer_list_service, _validator_keypairs, cluster_info) =
            create_voting_service(
                bls_receiver,
                (listener_pubkey, listener_addr),
                egress.clone(),
                peer_list_sender,
            );

        // Wait for the peer_list-driven connection to land, broadcast disposable
        // packets until one reaches the listener, confirming the connection is
        // up.
        let warmup = Bytes::from_static(b"warmup");
        let warmup_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _ = egress.try_send(warmup.clone());
            if ingress_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                break;
            }
            assert!(
                Instant::now() < warmup_deadline,
                "warmup datagram did not reach listener within 5s; connection never completed",
            );
        }

        bls_sender.send(bls_op).expect("bls_sender.send");

        // The listener endpoint should receive the serialized VersionedWireConsensusMessage.
        // Drain ingress skipping the warmup datagrams (which fail to deserialize).
        let deadline = Instant::now() + Duration::from_secs(5);
        let received = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("listener never received the message");
            }
            let recv_result = ingress_rx.recv_timeout(remaining);
            if let Ok(dg) = recv_result
                && let Ok(msg) =
                    VersionedWireConsensusMessage::deserialize_with_expected_shred_version(
                        dg.message.as_ref(),
                        packet_config(),
                        0,
                    )
            {
                break msg;
            }
        };
        assert_eq!(
            received,
            VersionedWireConsensusMessage::new(expected_message, cluster_info.my_shred_version())
        );
        rt.block_on(endpoint.join());
        // Dropping the client endpoint drops its peer_list receiver, which is the
        // service's exit signal.
        client_rt.block_on(client_endpoint.join());
        peer_list_service.join().expect("peer_list_service join");
    }
}
