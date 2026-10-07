//! The BLS signature verifier.

use {
    crate::{
        bls_cert_sigverify::{CertPayload, verify_and_send_certificates},
        bls_vote_sigverify::{batch::Batch, verify_and_send_votes},
        errors::SigVerifyError,
        generated_cert_types::GeneratedCertTypes,
        rank_map_cache::RankMapCache,
        rewards::{RewardInput, rewards_wants_vote},
        sig_verified_messages::SigVerifiedBatch,
        stats::SigVerifierStats,
        unverified_votes_batch::UnverifiedVotePayload,
        vote_pool::{VotePool, VotePoolError},
    },
    agave_votor_messages::{
        VerifiedVotorSlotsMessage,
        certificate::CertificateType,
        consensus_message::Block,
        metric_types::ConsensusMetricsEventSender,
        migration::MigrationStatus,
        unverified_vote_message::{
            DecodedWireConsensusMessage, UnverifiedCertificate, UnverifiedVoteMessage,
        },
        vote::Vote,
        wire::{VersionedWireConsensusMessage, VotePayloadToSign},
    },
    agave_votor_transport::endpoint::{BanSender, Datagram},
    crossbeam_channel::{Receiver, Sender, TryRecvError, select},
    log::{error, info},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_perf::packet::packet_config,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        cmp,
        collections::{HashMap, HashSet, hash_map::Entry},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder},
        time::Duration,
    },
};

/// If a certificate is so many slots in the future relative to the root slot, it is considered
/// invalid and discarded.
///
/// At 200ms slot times, 30K slots is 100mins.  We do not expect a node to catch up if it has
/// fallen so far behind.
pub const NUM_SLOTS_FOR_VERIFY: Slot = 30_000;

/// Votes further ahead of the highest ParentReady slot are discarded to bound vote tracking
/// memory while still allowing enough lookahead to maintain liveness.
const MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY: Slot = 40;

/// Maximum distance from the root at which a vote can be admitted.
///
/// Let's say root slot is as R.  We are willing to accept certs for upto R + [`NUM_SLOTS_FOR_VERIFY`]
/// slots into the future.  A cert at slot S can produce a parent ready event for the slot S+1, hence
/// the +1.  And then we are willing to accepts votes for another
/// [`MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY`] slots.
pub const MAX_VOTE_SLOT_DISTANCE_FROM_ROOT: Slot =
    NUM_SLOTS_FOR_VERIFY + 1 + MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY;

fn max_admitted_vote_slot(root_slot: Slot, highest_parent_ready_slot: Slot) -> Slot {
    root_slot
        .max(highest_parent_ready_slot)
        .saturating_add(MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY)
        .min(root_slot.saturating_add(MAX_VOTE_SLOT_DISTANCE_FROM_ROOT))
}

/// If we receive an invalid certificate or vote from a QUIC connection, we ban the sender.
/// We ban the sender for 10 seconds which prevents DoS but allows for recovery in case of instability.
pub(super) const BAN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct SigVerifierContext {
    pub migration_status: Arc<MigrationStatus>,
    /// Sends peer ban commands to the transport endpoint.
    pub ban_sender: BanSender,
    pub sharable_banks: SharableBanks,
    pub highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule: Arc<LeaderScheduleCache>,
    pub num_threads: usize,
    pub generated_cert_types: Arc<GeneratedCertTypes>,
}

pub struct SigVerifierChannels {
    pub(crate) packet_receiver: Receiver<Datagram>,
    pub(crate) certificate_receiver: Receiver<(Slot, UnverifiedCertificate)>,
    pub(crate) channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
    pub(crate) channel_to_reward: Sender<RewardInput>,
    pub(crate) channel_to_pool: Sender<SigVerifiedBatch>,
    pub(crate) channel_to_metrics: ConsensusMetricsEventSender,
}

impl SigVerifierChannels {
    pub fn new(
        packet_receiver: Receiver<Datagram>,
        certificate_receiver: Receiver<(Slot, UnverifiedCertificate)>,
        channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
        channel_to_reward: Sender<RewardInput>,
        channel_to_pool: Sender<SigVerifiedBatch>,
        channel_to_metrics: ConsensusMetricsEventSender,
    ) -> Self {
        Self {
            packet_receiver,
            certificate_receiver,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
        }
    }
}

/// Starts the BLS sigverifier service in its own dedicated thread.
pub fn spawn_service(
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
    ban_sender: BanSender,
    channels: SigVerifierChannels,
    /// Container to look up root banks from.
    sharable_banks: SharableBanks,
    highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
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
    vote_pool: VotePool,
    rank_map_cache: RankMapCache,
}

impl SigVerifier {
    fn new(context: SigVerifierContext, channels: SigVerifierChannels) -> Self {
        let SigVerifierContext {
            migration_status,
            ban_sender,
            sharable_banks,
            highest_parent_ready,
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
        Self {
            migration_status,
            ban_sender,
            channels,
            sharable_banks,
            highest_parent_ready,
            stats: SigVerifierStats::default(),
            verified_certs: HashSet::new(),
            vote_pool: VotePool::default(),
            last_checked_root_slot: 0,
            cluster_info,
            leader_schedule,
            thread_pool,
            generated_cert_types,
            rank_map_cache: RankMapCache::default(),
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>) {
        let mut datagrams_buffer = Vec::new();
        let mut votes_buffer = HashMap::new();
        while !exit.load(Ordering::Relaxed) {
            const SOFT_RECEIVE_CAP: usize = 5000;
            datagrams_buffer.clear();
            votes_buffer.clear();
            let Ok(certificates) = recv_inputs(
                &self.channels.packet_receiver,
                &self.channels.certificate_receiver,
                SOFT_RECEIVE_CAP,
                &mut datagrams_buffer,
            ) else {
                error!("sigverifier input channel disconnected: Exiting.");
                break;
            };
            if self.migration_status.is_pre_feature_activation() {
                continue;
            }
            self.stats.maybe_report(self.sharable_banks.root().slot());
            if datagrams_buffer.is_empty() && certificates.is_empty() {
                continue;
            }

            let (verify_res, verify_time_us) = measure_us!(self.verify_and_send_inputs(
                &self.cluster_info.id(),
                &datagrams_buffer,
                &mut votes_buffer,
                certificates
            ));
            self.stats
                .verify_and_send_batch_us
                .add_sample(verify_time_us);
            if let Err(e) = verify_res {
                error!("verify_and_send_batch() failed with {e}. Exiting.");
                break;
            }
        }
        self.stats.do_report(self.sharable_banks.root().slot());
    }

    fn verify_and_send_inputs(
        &mut self,
        my_pubkey: &Pubkey,
        datagrams: &[Datagram],
        votes_buffer: &mut HashMap<VotePayloadToSign, Batch>,
        certificates: Vec<(Slot, UnverifiedCertificate)>,
    ) -> Result<(), SigVerifyError> {
        let root_bank = self.sharable_banks.root();
        self.maybe_prune_caches(&root_bank);

        let (certs, extract_msgs_us) = measure_us!(self.extract_and_filter_msgs(
            my_pubkey,
            datagrams,
            votes_buffer,
            certificates,
            &root_bank
        ));
        self.stats
            .extract_filter_msgs_us
            .add_sample(extract_msgs_us);

        let (votes_result, certs_result) = self.thread_pool.join(
            || {
                verify_and_send_votes(
                    votes_buffer,
                    &root_bank,
                    my_pubkey,
                    &self.leader_schedule,
                    &self.ban_sender,
                    &self.thread_pool,
                    &self.channels,
                )
            },
            || {
                verify_and_send_certificates(
                    my_pubkey,
                    &mut self.verified_certs,
                    certs,
                    &root_bank,
                    &self.channels.channel_to_pool,
                    &self.ban_sender,
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

    fn maybe_prune_caches(&mut self, root_bank: &Bank) {
        let root_slot = root_bank.slot();
        let root_epoch = root_bank.epoch();
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() >= root_slot);
            self.vote_pool.prune(root_slot);
        }
        self.rank_map_cache.purge(root_epoch);
    }

    fn add_certificate_to_group(
        &mut self,
        cert_groups: &mut HashMap<CertificateType, Vec<CertPayload>>,
        cert: UnverifiedCertificate,
        sender_identity_pubkey: Pubkey,
    ) {
        if self.verified_certs.contains(&cert.cert_type) {
            self.stats.num_verified_certs_received += 1;
            return;
        }
        if self.generated_cert_types.has_cert(&cert.cert_type) {
            self.stats.num_generated_certs_received += 1;
            return;
        }
        cert_groups
            .entry(cert.cert_type)
            .or_default()
            .push(CertPayload {
                cert,
                sender_identity_pubkey,
            });
    }

    fn extract_and_filter_msgs(
        &mut self,
        my_pubkey: &Pubkey,
        datagrams: &[Datagram],
        votes_buffer: &mut HashMap<VotePayloadToSign, Batch>,
        certificates: Vec<(Slot, UnverifiedCertificate)>,
        root_bank: &Bank,
    ) -> HashMap<CertificateType, Vec<CertPayload>> {
        let root_slot = root_bank.slot();
        let highest_parent_ready_slot = self.highest_parent_ready.read().unwrap().0;
        let max_vote_slot = max_admitted_vote_slot(root_slot, highest_parent_ready_slot);
        let migration_slot = self.migration_status.migration_slot();
        let mut cert_groups = HashMap::<CertificateType, Vec<CertPayload>>::new();
        let mut num_pkts = 0u64;
        let my_shred_version = self.cluster_info.my_shred_version();
        for Datagram {
            peer_pubkey: sender_identity_pubkey,
            message,
            ..
        } in datagrams
        {
            num_pkts = num_pkts.saturating_add(1);
            let Ok(msg) = VersionedWireConsensusMessage::deserialize_with_expected_shred_version(
                message.as_ref(),
                packet_config(),
                my_shred_version,
            ) else {
                self.stats.num_malformed_pkts += 1;
                continue;
            };
            let decoded_msg = DecodedWireConsensusMessage::new(msg);

            match decoded_msg {
                DecodedWireConsensusMessage::Vote(unverified_vote) => {
                    self.extract_and_filter_vote(
                        my_pubkey,
                        *sender_identity_pubkey,
                        migration_slot,
                        max_vote_slot,
                        root_bank,
                        votes_buffer,
                        unverified_vote,
                    );
                }
                DecodedWireConsensusMessage::Certificate(cert) => {
                    let cert_slot = cert.cert_type.slot();
                    if cert_slot < root_slot {
                        self.stats.num_old_certs_received += 1;
                        continue;
                    }
                    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                        self.stats.cert_too_far_in_future += 1;
                        continue;
                    }
                    self.add_certificate_to_group(&mut cert_groups, cert, *sender_identity_pubkey);
                }
            }
        }
        for (carrier_slot, certificate) in certificates {
            let is_genesis = matches!(&certificate.cert_type, CertificateType::Genesis(_));
            let is_active = if is_genesis {
                // Genesis certificates from blockstore are only allowed when we are in migration
                self.migration_status.is_in_migration()
            } else {
                self.migration_status
                    .should_allow_block_markers(carrier_slot)
            };
            if carrier_slot < root_slot
                || certificate.shred_version != my_shred_version
                || !is_active
            {
                continue;
            }
            let cert_slot = certificate.cert_type.slot();
            if cert_slot < root_slot {
                self.stats.num_old_certs_received += 1;
                continue;
            }
            if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                self.stats.cert_too_far_in_future += 1;
                continue;
            }
            let Some(sender_identity_pubkey) = self
                .leader_schedule
                .slot_leader_at(carrier_slot, Some(root_bank))
                .map(|leader| leader.id)
            else {
                continue;
            };
            self.add_certificate_to_group(&mut cert_groups, certificate, sender_identity_pubkey);
        }
        self.stats.num_pkts += num_pkts;
        cert_groups
    }

    fn extract_and_filter_vote(
        &mut self,
        my_pubkey: &Pubkey,
        sender_identity_pubkey: Pubkey,
        migration_slot: Option<Slot>,
        max_vote_slot: Slot,
        root_bank: &Bank,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        unverified_vote: UnverifiedVoteMessage,
    ) {
        // votes from self take a different pathway.
        if &sender_identity_pubkey == my_pubkey {
            self.stats.num_keep_vote_failed += 1;
            return;
        }
        let root_slot = root_bank.slot();
        let vote_slot = unverified_vote.vote.slot();
        let is_in_range = match unverified_vote.vote {
            // Genesis votes bypass the normal range check, instead we require that they are only accepted during the
            // migration epoch and less than the migration slot
            Vote::Genesis(_) => {
                migration_slot.is_some_and(|migration_slot| vote_slot < migration_slot)
            }
            _ => vote_slot <= max_vote_slot,
        };
        if !is_in_range {
            self.stats.vote_too_far_in_future += 1;
            self.stats.num_keep_vote_failed += 1;
            return;
        }

        match vote_slot.cmp(&root_slot) {
            // Genesis votes are allowed on the root slot
            cmp::Ordering::Equal if unverified_vote.vote.is_genesis_vote() => (),
            // Votes are allowed at or below the root if they are useful for rewards
            cmp::Ordering::Less | cmp::Ordering::Equal => {
                if !rewards_wants_vote(
                    my_pubkey,
                    &self.leader_schedule,
                    root_slot,
                    &unverified_vote.vote,
                ) {
                    self.stats.num_old_votes_received += 1;
                    self.stats.num_keep_vote_failed += 1;
                    return;
                }
            }
            // Votes above the root are always allowed
            cmp::Ordering::Greater => (),
        }

        let vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(unverified_vote.vote, unverified_vote.shred_version);
        match votes.entry(vote_payload_to_sign) {
            Entry::Vacant(e) => {
                let vote_slot = unverified_vote.vote.slot();
                let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
                let Some(rank_map) = self.rank_map_cache.get_rank_map(root_bank, vote_epoch) else {
                    self.stats.discard_vote_no_epoch_stakes += 1;
                    self.stats.num_keep_vote_failed += 1;
                    return;
                };
                match self.keep_vote(&rank_map, unverified_vote, sender_identity_pubkey) {
                    Some((payload, sender_vote_account_pubkey)) => {
                        let batch = Batch::new(
                            vote_payload_to_sign,
                            payload,
                            sender_vote_account_pubkey,
                            rank_map,
                        );
                        e.insert(batch);
                    }
                    None => {
                        self.stats.num_keep_vote_failed += 1;
                    }
                }
            }
            Entry::Occupied(mut e) => {
                let batch = e.get_mut();
                match self.keep_vote(batch.rank_map(), unverified_vote, sender_identity_pubkey) {
                    Some((payload, sender_vote_account_pubkey)) => {
                        batch.push(payload, sender_vote_account_pubkey);
                    }
                    None => {
                        self.stats.num_keep_vote_failed += 1;
                    }
                }
            }
        }
    }

    /// If this vote should be verified, then returns the [`UnverifiedVotePayload`].
    fn keep_vote(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        msg: UnverifiedVoteMessage,
        sender_identity_pubkey: Pubkey,
    ) -> Option<(UnverifiedVotePayload, Pubkey)> {
        let (rank, entry) = rank_map
            .get_ranked_entry_for_node(&sender_identity_pubkey)
            .or_else(|| {
                self.stats.discard_vote_invalid_rank += 1;
                None
            })?;
        match self.vote_pool.try_add_vote(&msg, rank, rank_map.len()) {
            Ok(()) => Some((
                UnverifiedVotePayload {
                    vote_message: msg,
                    sender_bls_pubkey: entry.bls_pubkey,
                    sender_identity_pubkey,
                    stake: entry.stake,
                    rank,
                },
                entry.vote_account_pubkey,
            )),
            Err(VotePoolError::Duplicate) => {
                self.stats.vote_pool_duplicate += 1;
                None
            }
            Err(VotePoolError::Invalid) => {
                self.stats.invalid_vote_banning_validator += 1;
                self.ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
                info!(
                    "bls_sigverifier: banned sender={sender_identity_pubkey} due to invalid vote"
                );
                None
            }
        }
    }
}

/// Receives BLS datagrams and certificates recovered from blockstore. Certificate-only
/// traffic wakes the verifier immediately; datagrams retain their existing soft receive cap.
fn recv_inputs(
    packet_receiver: &Receiver<Datagram>,
    certificate_receiver: &Receiver<(Slot, UnverifiedCertificate)>,
    soft_receive_cap: usize,
    datagrams_buffer: &mut Vec<Datagram>,
) -> Result<Vec<(Slot, UnverifiedCertificate)>, ()> {
    let mut certificates = vec![];
    select! {
        recv(packet_receiver) -> datagram => {
            datagrams_buffer.push(datagram.map_err(|_| ())?);
        }
        recv(certificate_receiver) -> certificate => {
            certificates.push(certificate.map_err(|_| ())?);
        },
        default(Duration::from_secs(1)) => return Ok(certificates),
    }
    while datagrams_buffer.len() < soft_receive_cap {
        match packet_receiver.try_recv() {
            Ok(datagram) => {
                datagrams_buffer.push(datagram);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
    // Certificates from blockstore are very low throughput (1 per slot), so no need for a cap here
    certificates.extend(certificate_receiver.try_iter());
    Ok(certificates)
}
