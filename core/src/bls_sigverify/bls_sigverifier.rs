//! The BLS signature verifier.

use {
    super::{
        bls_cert_sigverify::{CertPayload, verify_and_send_certificates},
        bls_vote_sigverify::{VotePayload, verify_and_send_votes},
        errors::SigVerifyError,
        stats::SigVerifierStats,
    },
    crate::{
        block_creation_loop::rewards::{certs_builder::wants_vote, msg_types::AddVoteMessage},
        cluster_info_vote_listener::VerifiedVoterSlotsSender,
    },
    agave_votor::{
        consensus_metrics::ConsensusMetricsEventSender, generated_cert_types::GeneratedCertTypes,
    },
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::{ConsensusMessage, VoteMessage},
        migration::MigrationStatus,
    },
    crossbeam_channel::{Receiver, Select, Sender, TryRecvError},
    lazy_lru::LruCache,
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_quic_datagram::{Banlist, endpoint::Datagram},
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        collections::HashSet,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder},
        time::Duration,
    },
};

/// If a cert or vote is so many slots in the future relative to the root slot, it is considered
/// invalid and discarded.
///
/// This also sets an upper bound on how much storage the various structs in this module require.
pub(super) const NUM_SLOTS_FOR_VERIFY: Slot = 90_000;

/// If we receive an invalid certificate or vote from a QUIC connection, we ban the sender.
/// We ban the sender for 2 days which roughly corresponds to an epoch
pub(super) const BAN_TIMEOUT: Duration = Duration::from_hours(48);

/// Capacity of the cross-transport dedup cache.
///
/// PLACEHOLDER: a fixed-size LRU keyed by a hash of the raw message bytes.
/// During the dual-stack migration the same consensus message arrives over
/// both transports; this drops the second copy before sigverify. To be
/// refined in a follow-up (e.g. time-bounded eviction, content-aware keying).
const DEDUP_CACHE_CAPACITY: usize = 1 << 16;

/// How long the sigverifier blocks waiting for inbound messages before waking.
/// Also the idle cadence at which it reports stats (~one slot).
const INGRESS_RECV_TIMEOUT: Duration = Duration::from_millis(400);

pub(crate) struct SigVerifierContext {
    pub(crate) migration_status: Arc<MigrationStatus>,
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    pub(crate) sharable_banks: SharableBanks,
    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) leader_schedule: Arc<LeaderScheduleCache>,
    pub(crate) num_threads: usize,
    pub(crate) generated_cert_types: Arc<GeneratedCertTypes>,
}

/// Which transport delivered an inbound consensus message. Tracked during the
/// dual-stack migration to measure per-transport volume and which transport
/// delivers each message first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VotorTransport {
    /// New QUIC-datagram transport.
    Datagram,
    /// Legacy QUIC-stream transport.
    Stream,
}

/// A consensus-message [`Datagram`] tagged with the transport that delivered
/// it. The tag is attached at read time based on which channel the message
/// arrived on; it carries the transport into [`SigVerifier::extract_and_filter_msgs`]
/// for the per-transport metrics.
struct IngressMessage {
    transport: VotorTransport,
    datagram: Datagram,
}

pub(crate) struct SigVerifierChannels {
    /// Inbound consensus messages from the QUIC-datagram endpoint. `None` when
    /// that transport was not constructed (non-Testnet/Development clusters).
    pub(crate) datagram_receiver: Option<Receiver<Datagram>>,
    /// Inbound consensus messages from the legacy QUIC-stream server (after the
    /// PacketBatch->Datagram adapter). `None` when the legacy server was not
    /// spawned.
    pub(crate) stream_receiver: Option<Receiver<Datagram>>,
    pub(crate) channel_to_repair: VerifiedVoterSlotsSender,
    pub(crate) channel_to_reward: Sender<AddVoteMessage>,
    pub(crate) channel_to_pool: Sender<Vec<ConsensusMessage>>,
    pub(crate) channel_to_metrics: ConsensusMetricsEventSender,
}

/// Starts the BLS sigverifier service in its own dedicated thread.
pub(crate) fn spawn_service(
    exit: Arc<AtomicBool>,
    context: SigVerifierContext,
    channels: SigVerifierChannels,
) -> thread::JoinHandle<()> {
    let verifier = SigVerifier::new(context, channels);

    Builder::new()
        .name("solSigVerBLS".to_string())
        .spawn(move || verifier.run(exit))
        .unwrap()
}

struct SigVerifier {
    migration_status: Arc<MigrationStatus>,
    banlist: Arc<Banlist<Pubkey>>,
    channels: SigVerifierChannels,
    /// Container to look up root banks from.
    sharable_banks: SharableBanks,
    stats: SigVerifierStats,
    /// Set of recently verified certs to avoid duplicate work.
    verified_certs: HashSet<CertificateType>,
    /// Tracks when the cache was last pruned.
    last_checked_root_slot: Slot,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    /// thread pool to use for all parallel tasks
    thread_pool: ThreadPool,
    generated_cert_types: Arc<GeneratedCertTypes>,
    /// Cross-transport dedup cache (placeholder). Keyed by a hash of the raw
    /// message bytes; value unused. See [`DEDUP_CACHE_CAPACITY`].
    recent_msgs: LruCache<u64, ()>,
    /// Hasher state for the dedup cache.
    dedup_hasher: ahash::RandomState,
}

impl SigVerifier {
    fn new(context: SigVerifierContext, channels: SigVerifierChannels) -> Self {
        let SigVerifierContext {
            migration_status,
            banlist,
            sharable_banks,
            cluster_info,
            leader_schedule,
            num_threads,
            generated_cert_types,
        } = context;
        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("solSigVerBLS{i:02}"))
            .build()
            .unwrap();
        let root_slot = sharable_banks.root().slot();
        Self {
            migration_status,
            banlist,
            channels,
            sharable_banks,
            stats: SigVerifierStats::new(root_slot),
            verified_certs: HashSet::new(),
            last_checked_root_slot: 0,
            cluster_info,
            leader_schedule,
            thread_pool,
            generated_cert_types,
            recent_msgs: LruCache::new(DEDUP_CACHE_CAPACITY),
            dedup_hasher: ahash::RandomState::new(),
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>) {
        while !exit.load(Ordering::Relaxed) {
            const SOFT_RECEIVE_CAP: usize = 5000;
            let Ok(batches) = self.recv_batches(SOFT_RECEIVE_CAP) else {
                error!("all votor ingress channels disconnected: Exiting.");
                break;
            };
            if self.migration_status.is_pre_feature_activation() {
                // No votor traffic before activation; skip work and reporting
                // to avoid emitting empty datapoints on non-alpenglow nodes.
                continue;
            }

            if !batches.is_empty() {
                let (verify_res, verify_time_us) =
                    measure_us!(self.verify_and_send_batches(batches));
                self.stats
                    .verify_and_send_batch_us
                    .add_sample(verify_time_us);
                if let Err(e) = verify_res {
                    error!("verify_and_send_batch() failed with {e}. Exiting.");
                    break;
                }
            }
            // Report once per slot (~400ms). Driven every loop iteration —
            // including idle ones, since `recv_batches` wakes at least every
            // 400ms — so the cadence holds with or without traffic.
            self.stats.maybe_report(self.sharable_banks.root().slot());
        }
        self.stats.do_report(self.sharable_banks.root().slot());
    }

    fn verify_and_send_batches(
        &mut self,
        items: Vec<IngressMessage>,
    ) -> Result<(), SigVerifyError> {
        let root_bank = self.sharable_banks.root();
        self.maybe_prune_caches(root_bank.slot());

        let ((certs_to_verify, votes_to_verify), extract_msgs_us) =
            measure_us!(self.extract_and_filter_msgs(items, &root_bank));
        self.stats
            .extract_filter_msgs_us
            .add_sample(extract_msgs_us);

        let (votes_result, certs_result) = self.thread_pool.join(
            || {
                verify_and_send_votes(
                    votes_to_verify,
                    &root_bank,
                    &self.cluster_info,
                    &self.leader_schedule,
                    &self.banlist,
                    &self.thread_pool,
                    &self.channels,
                )
            },
            || {
                verify_and_send_certificates(
                    &mut self.verified_certs,
                    certs_to_verify,
                    &root_bank,
                    &self.channels.channel_to_pool,
                    &self.banlist,
                    &self.thread_pool,
                )
            },
        );

        let vote_stats = votes_result?;
        let cert_stats = certs_result?;

        self.stats.vote_stats.merge(vote_stats);
        self.stats.cert_stats.merge(cert_stats);
        Ok(())
    }

    fn maybe_prune_caches(&mut self, root_slot: Slot) {
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() >= root_slot);
        }
    }

    fn extract_and_filter_msgs(
        &mut self,
        items: Vec<IngressMessage>,
        root_bank: &Bank,
    ) -> (Vec<CertPayload>, Vec<VotePayload>) {
        let root_slot = root_bank.slot();
        let mut certs = Vec::new();
        let mut votes = Vec::new();
        let mut num_pkts = 0u64;
        for IngressMessage {
            transport,
            datagram:
                Datagram {
                    peer_pubkey: remote_pubkey,
                    message: bytes,
                    ..
                },
        } in items
        {
            num_pkts = num_pkts.saturating_add(1);
            // Per-transport receive volume (counts every arrival, including
            // copies later dropped as duplicates).
            match transport {
                VotorTransport::Datagram => self.stats.num_recv_datagram += 1,
                VotorTransport::Stream => self.stats.num_recv_stream += 1,
            }
            // Cross-transport dedup (placeholder): the same message from the
            // same sender reaches us over both the legacy QUIC-stream and the
            // new QUIC-datagram transport during migration. Drop the second
            // copy before the (expensive) deserialize + sigverify. Keyed on
            // (sender, raw bytes) so that distinct senders broadcasting an
            // identical payload (e.g. the same certificate) are not collapsed
            // here — cert dedup is handled separately via `verified_certs`.
            let digest = self
                .dedup_hasher
                .hash_one((remote_pubkey.as_ref(), bytes.as_ref()));
            if self.recent_msgs.get(&digest).is_some() {
                self.stats.num_dedup_dropped += 1;
                continue;
            }
            self.recent_msgs.put(digest, ());
            // This is the first copy of this message we've seen — record which
            // transport delivered it first (won the race).
            match transport {
                VotorTransport::Datagram => self.stats.num_first_datagram += 1,
                VotorTransport::Stream => self.stats.num_first_stream += 1,
            }
            let Ok(msg) = wincode::deserialize::<ConsensusMessage>(&bytes) else {
                self.stats.num_malformed_pkts += 1;
                continue;
            };
            match msg {
                ConsensusMessage::Vote(vote) => {
                    if let Some((pubkey, bls_pubkey)) = self.keep_vote(&vote, root_bank) {
                        votes.push(VotePayload {
                            vote_message: vote,
                            bls_pubkey,
                            pubkey,
                            remote_pubkey,
                            prepared_payload: None,
                        });
                    }
                }
                ConsensusMessage::Certificate(cert) => {
                    if cert.cert_type.slot() < root_slot {
                        self.stats.num_old_certs_received += 1;
                        continue;
                    }
                    if self.verified_certs.contains(&cert.cert_type) {
                        self.stats.num_verified_certs_received += 1;
                        continue;
                    }
                    if self.generated_cert_types.has_cert(&cert.cert_type) {
                        self.stats.num_generated_certs_received += 1;
                        continue;
                    }
                    certs.push(CertPayload {
                        cert,
                        remote_pubkey,
                    });
                }
            }
        }
        self.stats.num_pkts.add_sample(num_pkts);
        (certs, votes)
    }

    /// If this vote should be verified, then returns the sender's Pubkey and BlsPubkey.
    fn keep_vote(
        &mut self,
        vote: &VoteMessage,
        root_bank: &Bank,
    ) -> Option<(Pubkey, PopVerified<BlsPubkeyAffine>)> {
        let root_slot = root_bank.slot();
        let Some(rank_map) = root_bank.get_rank_map(vote.vote.slot()) else {
            self.stats.discard_vote_no_epoch_stakes += 1;
            return None;
        };
        let entry = rank_map
            .get_pubkey_stake_entry(vote.rank.into())
            .or_else(|| {
                self.stats.discard_vote_invalid_rank += 1;
                None
            })?;
        let ret = Some((entry.vote_account_pubkey, entry.bls_pubkey));
        if vote.vote.slot() > root_slot {
            return ret;
        }
        if wants_vote(&self.cluster_info, &self.leader_schedule, root_slot, vote) {
            return ret;
        }
        self.stats.num_old_votes_received += 1;
        None
    }

    /// Receive up to `soft_receive_cap` consensus messages across the two
    /// transport ingress channels, tagging each with the channel it arrived on
    /// (the datagram endpoint vs. the legacy stream server). Blocks up to
    /// [`INGRESS_RECV_TIMEOUT`] — which also bounds how long the run loop
    /// sleeps before its per-slot stats report while idle.
    ///
    /// A channel that disconnects is dropped from the set; returns `Err(())`
    /// only once *both* channels are gone.
    fn recv_batches(&mut self, soft_receive_cap: usize) -> Result<Vec<IngressMessage>, ()> {
        // Block until at least one live channel is ready (data or disconnect),
        // or the idle timeout fires.
        {
            let mut sel = Select::new();
            let dg = self
                .channels
                .datagram_receiver
                .as_ref()
                .map(|rx| sel.recv(rx));
            let st = self
                .channels
                .stream_receiver
                .as_ref()
                .map(|rx| sel.recv(rx));
            if dg.is_none() && st.is_none() {
                return Err(());
            }
            if sel.ready_timeout(INGRESS_RECV_TIMEOUT).is_err() {
                return Ok(Vec::new()); // idle — no traffic this interval
            }
        }
        // Drain both channels, tagging by source. A channel found disconnected
        // is dropped from the set so the next `Select` won't spin on it.
        let mut items = Vec::with_capacity(soft_receive_cap);
        let dg_alive = Self::drain_channel(
            &mut self.channels.datagram_receiver,
            VotorTransport::Datagram,
            &mut items,
            soft_receive_cap,
        );
        let st_alive = Self::drain_channel(
            &mut self.channels.stream_receiver,
            VotorTransport::Stream,
            &mut items,
            soft_receive_cap,
        );
        if !dg_alive && !st_alive {
            return Err(());
        }
        Ok(items)
    }

    /// Drain queued datagrams from `slot` (if present) into `out` up to `cap`,
    /// tagging each with `transport`. On disconnect, sets `*slot = None`.
    /// Returns whether the channel is still live afterwards.
    fn drain_channel(
        slot: &mut Option<Receiver<Datagram>>,
        transport: VotorTransport,
        out: &mut Vec<IngressMessage>,
        cap: usize,
    ) -> bool {
        let Some(rx) = slot.as_ref() else {
            return false;
        };
        while out.len() < cap {
            match rx.try_recv() {
                Ok(datagram) => out.push(IngressMessage {
                    transport,
                    datagram,
                }),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => {
                    *slot = None;
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::cluster_info_vote_listener::VerifiedVoterSlotsReceiver,
        agave_votor::{
            consensus_metrics::ConsensusMetricsEventReceiver,
            consensus_pool::certificate_builder::CertificateBuilder,
        },
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        bitvec::prelude::{BitVec, Lsb0},
        crossbeam_channel::{Receiver, TryRecvError},
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature},
        solana_epoch_schedule::EpochSchedule,
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        solana_signer_store::encode_base2,
    };

    fn new_test_banlist() -> Arc<Banlist<Pubkey>> {
        Arc::new(Banlist::<Pubkey>::default())
    }

    struct TestContext {
        verifier: SigVerifier,
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        banlist: Arc<Banlist<Pubkey>>,

        _packet_sender: Sender<Datagram>,
        repair_receiver: VerifiedVoterSlotsReceiver,
        _reward_receiver: Receiver<AddVoteMessage>,
        pool_receiver: Receiver<Vec<ConsensusMessage>>,
        _metrics_receiver: ConsensusMetricsEventReceiver,
        generated_cert_types: Arc<GeneratedCertTypes>,
    }

    impl TestContext {
        fn new() -> Self {
            let (channel_to_pool, pool_receiver) = crossbeam_channel::unbounded();
            Self::new_with_pool_channel(channel_to_pool, pool_receiver)
        }

        fn new_with_pool_channel(
            channel_to_pool: Sender<Vec<ConsensusMessage>>,
            pool_receiver: Receiver<Vec<ConsensusMessage>>,
        ) -> Self {
            let num_validators = 10;
            let validator_keypairs = (0..num_validators)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let stakes_vec = (0..validator_keypairs.len())
                .map(|i| 1_000 - i as u64)
                .collect::<Vec<_>>();
            let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                stakes_vec,
            );
            genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
            let bank = Bank::new_for_tests(&genesis.genesis_config);
            let bank_forks = BankForks::new_rw_arc(bank);
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            let keypair = Keypair::new();
            let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
            let cluster_info = Arc::new(ClusterInfo::new(
                contact_info,
                Arc::new(keypair),
                SocketAddrSpace::Unspecified,
            ));
            let leader_schedule =
                Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

            let (channel_to_repair, repair_receiver) = crossbeam_channel::unbounded();
            let (channel_to_reward, reward_receiver) = crossbeam_channel::unbounded();
            let (packet_sender, packet_receiver) = crossbeam_channel::unbounded();
            let (channel_to_metrics, metrics_receiver) = crossbeam_channel::unbounded();

            let generated_cert_types = Arc::new(GeneratedCertTypes::default());
            let banlist = new_test_banlist();
            let verifier = SigVerifier::new(
                SigVerifierContext {
                    migration_status: Arc::new(MigrationStatus::default()),
                    banlist: banlist.clone(),
                    sharable_banks,
                    cluster_info,
                    leader_schedule,
                    num_threads: 4,
                    generated_cert_types: generated_cert_types.clone(),
                },
                SigVerifierChannels {
                    datagram_receiver: Some(packet_receiver),
                    stream_receiver: None,
                    channel_to_repair,
                    channel_to_reward,
                    channel_to_pool,
                    channel_to_metrics,
                },
            );
            Self {
                validator_keypairs,
                verifier,
                banlist,
                _packet_sender: packet_sender,
                repair_receiver,
                _reward_receiver: reward_receiver,
                pool_receiver,
                _metrics_receiver: metrics_receiver,
                generated_cert_types,
            }
        }
    }

    fn create_signed_vote_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let payload = wincode::serialize(&vote).expect("Failed to serialize vote");
        let signature: Signature = bls_keypair.sign(&payload).into();
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
        }
    }

    fn create_signed_certificate_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        cert_type: CertificateType,
        ranks: &[usize],
    ) -> Certificate {
        let mut builder = CertificateBuilder::new(cert_type);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = cert_type.to_source_vote();
        let vote_messages: Vec<VoteMessage> = ranks
            .iter()
            .map(|&rank| create_signed_vote_message(validator_keypairs, vote, rank))
            .collect();

        builder
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        builder.build().expect("Failed to build certificate")
    }

    fn expect_no_receive<T: std::fmt::Debug>(receiver: &Receiver<T>) {
        match receiver.try_recv().unwrap_err() {
            TryRecvError::Empty => (),
            e => {
                panic!("unexpected error {e:?}");
            }
        }
    }

    #[test]
    fn test_blssigverifier_cross_transport_dedup() {
        // The same (sender, message) reaching us over both transports must be
        // verified once: the second copy is dropped before sigverify.
        let mut ctx = TestContext::new();

        let sender = Pubkey::new_unique();
        let vote_message =
            create_signed_vote_message(&ctx.validator_keypairs, Vote::new_finalization_vote(5), 2);
        let msg = ConsensusMessage::Vote(vote_message);
        // Same message from the same sender, datagram first then stream.
        let items = vec![
            message_to_item_with_transport(&msg, sender, VotorTransport::Datagram),
            message_to_item_with_transport(&msg, sender, VotorTransport::Stream),
        ];

        ctx.verifier.verify_and_send_batches(items).unwrap();

        assert_eq!(ctx.verifier.stats.num_dedup_dropped, 1);
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            1,
            "duplicate must not be forwarded twice"
        );
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 1);
        // Both arrivals counted per transport; datagram won the race.
        assert_eq!(ctx.verifier.stats.num_recv_datagram, 1);
        assert_eq!(ctx.verifier.stats.num_recv_stream, 1);
        assert_eq!(ctx.verifier.stats.num_first_datagram, 1);
        assert_eq!(ctx.verifier.stats.num_first_stream, 0);

        // A distinct sender broadcasting identical bytes is NOT collapsed here.
        // Deliver it stream-first to exercise the other win counter.
        let other_sender = Pubkey::new_unique();
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(vec![message_to_item_with_transport(
                &msg,
                other_sender,
                VotorTransport::Stream,
            )])
            .unwrap();
        assert_eq!(ctx.verifier.stats.num_dedup_dropped, 0);
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 1);
        assert_eq!(ctx.verifier.stats.num_recv_stream, 1);
        assert_eq!(ctx.verifier.stats.num_first_stream, 1);
        assert_eq!(ctx.verifier.stats.num_first_datagram, 0);
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let mut ctx = TestContext::new();

        let vote_rank1 = 2;
        let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert =
            create_signed_certificate_message(&ctx.validator_keypairs, cert_type, &cert_ranks);
        let messages1 = vec![
            ConsensusMessage::Vote(vote_message1),
            ConsensusMessage::Certificate(cert),
        ];

        ctx.verifier
            .verify_and_send_batches(messages_to_items(&messages1))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 2);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent, 1);
        let received_verified_votes1 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes1,
            (
                ctx.validator_keypairs[vote_rank1].vote_keypair.pubkey(),
                vec![5]
            )
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_notarization_vote(6, Hash::new_unique()),
            vote_rank2,
        );
        let messages2 = vec![ConsensusMessage::Vote(vote_message2)];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(messages_to_items(&messages2))
            .unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent, 0);
        let received_verified_votes2 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes2,
            (
                ctx.validator_keypairs[vote_rank2].vote_keypair.pubkey(),
                vec![6]
            )
        );

        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_notarization_fallback_vote(7, Hash::new_unique()),
            vote_rank3,
        );
        let messages3 = vec![ConsensusMessage::Vote(vote_message3)];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(messages_to_items(&messages3))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent, 0);
        let received_verified_votes3 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes3,
            (
                ctx.validator_keypairs[vote_rank3].vote_keypair.pubkey(),
                vec![7]
            )
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let mut ctx = TestContext::new();

        // An empty payload won't deserialize as ConsensusMessage; the
        // sigverifier should count it as malformed.
        let items = vec![IngressMessage {
            transport: VotorTransport::Datagram,
            datagram: Datagram {
                peer_pubkey: Pubkey::new_unique(),
                peer_address: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                message: bytes::Bytes::new(),
            },
        }];
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent, 0);
        assert_eq!(ctx.verifier.stats.num_malformed_pkts, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with no epoch stakes
        let vote_message_no_stakes = create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            0,
        );
        let messages_no_stakes = vec![ConsensusMessage::Vote(vote_message_no_stakes)];

        ctx.verifier
            .verify_and_send_batches(messages_to_items(&messages_no_stakes))
            .unwrap();

        assert_eq!(ctx.verifier.stats.discard_vote_no_epoch_stakes, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with invalid rank
        let messages_invalid_rank = vec![ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank: 1000, // Invalid rank
        })];
        ctx.verifier
            .verify_and_send_batches(messages_to_items(&messages_invalid_rank))
            .unwrap();
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        agave_logger::setup();
        let (channel_to_pool, pool_receiver) = crossbeam_channel::bounded(1);
        let mut ctx = TestContext::new_with_pool_channel(channel_to_pool, pool_receiver);

        let msg1 = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let msg2 = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_notarization_fallback_vote(6, Hash::new_unique()),
            2,
        ));
        ctx.verifier
            .verify_and_send_batches(messages_to_items(std::slice::from_ref(&msg1)))
            .unwrap();

        // The cap-1 channel is now full.  The second send hits Full and falls
        // back to a blocking send (see `send_votes_to_pool`); drain in a
        // background thread so the blocking send can complete.
        let pool_receiver = ctx.pool_receiver.clone();
        let drain = std::thread::spawn(move || {
            let m1 = pool_receiver.recv().expect("recv msg1");
            let m2 = pool_receiver.recv().expect("recv msg2");
            // No leftover messages on the channel after both deliveries.
            assert!(matches!(
                pool_receiver.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ));
            (m1, m2)
        });

        ctx.verifier
            .verify_and_send_batches(messages_to_items(std::slice::from_ref(&msg2)))
            .unwrap();

        let (m1_recv, m2_recv) = drain.join().expect("drain joined");
        // Both messages were eventually delivered (no silent drop).
        assert_eq!(m1_recv, vec![msg1]);
        assert_eq!(m2_recv, vec![msg2]);
        // pool_sent counts every message that made it onto the channel,
        // whether via try_send or the blocking fallback.
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, 2);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let mut ctx = TestContext::new();

        // Close the pool receiver to simulate a disconnected channel.
        drop(ctx.pool_receiver);

        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let messages = vec![msg];
        let result = ctx
            .verifier
            .verify_and_send_batches(messages_to_items(&messages));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);
        let vote_payload = wincode::serialize(&vote).expect("Failed to serialize vote");

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: Signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            packets.push(message_to_item(&consensus_message, Pubkey::new_unique()));
        }

        let items: Vec<IngressMessage> = packets;
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            num_votes,
            "Did not send all valid packets"
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let _vote1_payload = wincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_notarization_vote(43, Hash::new_unique());
        let _vote2_payload = wincode::serialize(&vote2).expect("Failed to serialize vote");

        // Group 1 votes
        for (i, _) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(num_votes_group1)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.validator_keypairs,
                vote1,
                i,
            ));
            packets.push(message_to_item(&msg, Pubkey::new_unique()));
        }

        // Group 2 votes
        for (i, _) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .skip(num_votes_group1)
            .take(num_votes_group2)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.validator_keypairs,
                vote2,
                i,
            ));
            packets.push(message_to_item(&msg, Pubkey::new_unique()));
        }

        let items: Vec<IngressMessage> = packets;
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            num_votes,
            "Did not send all valid packets"
        );
        assert_eq!(
            ctx.verifier.stats.vote_stats.distinct_votes_stats.count(),
            1
        );
        assert_eq!(
            ctx.verifier
                .stats
                .vote_stats
                .distinct_votes_stats
                .mean::<u64>()
                .unwrap(),
            2
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_invalid_in_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let invalid_rank = 3; // This voter will sign vote 2 with an invalid signature.
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote1_payload = wincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_skip_vote(43);
        let vote2_payload = wincode::serialize(&vote2).expect("Failed to serialize vote");
        let invalid_payload =
            wincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;

            // Split the votes: Ranks 0, 1 sign vote 1. Ranks 2, 3, 4 sign vote 2.
            let (vote, payload) = if i < 2 {
                (vote1, &vote1_payload)
            } else {
                (vote2, &vote2_payload)
            };

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(payload).into()
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            packets.push(message_to_item(&consensus_message, Pubkey::new_unique()));
        }

        let items: Vec<IngressMessage> = packets;
        ctx.verifier.verify_and_send_batches(items).unwrap();
        let sent_messages: Vec<_> = ctx.pool_receiver.try_iter().flatten().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.vote == vote2 && vm.rank == invalid_rank
            } else {
                false
            }
        }));
    }

    #[test]
    fn test_blssigverifier_verify_votes_one_invalid_signature() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let invalid_rank = 2;
        let mut packets = Vec::with_capacity(num_votes);
        let mut consensus_messages = Vec::with_capacity(num_votes); // ADDED: To hold messages for later comparison.

        let vote = Vote::new_skip_vote(42);
        let valid_vote_payload = wincode::serialize(&vote).expect("Failed to serialize vote");
        let invalid_vote_payload =
            wincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_vote_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(&valid_vote_payload).into() // Valid signature
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });

            consensus_messages.push(consensus_message.clone());

            packets.push(message_to_item(&consensus_message, Pubkey::new_unique()));
        }

        let items: Vec<IngressMessage> = packets;
        ctx.verifier.verify_and_send_batches(items).unwrap();
        let sent_messages: Vec<_> = ctx.pool_receiver.try_iter().flatten().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );

        // Ensure the message with the invalid rank is not in the sent messages.
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.rank == invalid_rank
            } else {
                false
            }
        }));
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let mut ctx = TestContext::new();

        // 2/3 of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 2).div_ceil(3);
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_just_enough_stake() {
        let mut ctx = TestContext::new();

        // 60% of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 6).div_ceil(10);
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_not_enough_stake() {
        let mut ctx = TestContext::new();

        // < 60% of validators sign the cert
        assert!(ctx.validator_keypairs.len() >= 2);
        let num_signers = (ctx.validator_keypairs.len() * 6) / 10 - 1;
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        // The call still succeeds, but the packet is marked for discard.
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            0,
            "This certificate should be invalid"
        );
        assert_eq!(ctx.verifier.stats.cert_stats.stake_verification_failed, 1);
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..7).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_just_enough_stake() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..6).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_not_enough_stake() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..5).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            0,
            "This certificate should be invalid"
        );
        assert_eq!(ctx.verifier.stats.cert_stats.stake_verification_failed, 1);
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let mut ctx = TestContext::new();

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert = Certificate {
            cert_type,
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]), // Use a default/wrong signature
            bitmap: encoded_bitmap,
        };
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(
            ctx.verifier.stats.cert_stats.signature_verification_failed,
            1
        );
    }

    #[test]
    fn test_verify_mixed_valid_batch() {
        let mut ctx = TestContext::new();

        let mut packets = Vec::new();
        let num_votes = 2;

        let vote = Vote::new_skip_vote(42);
        let vote_payload = wincode::serialize(&vote).unwrap();
        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: Signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            packets.push(message_to_item(&consensus_message, Pubkey::new_unique()));
        }

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert_original_vote = Vote::new_notarization_vote(10, cert_type.to_block().unwrap().1);
        let cert_payload = wincode::serialize(&cert_original_vote).unwrap();

        let cert_vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = ctx.validator_keypairs[i].bls_keypair.sign(&cert_payload);
                VoteMessage {
                    vote: cert_original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&cert_vote_messages)
            .expect("Failed to aggregate votes for certificate");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        packets.push(message_to_item(
            &consensus_message_cert,
            Pubkey::new_unique(),
        ));

        let items: Vec<IngressMessage> = packets;
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().flatten().count(),
            num_votes + 1,
            "All valid messages in a mixed batch should be sent"
        );
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent, num_votes as u64);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent, 1);
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let mut ctx = TestContext::new();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload = wincode::serialize(&vote).unwrap();
        let bls_keypair = &ctx.validator_keypairs[0].bls_keypair;
        let signature: Signature = bls_keypair.sign(&vote_payload).into();

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
        });

        let items = messages_to_items(&[consensus_message]);
        ctx.verifier.verify_and_send_batches(items).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank, 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (votes_for_repair_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (reward_votes_sender, _reward_votes_receiver) = crossbeam_channel::unbounded();
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let (bank0, _temp_bank_forks) = bank0.wrap_with_bank_forks_for_tests();
        let bank5 = Bank::new_from_parent(bank0, SlotLeader::default(), 5);
        let bank_forks = BankForks::new_rw_arc(bank5);

        bank_forks.write().unwrap().set_root(5, None, None);

        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        let (_packet_sender, packet_receiver) = crossbeam_channel::unbounded();
        let mut sig_verifier = SigVerifier::new(
            SigVerifierContext {
                migration_status: Arc::new(MigrationStatus::default()),
                banlist: new_test_banlist(),
                sharable_banks,
                cluster_info,
                leader_schedule,
                num_threads: 4,
                generated_cert_types: Arc::new(GeneratedCertTypes::default()),
            },
            SigVerifierChannels {
                datagram_receiver: Some(packet_receiver),
                stream_receiver: None,
                channel_to_repair: votes_for_repair_sender,
                channel_to_reward: reward_votes_sender,
                channel_to_pool: message_sender,
                channel_to_metrics: consensus_metrics_sender,
            },
        );

        let vote = Vote::new_skip_vote(2);
        let vote_payload = wincode::serialize(&vote).unwrap();
        let bls_keypair = &validator_keypairs[0].bls_keypair;
        let signature: Signature = bls_keypair.sign(&vote_payload).into();
        let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: 0,
        });
        let items_vote = messages_to_items(&[consensus_message_vote]);

        sig_verifier.verify_and_send_batches(items_vote).unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_votes_received, 1);

        let cert = create_signed_certificate_message(
            &validator_keypairs,
            CertificateType::Finalize(3),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let items_cert = messages_to_items(&[consensus_message_cert]);

        sig_verifier.verify_and_send_batches(items_cert).unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_certs_received, 1);
        assert_eq!(sig_verifier.stats.num_old_votes_received, 1);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let mut ctx = TestContext::new();

        // 80% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 8).div_ceil(10);
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let original_vote = Vote::new_notarization_vote(slot, block_hash);
        let signed_payload = wincode::serialize(&original_vote).unwrap();
        let mut vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = ctx.validator_keypairs[i].bls_keypair.sign(&signed_payload);
                VoteMessage {
                    vote: original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();

        let mut builder1 = CertificateBuilder::new(cert_type);
        builder1
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert1 = builder1.build().expect("Failed to build certificate");
        let consensus_message1 = ConsensusMessage::Certificate(cert1);
        let items1 = messages_to_items(&[consensus_message1]);

        ctx.verifier.verify_and_send_batches(items1).unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 1);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify, 1);

        vote_messages.pop(); // Remove one signature
        let mut builder2 = CertificateBuilder::new(cert_type);
        builder2
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert2 = builder2.build().expect("Failed to build certificate");
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let items2 = messages_to_items(&[consensus_message2]);

        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier.verify_and_send_batches(items2).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify, 0);
    }

    #[test]
    fn test_banlist_not_updated_for_valid_vote_and_cert() {
        let mut ctx = TestContext::new();

        let vote_message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_skip_vote(42),
            0,
        ));
        let cert_message = ConsensusMessage::Certificate(create_signed_certificate_message(
            &ctx.validator_keypairs,
            CertificateType::Notarize(43, Hash::new_unique()),
            &(0..7).collect::<Vec<_>>(),
        ));
        let vote_sender = Pubkey::new_unique();
        let cert_sender = Pubkey::new_unique();
        let items = messages_to_items_with_remote_pubkeys(&[
            (vote_message, vote_sender),
            (cert_message, cert_sender),
        ]);

        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 2);
        assert!(!ctx.banlist.is_banned(&vote_sender));
        assert!(!ctx.banlist.is_banned(&cert_sender));
    }

    #[test]
    fn test_banlist_updates_for_invalid_votes() {
        let mut ctx = TestContext::new();

        let vote = Vote::new_skip_vote(42);
        let valid_payload = wincode::serialize(&vote).unwrap();
        let invalid_payload = wincode::serialize(&Vote::new_skip_vote(999)).unwrap();
        let invalid_indexes = [1usize, 3usize];
        let messages: Vec<_> = ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(5)
            .map(|(i, keypair)| {
                let signature = if invalid_indexes.contains(&i) {
                    keypair.bls_keypair.sign(&invalid_payload).into()
                } else {
                    keypair.bls_keypair.sign(&valid_payload).into()
                };
                let message = ConsensusMessage::Vote(VoteMessage {
                    vote,
                    signature,
                    rank: i as u16,
                });
                (message, Pubkey::new_unique())
            })
            .collect();

        ctx.verifier
            .verify_and_send_batches(messages_to_items_with_remote_pubkeys(&messages))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 3);

        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    ctx.banlist.is_banned(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !ctx.banlist.is_banned(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn test_banlist_updates_for_invalid_certificates() {
        let mut ctx = TestContext::new();

        let invalid_indexes = [0usize, 4usize];
        let messages: Vec<_> = (0..5)
            .map(|i| {
                let slot = 10 + i as u64;
                let cert_type = CertificateType::Notarize(slot, Hash::new_unique());
                let mut cert = create_signed_certificate_message(
                    &ctx.validator_keypairs,
                    cert_type,
                    &(0..7).collect::<Vec<_>>(),
                );
                if invalid_indexes.contains(&i) {
                    cert.signature = Signature([0; BLS_SIGNATURE_AFFINE_SIZE]);
                }
                (ConsensusMessage::Certificate(cert), Pubkey::new_unique())
            })
            .collect();

        ctx.verifier
            .verify_and_send_batches(messages_to_items_with_remote_pubkeys(&messages))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().flatten().count(), 3);

        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    ctx.banlist.is_banned(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !ctx.banlist.is_banned(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn generated_certs_are_filtered() {
        let mut ctx = TestContext::new();
        let slot = 1235;
        let cert_type = CertificateType::Skip(slot);
        ctx.generated_cert_types.insert_cert(cert_type);
        let cert = create_signed_certificate_message(
            &ctx.validator_keypairs,
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let items = messages_to_items(&[consensus_message]);
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(ctx.verifier.stats.num_generated_certs_received, 1);
    }

    #[test]
    fn msgs_too_far_in_future_are_dropped() {
        let mut ctx = TestContext::new();
        let slot = ctx.verifier.sharable_banks.root().slot() + NUM_SLOTS_FOR_VERIFY + 1;
        let cert_type = CertificateType::Skip(slot);
        let cert = create_signed_certificate_message(
            &ctx.validator_keypairs,
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            Vote::new_skip_vote(slot),
            0,
        ));
        let items = messages_to_items(&[cert, vote]);
        ctx.verifier.verify_and_send_batches(items).unwrap();
        assert_eq!(ctx.verifier.stats.cert_stats.too_far_in_future, 1);
        assert_eq!(ctx.verifier.stats.vote_stats.too_far_in_future, 1);
    }

    fn messages_to_items(messages: &[ConsensusMessage]) -> Vec<IngressMessage> {
        let messages_with_remote_pubkeys: Vec<_> = messages
            .iter()
            .cloned()
            .map(|message| (message, Pubkey::new_unique()))
            .collect();
        messages_to_items_with_remote_pubkeys(&messages_with_remote_pubkeys)
    }

    fn messages_to_items_with_remote_pubkeys(
        messages: &[(ConsensusMessage, Pubkey)],
    ) -> Vec<IngressMessage> {
        messages
            .iter()
            .map(|(message, remote_pubkey)| message_to_item(message, *remote_pubkey))
            .collect()
    }

    /// Defaults to the datagram transport; tests that exercise the
    /// per-transport metrics use [`message_to_item_with_transport`].
    fn message_to_item(message: &ConsensusMessage, remote_pubkey: Pubkey) -> IngressMessage {
        message_to_item_with_transport(message, remote_pubkey, VotorTransport::Datagram)
    }

    fn message_to_item_with_transport(
        message: &ConsensusMessage,
        remote_pubkey: Pubkey,
        transport: VotorTransport,
    ) -> IngressMessage {
        let bytes =
            bytes::Bytes::from(wincode::serialize(message).expect("serialize ConsensusMessage"));
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        IngressMessage {
            transport,
            datagram: Datagram {
                peer_pubkey: remote_pubkey,
                peer_address: addr,
                message: bytes,
            },
        }
    }
}
