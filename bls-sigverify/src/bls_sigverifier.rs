//! The BLS signature verifier.

use {
    crate::{
        bls_cert_sigverify::{CertPayload, verify_and_send_certificates},
        bls_vote_sigverify::{UnverifiedVotePayload, verify_and_send_votes},
        errors::SigVerifyError,
        generated_cert_types::GeneratedCertTypes,
        rewards::{RewardInput, rewards_wants_vote},
        stats::SigVerifierStats,
        vote_pool::{VotePool, VotePoolError},
    },
    agave_votor_messages::{
        VerifiedVoterSlotsSender,
        certificate::CertificateType,
        consensus_message::Block,
        metric_types::ConsensusMetricsEventSender,
        migration::MigrationStatus,
        sig_verified_messages::SigVerifiedBatch,
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
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_perf::packet::packet_config,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
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

fn max_admitted_vote_slot(root_slot: Slot, highest_parent_ready_slot: Slot) -> Slot {
    cmp::max(root_slot, highest_parent_ready_slot)
        .saturating_add(MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY)
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
    pub(crate) channel_to_repair: VerifiedVoterSlotsSender,
    pub(crate) channel_to_reward: Sender<RewardInput>,
    pub(crate) channel_to_pool: Sender<SigVerifiedBatch>,
    pub(crate) channel_to_metrics: ConsensusMetricsEventSender,
}

impl SigVerifierChannels {
    pub fn new(
        packet_receiver: Receiver<Datagram>,
        certificate_receiver: Receiver<(Slot, UnverifiedCertificate)>,
        channel_to_repair: VerifiedVoterSlotsSender,
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

struct ExtractedMsgs {
    certs: HashMap<CertificateType, Vec<CertPayload>>,
    votes: HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>>,
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
    last_checked_root_epoch: Epoch,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    /// thread pool to use for all parallel tasks
    thread_pool: ThreadPool,
    generated_cert_types: Arc<GeneratedCertTypes>,
    vote_pool: VotePool,
    rank_map_cache: HashMap<Epoch, Arc<BLSPubkeyToRankMap>>,
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
        let root_slot = sharable_banks.root().slot();
        Self {
            migration_status,
            ban_sender,
            channels,
            sharable_banks,
            highest_parent_ready,
            stats: SigVerifierStats::new(root_slot),
            verified_certs: HashSet::new(),
            vote_pool: VotePool::default(),
            last_checked_root_slot: 0,
            last_checked_root_epoch: 0,
            cluster_info,
            leader_schedule,
            thread_pool,
            generated_cert_types,
            rank_map_cache: HashMap::new(),
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>) {
        let mut datagrams_buffer = Vec::new();
        while !exit.load(Ordering::Relaxed) {
            const SOFT_RECEIVE_CAP: usize = 5000;
            datagrams_buffer.clear();
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
            if datagrams_buffer.is_empty() && certificates.is_empty() {
                continue;
            }

            let (verify_res, verify_time_us) =
                measure_us!(self.verify_and_send_inputs(&datagrams_buffer, certificates));
            self.stats
                .verify_and_send_batch_us
                .add_sample(verify_time_us);
            if let Err(e) = verify_res {
                error!("verify_and_send_batch() failed with {e}. Exiting.");
                break;
            }
            self.stats.maybe_report(self.sharable_banks.root().slot());
        }
        let elapsed = self.stats.elapsed_since_last_report();
        self.stats
            .do_report(self.sharable_banks.root().slot(), elapsed);
    }

    #[cfg(test)]
    fn verify_and_send_datagrams(
        &mut self,
        datagrams: Vec<Datagram>,
    ) -> Result<(), SigVerifyError> {
        self.verify_and_send_inputs(&datagrams, vec![])
    }

    fn verify_and_send_inputs(
        &mut self,
        datagrams: &[Datagram],
        certificates: Vec<(Slot, UnverifiedCertificate)>,
    ) -> Result<(), SigVerifyError> {
        let root_bank = self.sharable_banks.root();
        self.maybe_prune_caches(&root_bank);

        let (extracted_msgs, extract_msgs_us) =
            measure_us!(self.extract_and_filter_msgs(datagrams, certificates, &root_bank));
        self.stats
            .extract_filter_msgs_us
            .add_sample(extract_msgs_us);

        let (votes_result, certs_result) = self.thread_pool.join(
            || {
                verify_and_send_votes(
                    extracted_msgs.votes,
                    &self.rank_map_cache,
                    &root_bank,
                    &self.cluster_info,
                    &self.leader_schedule,
                    &self.ban_sender,
                    &self.thread_pool,
                    &self.channels,
                )
            },
            || {
                verify_and_send_certificates(
                    &self.cluster_info.id(),
                    &mut self.verified_certs,
                    extracted_msgs.certs,
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
        if self.last_checked_root_epoch < root_epoch {
            self.last_checked_root_epoch = root_epoch;
            // Keeping previous epoch as we need to look up slots older than root_slot for rewards.
            self.rank_map_cache
                .retain(|epoch, _| *epoch >= root_epoch.saturating_sub(1));
        }
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
        datagrams: &[Datagram],
        certificates: Vec<(Slot, UnverifiedCertificate)>,
        root_bank: &Bank,
    ) -> ExtractedMsgs {
        let root_slot = root_bank.slot();
        let highest_parent_ready_slot = self.highest_parent_ready.read().unwrap().0;
        let max_vote_slot = max_admitted_vote_slot(root_slot, highest_parent_ready_slot);
        let migration_slot = self.migration_status.migration_slot();
        let mut cert_groups = HashMap::<CertificateType, Vec<CertPayload>>::new();
        let mut votes: HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>> = HashMap::new();
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
                    if let Some(payload) = self.keep_vote(
                        unverified_vote,
                        *sender_identity_pubkey,
                        root_bank,
                        max_vote_slot,
                        migration_slot,
                    ) {
                        let vote_payload_to_sign = VotePayloadToSign::new_from_vote(
                            payload.vote_message.vote,
                            payload.vote_message.shred_version,
                        );
                        votes.entry(vote_payload_to_sign).or_default().push(payload);
                    } else {
                        self.stats.num_keep_vote_failed += 1;
                    }
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
        ExtractedMsgs {
            certs: cert_groups,
            votes,
        }
    }

    /// If this vote should be verified, then returns the [`UnverifiedVotePayload`].
    fn keep_vote(
        &mut self,
        msg: UnverifiedVoteMessage,
        sender_identity_pubkey: Pubkey,
        root_bank: &Bank,
        max_vote_slot: Slot,
        migration_slot: Option<Slot>,
    ) -> Option<UnverifiedVotePayload> {
        // votes from self take a different pathway.
        if sender_identity_pubkey == self.cluster_info.id() {
            return None;
        }
        let root_slot = root_bank.slot();
        let vote_slot = msg.vote.slot();
        let is_in_range = match msg.vote {
            // Genesis votes bypass the normal range check, instead we require that they are only accepted during the
            // migration epoch and less than the migration slot
            Vote::Genesis(_) => {
                migration_slot.is_some_and(|migration_slot| vote_slot < migration_slot)
            }
            _ => vote_slot <= max_vote_slot,
        };
        if !is_in_range {
            self.stats.vote_too_far_in_future += 1;
            return None;
        }

        match vote_slot.cmp(&root_slot) {
            // Genesis votes are allowed on the root slot
            cmp::Ordering::Equal if msg.vote.is_genesis_vote() => (),
            // Votes are allowed at or below the root if they are useful for rewards
            cmp::Ordering::Less | cmp::Ordering::Equal => {
                if !rewards_wants_vote(
                    &self.cluster_info,
                    &self.leader_schedule,
                    root_slot,
                    &msg.vote,
                ) {
                    self.stats.num_old_votes_received += 1;
                    return None;
                }
            }
            // Votes above the root are always allowed
            cmp::Ordering::Greater => (),
        }

        let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
        let rank_map = match self.rank_map_cache.entry(vote_epoch) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                    self.stats.discard_vote_no_epoch_stakes += 1;
                    return None;
                };
                entry.insert(rank_map.clone())
            }
        };
        let (rank, entry) = rank_map
            .get_ranked_entry_for_node(&sender_identity_pubkey)
            .or_else(|| {
                self.stats.discard_vote_invalid_rank += 1;
                None
            })?;
        match self.vote_pool.try_add_vote(&msg, rank, rank_map.len()) {
            Ok(()) => Some(UnverifiedVotePayload {
                vote_message: msg,
                sender_bls_pubkey: entry.bls_pubkey,
                sender_vote_account_pubkey: entry.vote_account_pubkey,
                sender_identity_pubkey,
                stake: entry.stake,
                rank,
            }),
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
        agave_bls_cert_verify::cert_verify::{
            test_create_base2_certificate, test_create_base2_unverified_certificate,
            test_create_base3_certificate,
        },
        agave_votor_messages::{
            VerifiedVoterSlotsReceiver,
            certificate::{Certificate, CertificateType},
            consensus_message::{Block, ConsensusMessage, VoteMessage},
            metric_types::ConsensusMetricsEventReceiver,
            sig_verified_messages::VoteAggregate,
            vote::Vote,
            wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
        },
        agave_votor_transport::endpoint::{BanCommand, stub_ban_channel_for_tests},
        bitvec::prelude::{BitVec, Lsb0},
        bytes::Bytes,
        crossbeam_channel::{Receiver, TryRecvError, bounded},
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, Keypair as BLSKeypair, Signature, signature::SignatureAffine,
        },
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
        std::{
            net::{Ipv4Addr, SocketAddr},
            num::NonZero,
            sync::RwLock,
        },
        tokio::sync::mpsc,
    };

    fn new_vote_aggregate(bank: &Bank, mut msg: VoteMessage) -> VoteAggregate {
        let rank_map = bank
            .epoch_stakes_from_slot(msg.vote.slot())
            .unwrap()
            .bls_pubkey_to_rank_map();
        msg.stake = rank_map
            .get_pubkey_stake_entry(msg.rank as usize)
            .unwrap()
            .stake;
        let max_validators = rank_map.len();
        VoteAggregate::new_from_verified_vote(max_validators, msg)
    }

    struct TestContext {
        verifier: SigVerifier,
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        ban_receiver: mpsc::Receiver<BanCommand>,
        _packet_sender: Sender<Datagram>,
        repair_receiver: VerifiedVoterSlotsReceiver,
        _reward_receiver: Receiver<RewardInput>,
        pool_receiver: Receiver<SigVerifiedBatch>,
        _metrics_receiver: ConsensusMetricsEventReceiver,
        generated_cert_types: Arc<GeneratedCertTypes>,
        _certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
        _bank_forks: Arc<RwLock<BankForks>>,
    }

    impl TestContext {
        fn new() -> Self {
            let (channel_to_pool, pool_receiver) = bounded(1024);
            Self::new_with_pool_channel(channel_to_pool, pool_receiver)
        }

        /// Drain pending ban requests and collect the banned pubkeys.
        fn banned_pubkeys(&mut self) -> HashSet<Pubkey> {
            let mut banned = HashSet::new();
            while let Ok(BanCommand { peer, .. }) = self.ban_receiver.try_recv() {
                banned.insert(peer);
            }
            banned
        }

        fn new_with_pool_channel(
            channel_to_pool: Sender<SigVerifiedBatch>,
            pool_receiver: Receiver<SigVerifiedBatch>,
        ) -> Self {
            let num_validators = 10;
            let validator_keypairs = (0..num_validators)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let stakes_vec = (0..validator_keypairs.len())
                .map(|i| 1_000u64.saturating_sub(i as u64))
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

            let (channel_to_repair, repair_receiver) = bounded(1024);
            let (channel_to_reward, reward_receiver) = bounded(1024);
            let (packet_sender, packet_receiver) = bounded(1024);
            let (certificate_sender, certificate_receiver) = bounded(1024);
            let (channel_to_metrics, metrics_receiver) = bounded(1024);

            let generated_cert_types = Arc::new(GeneratedCertTypes::default());
            let (ban_sender, ban_receiver) = stub_ban_channel_for_tests(1024);
            let highest_parent_ready = Arc::new(RwLock::new((
                NUM_SLOTS_FOR_VERIFY,
                Block::new_unique(NUM_SLOTS_FOR_VERIFY.saturating_sub(1)),
            )));
            let verifier = SigVerifier::new(
                SigVerifierContext {
                    migration_status: Arc::new(MigrationStatus::default()),
                    ban_sender,
                    sharable_banks,
                    highest_parent_ready,
                    cluster_info,
                    leader_schedule,
                    num_threads: 4,
                    generated_cert_types: generated_cert_types.clone(),
                },
                SigVerifierChannels::new(
                    packet_receiver,
                    certificate_receiver,
                    channel_to_repair,
                    channel_to_reward,
                    channel_to_pool,
                    channel_to_metrics,
                ),
            );
            Self {
                validator_keypairs,
                verifier,
                ban_receiver,
                _packet_sender: packet_sender,
                repair_receiver,
                _reward_receiver: reward_receiver,
                pool_receiver,
                _metrics_receiver: metrics_receiver,
                generated_cert_types,
                _certificate_sender: certificate_sender,
                _bank_forks: bank_forks,
            }
        }

        fn bls_keypairs(&self) -> Vec<BLSKeypair> {
            self.validator_keypairs
                .iter()
                .map(|k| k.bls_keypair.clone())
                .collect()
        }
    }

    fn create_signed_vote_message(
        root_bank: &Bank,
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let rank_map = root_bank.get_rank_map(vote.slot()).unwrap();
        let stake = rank_map.get_pubkey_stake_entry(rank).unwrap().stake;
        create_signed_vote_message_with_stake(validator_keypairs, shred_version, vote, rank, stake)
    }

    fn create_signed_vote_message_with_stake(
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        vote: Vote,
        rank: usize,
        stake: NonZero<u64>,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let payload = get_vote_payload_to_sign(vote, shred_version);
        let signature = SignatureAffine::from(bls_keypair.sign(&payload));
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
            stake,
        }
    }

    fn expect_no_receive<T: std::fmt::Debug>(receiver: &Receiver<T>) {
        match receiver.try_recv().unwrap_err() {
            TryRecvError::Empty => (),
            e => {
                panic!("unexpected error {e:?}");
            }
        }
    }

    /// Builds a fake datagram carrying `message`, matching what transport would deliver to us.
    fn message_to_datagram(
        message: &ConsensusMessage,
        shred_version: u16,
        peer_pubkey: Pubkey,
    ) -> Datagram {
        let msg = VersionedWireConsensusMessage::new(message.clone(), shred_version);
        datagram_from_bytes(wincode::serialize(&msg).unwrap(), peer_pubkey)
    }

    fn datagram_from_bytes(message: impl Into<Bytes>, peer_pubkey: Pubkey) -> Datagram {
        Datagram {
            peer_pubkey,
            peer_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)), // this does not bind
            message: message.into(),
        }
    }

    #[test]
    fn test_blockstore_certificate_requires_active_alpenglow() {
        let mut ctx = TestContext::new();
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let block = Block::new_unique(1);
        let certificate = test_create_base2_unverified_certificate(
            &ctx.bls_keypairs(),
            shred_version,
            CertificateType::FinalizeFast(block),
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        let slot = 2;

        ctx.verifier
            .verify_and_send_inputs(&[], vec![(slot, certificate.clone())])
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);

        ctx.verifier.migration_status.enable_alpenglow_for_tests();
        ctx.verifier
            .verify_and_send_inputs(&[], vec![(slot, certificate)])
            .unwrap();
        let SigVerifiedBatch::Certificates(certs) = ctx.pool_receiver.try_recv().unwrap() else {
            panic!("expected a certificate batch");
        };
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].cert_type, CertificateType::FinalizeFast(block));
    }

    #[test]
    fn test_old_blockstore_certificate_is_filtered() {
        let mut ctx = TestContext::new();
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let block = Block::new_unique(1);
        let certificate = test_create_base2_unverified_certificate(
            &ctx.bls_keypairs(),
            shred_version,
            CertificateType::FinalizeFast(block),
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        let slot = 6;
        let root_bank =
            Bank::new_from_parent(ctx.verifier.sharable_banks.root(), SlotLeader::default(), 5);
        ctx.verifier.migration_status.enable_alpenglow_for_tests();

        let extracted_msgs =
            ctx.verifier
                .extract_and_filter_msgs(&[], vec![(slot, certificate)], &root_bank);
        assert!(extracted_msgs.certs.is_empty());
        assert!(extracted_msgs.votes.is_empty());
        assert_eq!(ctx.verifier.stats.num_old_certs_received.0, 1);
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let mut ctx = TestContext::new();

        let vote_rank1 = 2;
        let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &cert_ranks,
        );
        let messages1 = [
            (
                ConsensusMessage::Vote(vote_message1),
                ctx.validator_keypairs[vote_rank1].node_keypair.pubkey(),
            ),
            (ConsensusMessage::Certificate(cert), Pubkey::new_unique()),
        ];

        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages1,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 2);
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 1);
        let mut received_verified_votes1 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(received_verified_votes1.len(), 1);
        assert_eq!(
            received_verified_votes1.remove(&5).unwrap(),
            vec![ctx.validator_keypairs[vote_rank1].vote_keypair.pubkey()]
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar(6),
            vote_rank2,
        );
        let messages2 = [(
            ConsensusMessage::Vote(vote_message2),
            ctx.validator_keypairs[vote_rank2].node_keypair.pubkey(),
        )];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages2,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 0);
        let mut received_verified_votes2 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(received_verified_votes2.len(), 1);
        assert_eq!(
            received_verified_votes2.remove(&6).unwrap(),
            vec![ctx.validator_keypairs[vote_rank2].vote_keypair.pubkey()]
        );

        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar_fallback(7),
            vote_rank3,
        );
        let messages3 = [(
            ConsensusMessage::Vote(vote_message3),
            ctx.validator_keypairs[vote_rank3].node_keypair.pubkey(),
        )];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages3,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 0);
        let mut received_verified_votes3 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(received_verified_votes3.len(), 1);
        assert_eq!(
            received_verified_votes3.remove(&7).unwrap(),
            vec![ctx.validator_keypairs[vote_rank3].vote_keypair.pubkey()]
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let mut ctx = TestContext::new();

        let datagrams = vec![datagram_from_bytes(Bytes::new(), Pubkey::new_unique())];
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 0);
        assert_eq!(ctx.verifier.stats.num_malformed_pkts.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet too far in the future
        let rank = 0;
        let vote_message_no_stakes = create_signed_vote_message_with_stake(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            rank,
            NonZero::new(123).unwrap(),
        );
        let messages_no_stakes = [(
            ConsensusMessage::Vote(vote_message_no_stakes),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )];

        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages_no_stakes,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        assert_eq!(ctx.verifier.stats.vote_too_far_in_future.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with invalid rank
        let vote = Vote::new_finalization_vote(5);
        let payload = get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let signature = SignatureAffine::from(ctx.validator_keypairs[0].bls_keypair.sign(&payload));
        let messages_invalid_rank = [(
            ConsensusMessage::Vote(VoteMessage {
                vote: Vote::new_finalization_vote(5),
                signature,
                rank: 1000, // Invalid rank
                stake: NonZero::new(123).unwrap(),
            }),
            Pubkey::new_unique(),
        )];
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages_invalid_rank,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_shred_version_mismatch() {
        let mut ctx = TestContext::new();
        let rank = 0;
        let msgs = [(
            ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version() + 1,
                Vote::new_finalization_vote(5),
                rank,
            )),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )];
        // creating a datagram with the wrong shred version
        let datagrams =
            messages_to_datagrams(&msgs, ctx.verifier.cluster_info.my_shred_version() + 1);
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 0);
        assert_eq!(ctx.verifier.stats.num_malformed_pkts.0, 1);
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        agave_logger::setup();
        let (channel_to_pool, pool_receiver) = crossbeam_channel::bounded(1);
        let mut ctx = TestContext::new_with_pool_channel(channel_to_pool, pool_receiver);

        let msg1_rank = 0;
        let msg2_rank = 2;
        let msg1 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            msg1_rank,
        );
        let msg2 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar_fallback(6),
            msg2_rank,
        );
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &[(
                    ConsensusMessage::Vote(msg1.clone()),
                    ctx.validator_keypairs[msg1_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
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
            .verify_and_send_datagrams(messages_to_datagrams(
                &[(
                    ConsensusMessage::Vote(msg2.clone()),
                    ctx.validator_keypairs[msg2_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        let (m1_recv, m2_recv) = drain.join().expect("drain joined");
        // Both messages were eventually delivered (no silent drop).
        let bank = ctx.verifier.sharable_banks.root();
        let batch1 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg1)]);
        let batch2 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg2)]);
        assert_eq!(m1_recv, batch1);
        assert_eq!(m2_recv, batch2);
        // pool_sent counts every message that made it onto the channel,
        // whether via try_send or the blocking fallback.
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 2);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let mut ctx = TestContext::new();

        // Close the pool receiver to simulate a disconnected channel.
        drop(ctx.pool_receiver);

        let rank = 0;
        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            rank,
        ));
        let messages = [(msg, ctx.validator_keypairs[rank].node_keypair.pubkey())];
        let result = ctx
            .verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(aggregates) => {
                assert_eq!(aggregates.len(), 1);
                assert_eq!(aggregates[0].num_votes(), num_votes);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote2 = Vote::new_unique_notar(43);

        // Group 1 votes
        for (i, validator_keypair) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(num_votes_group1)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote1,
                i,
            ));
            packets.push(message_to_datagram(
                &msg,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        // Group 2 votes
        for (i, validator_keypair) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .skip(num_votes_group1)
            .take(num_votes_group2)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote2,
                i,
            ));
            packets.push(message_to_datagram(
                &msg,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        let total_votes_verified = batches
            .into_iter()
            .map(|batch| match batch {
                SigVerifiedBatch::Votes(aggregates) => {
                    assert_eq!(aggregates.len(), 2);
                    aggregates
                        .iter()
                        .map(|aggregate| aggregate.num_votes())
                        .sum::<usize>()
                }
                rest => panic!("unexpected type: {rest:?}"),
            })
            .sum::<usize>();
        assert_eq!(total_votes_verified, num_votes);
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
        let vote1_payload =
            get_vote_payload_to_sign(vote1, ctx.verifier.cluster_info.my_shred_version());
        let vote2 = Vote::new_skip_vote(43);
        let vote2_payload =
            get_vote_payload_to_sign(vote2, ctx.verifier.cluster_info.my_shred_version());
        let invalid_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(99),
            ctx.verifier.cluster_info.my_shred_version(),
        );

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
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        let total_votes_verified = batches
            .into_iter()
            .map(|batch| match batch {
                SigVerifiedBatch::Votes(aggregates) => {
                    assert_eq!(aggregates.len(), 3);
                    for aggregate in &aggregates {
                        if aggregate.vote() == &vote2
                            && *aggregate.ranks().get(invalid_rank as usize).unwrap()
                        {
                            panic!("invalid vote verified");
                        }
                    }
                    aggregates.iter().map(|v| v.num_votes()).sum::<usize>()
                }
                rest => panic!("unexpected type: {rest:?}"),
            })
            .sum::<usize>();
        assert_eq!(total_votes_verified, num_votes - 1);
    }

    #[test]
    fn test_blssigverifier_verify_votes_one_invalid_signature() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let invalid_rank = 2;
        let mut packets = Vec::with_capacity(num_votes);
        let mut consensus_messages = Vec::with_capacity(num_votes); // ADDED: To hold messages for later comparison.

        let vote = Vote::new_skip_vote(42);
        let valid_vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let invalid_vote_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(99),
            ctx.verifier.cluster_info.my_shred_version(),
        );

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
                stake: NonZero::new(123).unwrap(),
            });

            consensus_messages.push(consensus_message.clone());

            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches: Vec<_> = ctx.pool_receiver.try_iter().collect();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(aggregates) => {
                assert_eq!(aggregates.len(), num_votes - 1);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        // Ensure the message with the invalid rank is not in the sent messages.
        let mut found_msg = false;
        match &batches[0] {
            SigVerifiedBatch::Votes(aggregates) => {
                for aggregate in aggregates {
                    if *aggregate.ranks().get(invalid_rank as usize).unwrap() {
                        found_msg = true;
                        break;
                    }
                }
            }
            rest => panic!("unexpected type: {rest:?}"),
        }
        assert!(!found_msg);
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let mut ctx = TestContext::new();

        // 2/3 of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 2).div_ceil(3);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_just_enough_stake() {
        let mut ctx = TestContext::new();

        // 60% of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 6).div_ceil(10);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let cert_type = CertificateType::new_unique_notar_fallback(slot);
        let cert = test_create_base3_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &[0, 1, 2, 3],
            &[4, 5, 6],
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_just_enough_stake() {
        let mut ctx = TestContext::new();
        let slot = 20;
        let cert_type = CertificateType::new_unique_notar_fallback(slot);
        let cert = test_create_base3_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &[0, 1, 2, 3],
            &[4, 5],
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let mut ctx = TestContext::new();

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let slot = 10;
        let cert_type = CertificateType::new_unique_notar(slot);
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
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(
            ctx.verifier
                .stats
                .cert_stats
                .certificate_verification_failed
                .0,
            1
        );
    }

    #[test]
    fn test_verify_mixed_valid_batch() {
        let mut ctx = TestContext::new();

        let mut packets = Vec::new();
        let num_votes = 2;

        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        packets.push(message_to_datagram(
            &consensus_message_cert,
            ctx.verifier.cluster_info.my_shred_version(),
            Pubkey::new_unique(),
        ));

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);

        let batch_0_was_votes = match &batches[0] {
            SigVerifiedBatch::Votes(aggregates) => {
                assert_eq!(aggregates.len(), 1);
                assert_eq!(aggregates[0].num_votes(), num_votes);
                true
            }
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 1);
                false
            }
        };

        match &batches[1] {
            SigVerifiedBatch::Votes(aggregates) => {
                assert!(!batch_0_was_votes);
                assert_eq!(aggregates.len(), 1);
                assert_eq!(aggregates[0].num_votes(), num_votes);
            }
            SigVerifiedBatch::Certificates(certs) => {
                assert!(batch_0_was_votes);
                assert_eq!(certs.len(), 1);
            }
        }
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 1);
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let mut ctx = TestContext::new();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let bls_keypair = &ctx.validator_keypairs[0].bls_keypair;
        let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
            stake: NonZero::new(123).unwrap(),
        });

        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank.0, 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (channel_to_pool, pool_receiver) = bounded(1024);
        let (channel_to_repair, _repair_receiver) = bounded(1024);
        let (channel_to_metrics, _metrics_receiver) = bounded(1024);
        let (channel_to_reward, _reward_receiver) = bounded(1024);
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
        let (_packet_sender, packet_receiver) = bounded(1024);
        let (_certificate_sender, certificate_receiver) = bounded(1024);
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
        let mut sig_verifier = SigVerifier::new(
            SigVerifierContext {
                migration_status: Arc::new(MigrationStatus::default()),
                ban_sender,
                sharable_banks,
                highest_parent_ready: Arc::new(RwLock::default()),
                cluster_info,
                leader_schedule,
                num_threads: 4,
                generated_cert_types: Arc::new(GeneratedCertTypes::default()),
            },
            SigVerifierChannels::new(
                packet_receiver,
                certificate_receiver,
                channel_to_repair,
                channel_to_reward,
                channel_to_pool,
                channel_to_metrics,
            ),
        );

        let rank = 0;
        let vote = Vote::new_skip_vote(2);
        let vote_payload =
            get_vote_payload_to_sign(vote, sig_verifier.cluster_info.my_shred_version());
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));
        let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
            stake: NonZero::new(123).unwrap(),
        });
        let datagrams_vote = messages_to_datagrams(
            &[(
                consensus_message_vote,
                validator_keypairs[rank].node_keypair.pubkey(),
            )],
            sig_verifier.cluster_info.my_shred_version(),
        );

        sig_verifier
            .verify_and_send_datagrams(datagrams_vote)
            .unwrap();
        expect_no_receive(&pool_receiver);
        assert_eq!(sig_verifier.stats.num_old_votes_received.0, 1);

        let cert = test_create_base2_certificate(
            &validator_keypairs
                .iter()
                .map(|k| k.bls_keypair.clone())
                .collect::<Vec<_>>(),
            sig_verifier.cluster_info.my_shred_version(),
            CertificateType::Finalize(3),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let datagrams_cert = messages_to_datagrams(
            &[(consensus_message_cert, Pubkey::new_unique())],
            sig_verifier.cluster_info.my_shred_version(),
        );

        sig_verifier
            .verify_and_send_datagrams(datagrams_cert)
            .unwrap();
        expect_no_receive(&pool_receiver);
        assert_eq!(sig_verifier.stats.num_old_certs_received.0, 1);
        assert_eq!(sig_verifier.stats.num_old_votes_received.0, 1);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let mut ctx = TestContext::new();

        // 80% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 8).div_ceil(10);
        let slot = 10;
        let cert_type = CertificateType::new_unique_notar(slot);
        let cert1 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message1 = ConsensusMessage::Certificate(cert1);
        let datagrams1 = messages_to_datagrams(
            &[(consensus_message1, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams1).unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 1);

        let cert2 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers - 1).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let datagrams2 = messages_to_datagrams(
            &[(consensus_message2, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier.verify_and_send_datagrams(datagrams2).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 0);
    }

    #[test]
    fn test_same_type_certs_verify_until_first_valid() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::new_unique_notar(10);
        let cert1 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let cert2 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let datagrams = messages_to_datagrams(
            &[
                (ConsensusMessage::Certificate(cert1), Pubkey::new_unique()),
                (ConsensusMessage::Certificate(cert2), Pubkey::new_unique()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.sig_verified_certs.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.redundant_certs_skipped.0, 1);
        assert_eq!(
            ctx.verifier.stats.cert_stats.unnecessary_certs_verified.0,
            0
        );
    }

    #[test]
    fn test_same_type_certs_try_next_candidate_after_failure() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::new_unique_notar(10);
        let num_signers = 7;
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let invalid_cert = Certificate {
            cert_type,
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: encode_base2(&bitmap).unwrap(),
        };
        let valid_cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let invalid_sender = Pubkey::new_unique();
        let valid_sender = Pubkey::new_unique();
        let redundant_sender = Pubkey::new_unique();
        let datagrams = messages_to_datagrams(
            &[
                (ConsensusMessage::Certificate(invalid_cert), invalid_sender),
                (
                    ConsensusMessage::Certificate(valid_cert.clone()),
                    valid_sender,
                ),
                (ConsensusMessage::Certificate(valid_cert), redundant_sender),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
        let banlist = ctx.banned_pubkeys();
        assert!(banlist.contains(&invalid_sender), "Invalid cert -> ban");
        assert!(!banlist.contains(&valid_sender), "Valid certs ok");
        assert!(!banlist.contains(&redundant_sender), "Redundant certs ok");
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 2);
        assert_eq!(ctx.verifier.stats.cert_stats.sig_verified_certs.0, 1);
        assert_eq!(
            ctx.verifier
                .stats
                .cert_stats
                .certificate_verification_failed
                .0,
            1
        );
        assert_eq!(ctx.verifier.stats.cert_stats.redundant_certs_skipped.0, 1);
    }

    #[test]
    fn test_banlist_not_updated_for_valid_vote_and_cert() {
        let mut ctx = TestContext::new();

        let rank = 0;
        let vote_message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(42),
            rank,
        ));
        let cert_message = ConsensusMessage::Certificate(test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            CertificateType::new_unique_notar(43),
            &(0..7).collect::<Vec<_>>(),
        ));
        let vote_sender = ctx.validator_keypairs[rank].node_keypair.pubkey();
        let cert_sender = Pubkey::new_unique();
        let datagrams = messages_to_datagrams(
            &[(vote_message, vote_sender), (cert_message, cert_sender)],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 2);
        let banned = ctx.banned_pubkeys();
        assert!(!banned.contains(&vote_sender));
        assert!(!banned.contains(&cert_sender));
    }

    #[test]
    fn test_banlist_updates_for_invalid_votes() {
        let mut ctx = TestContext::new();

        let vote = Vote::new_skip_vote(42);
        let valid_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let invalid_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(999),
            ctx.verifier.cluster_info.my_shred_version(),
        );
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
                    stake: NonZero::new(123).unwrap(),
                });
                (message, keypair.node_keypair.pubkey())
            })
            .collect();

        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(aggregates) => {
                assert_eq!(aggregates.len(), 3);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        let banned = ctx.banned_pubkeys();
        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    banned.contains(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !banned.contains(sender),
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
                let cert_type = CertificateType::new_unique_notar(slot);
                let mut cert = test_create_base2_certificate(
                    &ctx.bls_keypairs(),
                    ctx.verifier.cluster_info.my_shred_version(),
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
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 3);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        let banned = ctx.banned_pubkeys();
        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    banned.contains(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !banned.contains(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn generated_certs_are_filtered() {
        let mut ctx = TestContext::new();
        let slot = 1235;
        let cert_type = CertificateType::Finalize(slot);
        ctx.generated_cert_types.insert_cert(cert_type);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(ctx.verifier.stats.num_generated_certs_received.0, 1);
    }

    #[test]
    fn votes_are_bounded_by_highest_parent_ready() {
        let mut ctx = TestContext::new();
        let highest_parent_ready_slot = 100;
        *ctx.verifier.highest_parent_ready.write().unwrap() = (
            highest_parent_ready_slot,
            // The ParentReady target slot, rather than the parent block's slot, sets the bound.
            Block::new_unique(7),
        );
        let max_vote_slot = highest_parent_ready_slot + MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY;
        let first_rejected_vote_slot = max_vote_slot + 1;

        let accepted_vote_rank = 0;
        let accepted_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(max_vote_slot),
            accepted_vote_rank,
        ));
        let rejected_vote_rank = 1;
        let rejected_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(first_rejected_vote_slot),
            rejected_vote_rank,
        ));

        // Certificates retain the root-relative bound and are not limited by ParentReady.
        let cert_type = CertificateType::Finalize(first_rejected_vote_slot);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[
                (
                    accepted_vote,
                    ctx.validator_keypairs[accepted_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    rejected_vote,
                    ctx.validator_keypairs[rejected_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (cert, Pubkey::new_unique()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        assert_eq!(ctx.verifier.stats.vote_too_far_in_future.0, 1);
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_too_far_in_future.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sender.sent.0, 1);
        assert_eq!(ctx.pool_receiver.try_iter().count(), 2);
        let mut map = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.remove(&max_vote_slot).unwrap(),
            vec![
                ctx.validator_keypairs[accepted_vote_rank]
                    .vote_keypair
                    .pubkey(),
            ]
        );
        expect_no_receive(&ctx.repair_receiver);
    }

    #[test]
    fn genesis_votes_bypass_future_bound_during_migration() {
        let mut ctx = TestContext::new();
        let highest_parent_ready_slot = 100;
        *ctx.verifier.highest_parent_ready.write().unwrap() = (
            highest_parent_ready_slot,
            Block {
                slot: highest_parent_ready_slot,
                block_id: Hash::new_unique(),
            },
        );
        let max_vote_slot = highest_parent_ready_slot + MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY;
        let migration_slot = ctx.verifier.migration_status.record_feature_activation(200);
        let genesis_slot = migration_slot.saturating_sub(1);
        assert!(genesis_slot > max_vote_slot);

        let genesis_block = Block {
            slot: genesis_slot,
            block_id: Hash::new_unique(),
        };
        ctx.verifier
            .migration_status
            .set_genesis_block(genesis_block);
        let genesis_vote_rank = 0;
        let genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(genesis_block),
            genesis_vote_rank,
        ));

        // Normal votes remain bounded by ParentReady even when they target the exact block.
        let normal_vote_rank = 1;
        let normal_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_notarization_vote(genesis_block),
            normal_vote_rank,
        ));

        // The migration slot itself cannot be the Genesis slot.
        let different_slot_genesis_vote_rank = 2;
        let different_slot_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(Block {
                slot: migration_slot,
                block_id: genesis_block.block_id,
            }),
            different_slot_genesis_vote_rank,
        ));

        // The Genesis exception does not require the locally discovered block's hash either.
        let different_hash_genesis_vote_rank = 3;
        let different_hash_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(Block {
                slot: genesis_slot,
                block_id: Hash::new_unique(),
            }),
            different_hash_genesis_vote_rank,
        ));

        let datagrams = messages_to_datagrams(
            &[
                (
                    genesis_vote,
                    ctx.validator_keypairs[genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    normal_vote,
                    ctx.validator_keypairs[normal_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    different_slot_genesis_vote,
                    ctx.validator_keypairs[different_slot_genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    different_hash_genesis_vote,
                    ctx.validator_keypairs[different_hash_genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        assert_eq!(ctx.verifier.stats.vote_too_far_in_future.0, 2);
        assert_eq!(ctx.verifier.stats.vote_stats.senders.pool_sender.sent.0, 2);
        let SigVerifiedBatch::Votes(aggregates) = ctx.pool_receiver.try_recv().unwrap() else {
            panic!("expected a vote batch");
        };
        assert_eq!(aggregates.len(), 2);
        assert!(
            aggregates
                .iter()
                .all(|aggregate| aggregate.vote().is_genesis_vote())
        );
        expect_no_receive(&ctx.pool_receiver);
        expect_no_receive(&ctx.repair_receiver);
    }

    #[test]
    fn max_admitted_vote_slot_handles_startup_and_overflow() {
        assert_eq!(max_admitted_vote_slot(500, 0), 540);
        assert_eq!(max_admitted_vote_slot(0, Slot::MAX), Slot::MAX);
    }

    #[test]
    fn certs_too_far_in_future_are_dropped() {
        let mut ctx = TestContext::new();
        let slot = ctx.verifier.sharable_banks.root().slot() + NUM_SLOTS_FOR_VERIFY + 1;
        let cert_type = CertificateType::Finalize(slot);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(cert, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        assert_eq!(ctx.verifier.stats.cert_too_far_in_future.0, 1);
        expect_no_receive(&ctx.pool_receiver);
    }

    fn messages_to_datagrams(
        messages: &[(ConsensusMessage, Pubkey)],
        shred_version: u16,
    ) -> Vec<Datagram> {
        messages
            .iter()
            .map(|(message, peer_pubkey)| message_to_datagram(message, shred_version, *peer_pubkey))
            .collect()
    }
}
