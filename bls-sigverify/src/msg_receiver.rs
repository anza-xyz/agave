use {
    crate::{
        MessageToVotesVerifier,
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        certs_verifier::CertPayload,
        generated_cert_types::GeneratedCertTypes,
        rewards::rewards_wants_vote,
        stats::MsgReceiverStats,
        vote_pool::{VotePool, VotePoolError},
        votes_verifier::UnverifiedVote,
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
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_perf::packet::packet_config,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        cmp,
        collections::{HashMap, hash_map::Entry},
        num::Saturating,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

/// Votes further ahead of the highest ParentReady slot are discarded to bound vote tracking
/// memory while still allowing enough lookahead to maintain liveness.
pub(crate) const MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY: Slot = 40;

pub(crate) struct MsgReceiver {
    exit: Arc<AtomicBool>,
    migration_status: Arc<MigrationStatus>,
    datagrams_receiver: Receiver<Datagram>,
    certs_receiver: Receiver<(Slot, UnverifiedCertificate)>,
    highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    cluster_info: Arc<ClusterInfo>,
    sharable_banks: SharableBanks,
    leader_schedule: Arc<LeaderScheduleCache>,
    rank_map_cache: HashMap<Epoch, Arc<BLSPubkeyToRankMap>>,
    vote_pool: VotePool,
    ban_sender: BanSender,
    generated_cert_types: Arc<GeneratedCertTypes>,
    unverified_votes_sender: Sender<MessageToVotesVerifier>,
    unverified_certs_sender: Sender<HashMap<CertificateType, Vec<CertPayload>>>,
    last_checked_root_slot: Slot,
    last_checked_root_epoch: Epoch,
    stats: MsgReceiverStats,
}

impl MsgReceiver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        migration_status: Arc<MigrationStatus>,
        datagrams_receiver: Receiver<Datagram>,
        certs_receiver: Receiver<(Slot, UnverifiedCertificate)>,
        highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        leader_schedule: Arc<LeaderScheduleCache>,
        ban_sender: BanSender,
        generated_cert_types: Arc<GeneratedCertTypes>,
        unverified_votes_sender: Sender<MessageToVotesVerifier>,
        unverified_certs_sender: Sender<HashMap<CertificateType, Vec<CertPayload>>>,
    ) -> Self {
        Self {
            exit,
            migration_status,
            datagrams_receiver,
            certs_receiver,
            highest_parent_ready,
            sharable_banks,
            cluster_info,
            leader_schedule,
            rank_map_cache: HashMap::new(),
            vote_pool: VotePool::default(),
            ban_sender,
            generated_cert_types,
            unverified_votes_sender,
            unverified_certs_sender,
            last_checked_root_slot: 0,
            last_checked_root_epoch: 0,
            stats: MsgReceiverStats::default(),
        }
    }

    fn recv(
        &self,
        datagrams_buffer: &mut Vec<Datagram>,
    ) -> Result<Vec<(Slot, UnverifiedCertificate)>, ()> {
        const SOFT_RECEIVE_CAP: usize = 5000;
        let mut certificates = vec![];
        select! {
            recv(self.datagrams_receiver) -> datagram => {
                datagrams_buffer.push(datagram.map_err(|_| ())?);
            }
            recv(self.certs_receiver) -> certificate => {
                certificates.push(certificate.map_err(|_| ())?);
            },
            default(Duration::from_secs(1)) => return Ok(certificates),
        }
        while datagrams_buffer.len() < SOFT_RECEIVE_CAP {
            match self.datagrams_receiver.try_recv() {
                Ok(datagram) => {
                    datagrams_buffer.push(datagram);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Err(()),
            }
        }
        // Certificates from blockstore are very low throughput (1 per slot), so no need for a cap here
        certificates.extend(self.certs_receiver.try_iter());
        Ok(certificates)
    }

    /// If this vote should be verified, then returns the [`UnverifiedVotePayload`].
    fn keep_vote(
        &mut self,
        msg: UnverifiedVoteMessage,
        rank_map: &BLSPubkeyToRankMap,
        sender_identity_pubkey: Pubkey,
        root_bank: &Bank,
    ) -> Option<UnverifiedVote> {
        // votes from self take a different pathway.
        if sender_identity_pubkey == self.cluster_info.id() {
            return None;
        }
        let root_slot = root_bank.slot();
        let vote_slot = msg.vote.slot();

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
                    self.stats.old_votes += 1;
                    return None;
                }
            }
            // Votes above the root are always allowed
            cmp::Ordering::Greater => (),
        }

        let (rank, entry) = rank_map
            .get_ranked_entry_for_node(&sender_identity_pubkey)
            .or_else(|| {
                self.stats.invalid_rank += 1;
                None
            })?;
        match self.vote_pool.try_add_vote(&msg, rank, rank_map.len()) {
            Ok(()) => Some(UnverifiedVote {
                vote_message: msg,
                sender_bls_pubkey: entry.bls_pubkey,
                sender_vote_account_pubkey: entry.vote_account_pubkey,
                sender_identity_pubkey,
                stake: entry.stake,
                rank,
            }),
            Err(VotePoolError::Duplicate) => {
                self.stats.duplicate_vote += 1;
                None
            }
            Err(VotePoolError::Invalid) => {
                self.stats.invalid_vote += 1;
                self.ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
                info!(
                    "bls_sigverifier: banned sender={sender_identity_pubkey} due to invalid vote"
                );
                None
            }
        }
    }

    fn add_certificate_to_group(
        &mut self,
        cert_groups: &mut HashMap<CertificateType, Vec<CertPayload>>,
        cert: UnverifiedCertificate,
        sender_identity_pubkey: Pubkey,
    ) {
        if self.generated_cert_types.has_cert(&cert.cert_type) {
            self.stats.generated_certs_received += 1;
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

    fn extract_msgs(
        &mut self,
        root_bank: &Bank,
        datagrams: &[Datagram],
        certificates: Vec<(Slot, UnverifiedCertificate)>,
    ) -> (
        HashMap<CertificateType, Vec<CertPayload>>,
        MessageToVotesVerifier,
    ) {
        let root_slot = root_bank.slot();
        let highest_parent_ready_slot = self.highest_parent_ready.read().unwrap().0;
        let max_vote_slot = max_admitted_vote_slot(root_slot, highest_parent_ready_slot);
        let migration_slot = self.migration_status.migration_slot();
        let mut cert_groups = HashMap::<CertificateType, Vec<CertPayload>>::new();
        let mut votes: HashMap<VotePayloadToSign, (Vec<UnverifiedVote>, Arc<BLSPubkeyToRankMap>)> =
            HashMap::new();
        let my_shred_version = self.cluster_info.my_shred_version();
        let mut num_votes = Saturating(0);
        let mut num_certs = Saturating(0);
        self.stats.total_pkts += datagrams.len().saturating_add(certificates.len());
        for Datagram {
            peer_pubkey: sender_identity_pubkey,
            message,
            ..
        } in datagrams
        {
            let Ok(msg) = VersionedWireConsensusMessage::deserialize_with_expected_shred_version(
                message.as_ref(),
                packet_config(),
                my_shred_version,
            ) else {
                self.stats.deserialization_failed += 1;
                continue;
            };
            let decoded_msg = DecodedWireConsensusMessage::new(msg);

            match decoded_msg {
                DecodedWireConsensusMessage::Vote(unverified_vote) => {
                    let vote_slot = unverified_vote.vote.slot();
                    let is_in_range = match unverified_vote.vote {
                        // Genesis votes bypass the normal range check. They are accepted only
                        // during the migration epoch and before the migration slot.
                        Vote::Genesis(_) => {
                            migration_slot.is_some_and(|migration_slot| vote_slot < migration_slot)
                        }
                        _ => vote_slot <= max_vote_slot,
                    };
                    if !is_in_range {
                        self.stats.votes_too_far_in_future += 1;
                        continue;
                    }
                    let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
                    let rank_map = match self.rank_map_cache.entry(vote_epoch) {
                        Entry::Occupied(entry) => Arc::clone(entry.get()),
                        Entry::Vacant(entry) => {
                            let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                                self.stats.no_epoch_stakes += 1;
                                continue;
                            };
                            Arc::clone(entry.insert(rank_map.clone()))
                        }
                    };
                    if let Some(payload) = self.keep_vote(
                        unverified_vote,
                        &rank_map,
                        *sender_identity_pubkey,
                        root_bank,
                    ) {
                        num_votes += 1;
                        let vote_payload_to_sign = VotePayloadToSign::new_from_vote(
                            payload.vote_message.vote,
                            payload.vote_message.shred_version,
                        );
                        votes
                            .entry(vote_payload_to_sign)
                            .or_insert_with(|| (vec![], rank_map))
                            .0
                            .push(payload);
                    } else {
                        self.stats.keep_vote_failed += 1;
                    }
                }
                DecodedWireConsensusMessage::Certificate(cert) => {
                    let cert_slot = cert.cert_type.slot();
                    if cert_slot < root_slot {
                        self.stats.old_certs += 1;
                        continue;
                    }
                    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                        self.stats.certs_too_far_in_future += 1;
                        continue;
                    }
                    num_certs += 1;
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
                self.stats.old_certs += 1;
                continue;
            }
            if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                self.stats.certs_too_far_in_future += 1;
                continue;
            }
            let Some(sender_identity_pubkey) = self
                .leader_schedule
                .slot_leader_at(carrier_slot, Some(root_bank))
                .map(|leader| leader.id)
            else {
                continue;
            };
            num_certs += 1;
            self.add_certificate_to_group(&mut cert_groups, certificate, sender_identity_pubkey);
        }
        self.stats.votes_received += num_votes;
        self.stats.votes_batches.add_sample(num_votes.0);
        self.stats.certs_received += num_certs;
        self.stats.certs_batches.add_sample(num_certs.0);
        (cert_groups, votes)
    }

    fn send_votes_and_certs(
        &self,
        votes: HashMap<VotePayloadToSign, (Vec<UnverifiedVote>, Arc<BLSPubkeyToRankMap>)>,
        certs: HashMap<CertificateType, Vec<CertPayload>>,
    ) -> Result<(), ()> {
        if votes.is_empty() && certs.is_empty() {
            return Ok(());
        }
        if votes.is_empty() {
            return self.unverified_certs_sender.send(certs).map_err(|_| ());
        }
        if certs.is_empty() {
            return self.unverified_votes_sender.send(votes).map_err(|_| ());
        }
        select! {
            send(self.unverified_votes_sender, votes) -> result => {
                result.map_err(|_| ())?;
                self.unverified_certs_sender.send(certs).map_err(|_| ())?;
            }
            send(self.unverified_certs_sender, certs) -> result => {
                result.map_err(|_| ())?;
                self.unverified_votes_sender.send(votes).map_err(|_| ())?;
            }
        }
        Ok(())
    }

    fn maybe_prune(&mut self, root_bank: &Bank) {
        let root_slot = root_bank.slot();
        let root_epoch = root_bank.epoch();
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.vote_pool.prune(root_slot);
        }
        if self.last_checked_root_epoch < root_epoch {
            self.last_checked_root_epoch = root_epoch;
            // Keeping previous epoch as we need to look up slots older than root_slot for rewards.
            self.rank_map_cache
                .retain(|epoch, _| *epoch >= root_epoch.saturating_sub(1));
        }
    }

    pub(crate) fn run(mut self) {
        let mut datagrams_buffer = Vec::new();
        while !self.exit.load(Ordering::Relaxed) {
            datagrams_buffer.clear();
            let Ok(certificates) = self.recv(&mut datagrams_buffer) else {
                error!("sigverifier input channel disconnected: Exiting.");
                break;
            };
            if self.migration_status.is_pre_feature_activation() {
                continue;
            }
            if datagrams_buffer.is_empty() && certificates.is_empty() {
                continue;
            }

            let root_bank = self.sharable_banks.root();
            let (certs, votes) = self.extract_msgs(&root_bank, &datagrams_buffer, certificates);
            if let Err(()) = self.send_votes_and_certs(votes, certs) {
                error!("vote sender certs sender channel disconnected: Exiting.");
                break;
            }
            self.stats.maybe_report();
            let root_bank = self.sharable_banks.root();
            self.maybe_prune(&root_bank);
        }
    }
}

pub(crate) fn max_admitted_vote_slot(root_slot: Slot, highest_parent_ready_slot: Slot) -> Slot {
    cmp::max(root_slot, highest_parent_ready_slot)
        .saturating_add(MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY)
}

#[cfg(test)]
mod metrics_tests {
    use {
        super::*, crate::test_utils::MetricsTestContext,
        agave_votor_transport::endpoint::stub_ban_channel_for_tests, crossbeam_channel::unbounded,
    };

    #[test]
    fn metrics_count_filtered_inputs_and_accumulate_batches() {
        let ctx = MetricsTestContext::new();
        let root = ctx.banks.root();
        let (ban_sender, mut bans) = stub_ban_channel_for_tests(16);
        let mut receiver = MsgReceiver::new(
            Arc::new(AtomicBool::new(false)),
            Arc::new(MigrationStatus::post_migration_status()),
            unbounded().1,
            unbounded().1,
            Arc::new(RwLock::new((0, Block::new_unique(0)))),
            ctx.cluster_info.clone(),
            ctx.banks.clone(),
            Arc::new(LeaderScheduleCache::new_from_bank(&root)),
            ban_sender,
            Arc::new(GeneratedCertTypes::default()),
            unbounded().0,
            unbounded().0,
        );
        let mut unknown_sender = ctx.datagram(Vote::new_skip_vote(2), 0);
        unknown_sender.peer_pubkey = Pubkey::new_unique();
        let mut malformed = ctx.datagram(Vote::new_skip_vote(3), 0);
        malformed.message = vec![255].into();
        let datagrams = vec![
            ctx.datagram(Vote::new_skip_vote(1), 0),
            ctx.datagram(Vote::new_skip_vote(1), 0), // Duplicate.
            ctx.datagram(Vote::new_finalization_vote(1), 0), // Conflicts with skip.
            unknown_sender,
            malformed,
            ctx.datagram(Vote::new_skip_vote(41), 0), // Beyond the admission window.
            ctx.datagram(Vote::new_finalization_vote(0), 0), // Old and not useful for rewards.
        ];
        let (certs, votes) = receiver.extract_msgs(&root, &datagrams, vec![]);
        assert!(certs.is_empty());
        assert_eq!(votes.len(), 1);
        assert_eq!(receiver.stats.total_pkts.0, 7);
        assert_eq!(receiver.stats.votes_received.0, 1);
        assert_eq!(receiver.stats.duplicate_vote.0, 1);
        assert_eq!(receiver.stats.invalid_vote.0, 1);
        assert_eq!(receiver.stats.invalid_rank.0, 1);
        assert_eq!(receiver.stats.deserialization_failed.0, 1);
        assert_eq!(receiver.stats.votes_too_far_in_future.0, 1);
        assert_eq!(receiver.stats.old_votes.0, 1);
        assert_eq!(receiver.stats.keep_vote_failed.0, 4);
        assert!(bans.try_recv().is_ok());
        assert!(bans.try_recv().is_err());

        let generated = CertificateType::Finalize(2);
        receiver.generated_cert_types.insert_cert(generated);
        let mut wrong_version = ctx.certificate(CertificateType::Finalize(3));
        wrong_version.shred_version = ctx.cluster_info.my_shred_version().wrapping_add(1);
        let certificates = vec![
            (3, ctx.certificate(generated)),
            (3, ctx.certificate(CertificateType::Finalize(3))),
            (
                3,
                ctx.certificate(CertificateType::Finalize(NUM_SLOTS_FOR_VERIFY + 1)),
            ),
            (3, wrong_version),
        ];
        let (certs, votes) = receiver.extract_msgs(
            &root,
            &[
                ctx.datagram(Vote::new_skip_vote(2), 0),
                ctx.datagram(Vote::new_skip_vote(2), 1),
            ],
            certificates,
        );
        assert_eq!(certs.len(), 1);
        assert_eq!(
            votes.values().map(|(votes, _)| votes.len()).sum::<usize>(),
            2
        );
        assert_eq!(receiver.stats.total_pkts.0, 13);
        assert_eq!(receiver.stats.votes_received.0, 3);
        assert_eq!(receiver.stats.certs_received.0, 2);
        assert_eq!(receiver.stats.generated_certs_received.0, 1);
        assert_eq!(receiver.stats.certs_too_far_in_future.0, 1);
        assert_eq!(receiver.stats.votes_batches.count(), 2);
        assert_eq!(receiver.stats.votes_batches.mean::<f64>(), Some(1.5));
        assert_eq!(receiver.stats.votes_batches.maximum::<u64>(), Some(2));
        assert_eq!(receiver.stats.certs_batches.count(), 2);
        assert_eq!(receiver.stats.certs_batches.mean::<u64>(), Some(1));
        assert_eq!(receiver.stats.certs_batches.maximum::<u64>(), Some(2));

        // A batch containing only rejected input still contributes to total traffic.
        let mut malformed = ctx.datagram(Vote::new_skip_vote(4), 0);
        malformed.message = vec![255].into();
        receiver.extract_msgs(&root, &[malformed], vec![]);
        assert_eq!(receiver.stats.total_pkts.0, 14);
        assert_eq!(receiver.stats.deserialization_failed.0, 2);
        assert_eq!(receiver.stats.votes_received.0, 3);
        assert_eq!(receiver.stats.votes_batches.count(), 3);
        assert_eq!(receiver.stats.votes_batches.mean::<u64>(), Some(1));
        assert_eq!(receiver.stats.certs_batches.count(), 3);

        // Admit a distant slot but omit its epoch stakes.
        receiver.highest_parent_ready.write().unwrap().0 = 10_000_000;
        receiver.extract_msgs(
            &root,
            &[ctx.datagram(Vote::new_skip_vote(10_000_000), 0)],
            vec![],
        );
        assert_eq!(receiver.stats.no_epoch_stakes.0, 1);
        assert_eq!(receiver.stats.total_pkts.0, 15);

        let later_root =
            Bank::new_from_parent(root, solana_runtime::bank::SlotLeader::default(), 5);
        receiver.extract_msgs(
            &later_root,
            &[],
            vec![(6, ctx.certificate(CertificateType::Finalize(1)))],
        );
        assert_eq!(receiver.stats.old_certs.0, 1);
        assert_eq!(receiver.stats.total_pkts.0, 16);
        assert_eq!(receiver.stats.certs_received.0, 2);
    }
}
