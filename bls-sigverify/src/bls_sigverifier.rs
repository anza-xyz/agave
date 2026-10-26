//! The BLS signature verifier.

use {
    crate::{
        bls_cert_sigverify::{
            CertGroups, CertPayload, MAX_CERTIFICATE_CANDIDATES_PER_TYPE,
            RewardCertificateVerifier, verify_and_send_certificates,
        },
        bls_vote_sigverify::{batch::Batch, verify_and_send_votes},
        errors::SigVerifyError,
        generated_cert_types::GeneratedCertTypes,
        metric_types::ConsensusMetricsEventSender,
        pubkeys::VerifiedVotorSlotsMessage,
        rank_map_cache::RankMapCache,
        rewards::{
            RewardInput, certificate_reward_vote, rewards_wants_certificate, rewards_wants_vote,
        },
        sig_verified_messages::SigVerifiedBatch,
        stats::SigVerifierStats,
        unverified_votes_batch::UnverifiedVotePayload,
        vote_pool::{VotePool, VotePoolError},
    },
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::Block,
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
    /// Verifies reward evidence without blocking this loop.
    reward_verifier: RewardCertificateVerifier,
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
        let reward_verifier = RewardCertificateVerifier::new(
            sharable_banks.clone(),
            cluster_info.clone(),
            leader_schedule.clone(),
            channels.channel_to_reward.clone(),
            ban_sender.clone(),
        );
        Self {
            migration_status,
            ban_sender,
            channels,
            sharable_banks,
            highest_parent_ready,
            stats: SigVerifierStats::default(),
            verified_certs: HashSet::new(),
            reward_verifier,
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
                    &mut self.reward_verifier,
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
        cert_groups: &mut CertGroups,
        cert: UnverifiedCertificate,
        sender_identity_pubkey: Pubkey,
        wants_rewards: bool,
        root_bank: &Bank,
    ) {
        let cert_type = cert.cert_type;
        let payload = CertPayload {
            cert,
            sender_identity_pubkey,
            wants_rewards,
        };
        if cert_type.slot() >= root_bank.slot()
            && !self.verified_certs.contains(&cert_type)
            && !self.generated_cert_types.has_cert(&cert_type)
        {
            cert_groups
                .consensus
                .entry(cert_type)
                .or_default()
                .push(payload);
            return;
        }
        if !wants_rewards {
            if self.verified_certs.contains(&cert_type) {
                self.stats.num_verified_certs_received += 1;
            } else {
                self.stats.num_generated_certs_received += 1;
            }
            return;
        }
        // Consensus already has this type, or never needs it. Copies of a retained signer set and
        // repeats of a batched certificate must not use up the per-type limit. A forged copy of a
        // batched certificate is kept, so it cannot shadow the genuine one. The reward verifier
        // judges whether the remaining candidates improve coverage, on its own thread.
        if self
            .reward_verifier
            .retained()
            .contains(&cert_type, &payload.cert.bitmap)
        {
            self.stats.cert_stats.reward_candidates_filtered += 1;
            return;
        }
        let group = cert_groups.rewards.entry(cert_type).or_default();
        if group.iter().any(|candidate| {
            candidate.cert.bitmap == payload.cert.bitmap
                && candidate.cert.signature == payload.cert.signature
        }) {
            self.stats.cert_stats.reward_candidates_filtered += 1;
            return;
        }
        if group.len() < MAX_CERTIFICATE_CANDIDATES_PER_TYPE {
            group.push(payload);
        } else {
            self.stats.cert_stats.reward_candidate_cap_dropped += 1;
        }
    }

    fn certificate_is_relevant_for_rewards(
        &self,
        my_pubkey: &Pubkey,
        root_bank: &Bank,
        cert: &UnverifiedCertificate,
    ) -> bool {
        rewards_wants_certificate(
            my_pubkey,
            &self.leader_schedule,
            root_bank.slot(),
            &cert.cert_type,
        ) && root_bank
            .get_rank_map(cert.cert_type.slot())
            .is_some_and(|map| {
                certificate_reward_vote(&cert.cert_type, &cert.bitmap, map.len()).is_some()
            })
    }

    fn extract_and_filter_msgs(
        &mut self,
        my_pubkey: &Pubkey,
        datagrams: &[Datagram],
        votes_buffer: &mut HashMap<VotePayloadToSign, Batch>,
        certificates: Vec<(Slot, UnverifiedCertificate)>,
        root_bank: &Bank,
    ) -> CertGroups {
        let root_slot = root_bank.slot();
        let highest_parent_ready_slot = self.highest_parent_ready.read().unwrap().0;
        let max_vote_slot = max_admitted_vote_slot(root_slot, highest_parent_ready_slot);
        let migration_slot = self.migration_status.migration_slot();
        let mut cert_groups = CertGroups::default();
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
                    // Cheap checks first: only certificates that remain need the reward lookup.
                    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                        self.stats.cert_too_far_in_future += 1;
                        continue;
                    }
                    let wants_rewards =
                        self.certificate_is_relevant_for_rewards(my_pubkey, root_bank, &cert);
                    if cert_slot < root_slot && !wants_rewards {
                        self.stats.num_old_certs_received += 1;
                        continue;
                    }
                    self.add_certificate_to_group(
                        &mut cert_groups,
                        cert,
                        *sender_identity_pubkey,
                        wants_rewards,
                        root_bank,
                    );
                }
            }
        }
        for (carrier_slot, certificate) in certificates {
            let cert_slot = certificate.cert_type.slot();
            let is_genesis = matches!(&certificate.cert_type, CertificateType::Genesis(_));
            let is_active = if is_genesis {
                // Genesis certificates from blockstore are only allowed when we are in migration
                self.migration_status.is_in_migration()
            } else {
                self.migration_status
                    .should_allow_block_markers(carrier_slot)
            };
            if certificate.shred_version != my_shred_version || !is_active {
                continue;
            }
            let wants_rewards =
                self.certificate_is_relevant_for_rewards(my_pubkey, root_bank, &certificate);
            // A carrier below the root is kept only for an old certificate that rewards still
            // want, which never reaches the consensus path.
            if carrier_slot < root_slot && (cert_slot >= root_slot || !wants_rewards) {
                continue;
            }
            if cert_slot < root_slot && !wants_rewards {
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
            self.add_certificate_to_group(
                &mut cert_groups,
                certificate,
                sender_identity_pubkey,
                wants_rewards,
                root_bank,
            );
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bls_cert_sigverify::MAX_CERTIFICATE_CANDIDATES_PER_TYPE,
        agave_bls_cert_verify::cert_verify::test_create_base2_unverified_certificate,
        agave_votor_messages::{
            certificate::Certificate, consensus_message::ConsensusMessage,
            reward_certificate::NUM_SLOTS_FOR_REWARD,
        },
        agave_votor_transport::endpoint::stub_ban_channel_for_tests,
        crossbeam_channel::bounded,
        solana_bls_signatures::Keypair as BLSKeypair,
        solana_gossip::contact_info::ContactInfo,
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank::SlotLeader,
            bank_forks::BankForks,
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        },
        solana_signer::Signer,
        std::net::{Ipv4Addr, SocketAddr},
    };

    /// A datagram carrying `cert` from a random peer, as transport delivers it.
    fn certificate_datagram(cert: &UnverifiedCertificate) -> Datagram {
        let message = VersionedWireConsensusMessage::new(
            ConsensusMessage::Certificate(Certificate {
                cert_type: cert.cert_type,
                signature: cert.signature,
                bitmap: cert.bitmap.clone(),
            }),
            cert.shred_version,
        );
        Datagram {
            peer_pubkey: Pubkey::new_unique(),
            peer_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)),
            message: wincode::serialize(&message).unwrap().into(),
        }
    }

    /// A verifier on the leader of `leader_slot`, with its bank forks and the validators' BLS
    /// keypairs.
    fn new_test_verifier(
        leader_slot: Slot,
    ) -> (SigVerifier, Arc<RwLock<BankForks>>, Vec<BLSKeypair>) {
        let validators = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validators, vec![100; 10]);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        let leader = leader_schedule
            .slot_leader_at(leader_slot, None)
            .unwrap()
            .id;
        let node_keypair = validators
            .iter()
            .find(|keys| keys.node_keypair.pubkey() == leader)
            .unwrap()
            .node_keypair
            .insecure_clone();
        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&leader, 0),
            Arc::new(node_keypair),
            SocketAddrSpace::Unspecified,
        ));
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(16);
        let context = SigVerifierContext {
            migration_status: Arc::new(MigrationStatus::post_migration_status()),
            ban_sender,
            sharable_banks,
            highest_parent_ready: Arc::new(RwLock::new((0, Block::new_unique(0)))),
            cluster_info,
            leader_schedule,
            num_threads: 1,
            generated_cert_types: Arc::default(),
        };
        // These tests only filter inputs, so the channels stay unused.
        let (_packet_sender, packet_receiver) = bounded(1);
        let (_certificate_sender, certificate_receiver) = bounded(1);
        let (channel_to_repair, _repair_receiver) = EvictingSender::new_bounded(1);
        let (channel_to_reward, _reward_receiver) = bounded(1);
        let (channel_to_pool, _pool_receiver) = bounded(1);
        let (channel_to_metrics, _metrics_receiver) = bounded(1);
        let verifier = SigVerifier::new(
            context,
            SigVerifierChannels::new(
                packet_receiver,
                certificate_receiver,
                channel_to_repair,
                channel_to_reward,
                channel_to_pool,
                channel_to_metrics,
            ),
        );
        let keypairs = validators
            .iter()
            .map(|keys| keys.bls_keypair.clone())
            .collect();
        (verifier, bank_forks, keypairs)
    }

    #[test]
    fn test_reward_admission_drops_exact_copies_before_the_per_type_limit() {
        let slot = 10;
        // This node publishes the rewards for `slot`.
        let (mut verifier, _bank_forks, keypairs) = new_test_verifier(slot + NUM_SLOTS_FOR_REWARD);
        let my_pubkey = verifier.cluster_info.id();
        let shred_version = verifier.cluster_info.my_shred_version();
        let root_bank = verifier.sharable_banks.root();
        let cert_type = CertificateType::new_unique_notar(slot);
        // A locally generated type needs no consensus proof, so every candidate is reward evidence.
        verifier.generated_cert_types.insert_cert(cert_type);
        let certificate = |ranks: &[usize]| {
            test_create_base2_unverified_certificate(&keypairs, shred_version, cert_type, ranks)
        };
        let forge = |cert: &UnverifiedCertificate, message: &[u8]| {
            let mut forged = cert.clone();
            forged.signature = keypairs[0].sign(message).into();
            forged
        };
        let retained = certificate(&(0..7).collect::<Vec<_>>());
        verifier
            .reward_verifier
            .retained()
            .publish(cert_type, std::slice::from_ref(&retained.bitmap));
        let alternative = certificate(&(1..8).collect::<Vec<_>>());
        let forged_alternative = forge(&alternative, b"forged");
        // Copies of a retained signer set are dropped whatever their signatures.
        let mut certificates = (0..9u8)
            .map(|copy| forge(&retained, &[copy]))
            .collect::<Vec<_>>();
        // A forged copy of a new signer set must not shadow the genuine certificate, while a
        // repeated certificate adds nothing.
        certificates.extend([
            forged_alternative.clone(),
            alternative.clone(),
            alternative.clone(),
        ]);
        // Further signer sets fill the per-type limit, and the next one is dropped.
        let others = (0..MAX_CERTIFICATE_CANDIDATES_PER_TYPE - 1)
            .map(|rank| certificate(&[rank]))
            .collect::<Vec<_>>();
        certificates.extend(others.iter().cloned());

        let carrier_slot = slot + 1;
        let groups = verifier.extract_and_filter_msgs(
            &my_pubkey,
            &[],
            &mut HashMap::new(),
            certificates
                .into_iter()
                .map(|cert| (carrier_slot, cert))
                .collect(),
            &root_bank,
        );
        assert!(groups.consensus.is_empty());
        let admitted = groups.rewards[&cert_type]
            .iter()
            .map(|payload| (&payload.cert.bitmap, &payload.cert.signature))
            .collect::<Vec<_>>();
        let expected = [&forged_alternative, &alternative]
            .into_iter()
            .chain(&others[..MAX_CERTIFICATE_CANDIDATES_PER_TYPE - 2])
            .map(|cert| (&cert.bitmap, &cert.signature))
            .collect::<Vec<_>>();
        assert_eq!(admitted, expected);
        assert_eq!(verifier.stats.cert_stats.reward_candidates_filtered.0, 10);
        assert_eq!(verifier.stats.cert_stats.reward_candidate_cap_dropped.0, 1);
    }

    #[test]
    fn test_old_carrier_adds_only_old_certificates_for_rewards() {
        // The leader of slots 20 to 23 publishes the rewards for slots 12 to 15.
        let (mut verifier, bank_forks, keypairs) = new_test_verifier(20);
        let my_pubkey = verifier.cluster_info.id();
        let shred_version = verifier.cluster_info.my_shred_version();
        let (carrier_slot, root_slot) = (13, 14);
        for slot in [carrier_slot, root_slot] {
            let leader = verifier
                .leader_schedule
                .slot_leader_at(slot + NUM_SLOTS_FOR_REWARD, None);
            assert_eq!(leader.unwrap().id, my_pubkey);
        }
        // Creating a bank can read the bank forks, so it must not hold their lock.
        let root = Bank::new_from_parent(
            verifier.sharable_banks.root(),
            SlotLeader::default(),
            root_slot,
        );
        {
            let mut bank_forks = bank_forks.write().unwrap();
            bank_forks.insert(root);
            bank_forks.set_root(root_slot, None, None);
        }
        let root_bank = verifier.sharable_banks.root();
        assert_eq!(root_bank.slot(), root_slot);
        let certificate = |slot| {
            test_create_base2_unverified_certificate(
                &keypairs,
                shred_version,
                CertificateType::new_unique_notar(slot),
                &(0..7).collect::<Vec<_>>(),
            )
        };
        let old = certificate(carrier_slot);
        // Only a faulty leader's block carries a certificate for a later slot.
        let later = certificate(root_slot);
        let groups = verifier.extract_and_filter_msgs(
            &my_pubkey,
            &[],
            &mut HashMap::new(),
            vec![(carrier_slot, later), (carrier_slot, old.clone())],
            &root_bank,
        );
        // As before, a carrier below the root adds nothing to the consensus path.
        assert!(groups.consensus.is_empty());
        assert_eq!(groups.rewards.keys().collect::<Vec<_>>(), [&old.cert_type]);
    }

    #[test]
    fn test_reward_recovery_admits_evidence_without_changing_consensus_admission() {
        // The leader of slots 20 to 23 publishes the rewards for slots 12 to 15.
        let (mut verifier, bank_forks, keypairs) = new_test_verifier(20);
        let my_pubkey = verifier.cluster_info.id();
        let shred_version = verifier.cluster_info.my_shred_version();
        let (old_slot, root_slot, later_slot) = (13, 14, 15);
        for slot in [old_slot, later_slot] {
            let leader = verifier
                .leader_schedule
                .slot_leader_at(slot + NUM_SLOTS_FOR_REWARD, None);
            assert_eq!(leader.unwrap().id, my_pubkey);
        }
        let root = Bank::new_from_parent(
            verifier.sharable_banks.root(),
            SlotLeader::default(),
            root_slot,
        );
        {
            let mut bank_forks = bank_forks.write().unwrap();
            bank_forks.insert(root);
            bank_forks.set_root(root_slot, None, None);
        }
        let root_bank = verifier.sharable_banks.root();
        let certificate = |cert_type| {
            test_create_base2_unverified_certificate(
                &keypairs,
                shred_version,
                cert_type,
                &(0..7).collect::<Vec<_>>(),
            )
        };
        // Only rewards could use these: two are below the root, and consensus never needs a
        // locally generated type. Each path gets its own pair.
        let reward_only_pair = || {
            let generated = CertificateType::new_unique_notar(later_slot);
            verifier.generated_cert_types.insert_cert(generated);
            (
                certificate(CertificateType::new_unique_notar(old_slot)),
                certificate(generated),
            )
        };
        let (old, reward_only) = reward_only_pair();
        let (old_datagram, reward_only_datagram) = reward_only_pair();
        let consensus = certificate(CertificateType::new_unique_notar(later_slot));
        let groups = verifier.extract_and_filter_msgs(
            &my_pubkey,
            &[
                certificate_datagram(&old_datagram),
                certificate_datagram(&reward_only_datagram),
            ],
            &mut HashMap::new(),
            vec![
                (old_slot, old.clone()),
                (later_slot, reward_only.clone()),
                (later_slot, consensus.clone()),
            ],
            &root_bank,
        );
        let rewards = groups.rewards.keys().copied().collect::<HashSet<_>>();
        let stats = &verifier.stats;
        assert_eq!(
            rewards,
            HashSet::from([
                old.cert_type,
                reward_only.cert_type,
                old_datagram.cert_type,
                reward_only_datagram.cert_type,
            ])
        );
        assert_eq!(stats.num_old_certs_received.0, 0);
        assert_eq!(stats.num_generated_certs_received.0, 0);
        // The first consensus proof is admitted and also marked as reward evidence.
        assert_eq!(groups.consensus.len(), 1);
        let candidates = &groups.consensus[&consensus.cert_type];
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].wants_rewards);
    }
}
