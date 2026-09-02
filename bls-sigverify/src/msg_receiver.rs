use {
    crate::{
        UnverifiedVotesMessage,
        bls_cert_sigverify::CertPayload,
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        bls_vote_sigverify::UnverifiedVotePayload,
        generated_cert_types::GeneratedCertTypes,
        rewards::rewards_wants_vote,
        stats::MsgReceiverStats,
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
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_perf::packet::packet_config,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        cmp,
        collections::{HashMap, hash_map::Entry},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

/// Votes further ahead of the highest ParentReady slot are discarded to bound vote tracking
/// memory while still allowing enough lookahead to maintain liveness.
const MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY: Slot = 40;

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
    unverified_votes_sender: Sender<UnverifiedVotesMessage>,
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
        unverified_votes_sender: Sender<UnverifiedVotesMessage>,
        unverified_certs_sender: Sender<HashMap<CertificateType, Vec<CertPayload>>>,
    ) -> Self {
        let root_slot = sharable_banks.root().slot();
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
            stats: MsgReceiverStats::new(root_slot),
        }
    }

    fn recv_inputs(
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
            self.stats.votes_too_far_in_future += 1;
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
                    self.stats.old_votes_received += 1;
                    return None;
                }
            }
            // Votes above the root are always allowed
            cmp::Ordering::Greater => (),
        }

        let (rank, entry) = rank_map
            .get_ranked_entry_for_node(&sender_identity_pubkey)
            .or_else(|| {
                self.stats.vote_invalid_rank += 1;
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
        UnverifiedVotesMessage,
    ) {
        let root_slot = root_bank.slot();
        let highest_parent_ready_slot = self.highest_parent_ready.read().unwrap().0;
        let max_vote_slot = max_admitted_vote_slot(root_slot, highest_parent_ready_slot);
        let migration_slot = self.migration_status.migration_slot();
        let mut cert_groups = HashMap::<CertificateType, Vec<CertPayload>>::new();
        let mut votes: HashMap<
            VotePayloadToSign,
            (Vec<UnverifiedVotePayload>, Arc<BLSPubkeyToRankMap>),
        > = HashMap::new();
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
                self.stats.deserialization_failed += 1;
                continue;
            };
            let decoded_msg = DecodedWireConsensusMessage::new(msg);

            match decoded_msg {
                DecodedWireConsensusMessage::Vote(unverified_vote) => {
                    let vote_slot = unverified_vote.vote.slot();
                    let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
                    let rank_map = match self.rank_map_cache.entry(vote_epoch) {
                        Entry::Occupied(entry) => Arc::clone(entry.get()),
                        Entry::Vacant(entry) => {
                            let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                                self.stats.vote_no_epoch_stakes += 1;
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
                        max_vote_slot,
                        migration_slot,
                    ) {
                        self.stats.votes_received += 1;
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
                        self.stats.old_certs_received += 1;
                        continue;
                    }
                    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                        self.stats.certs_too_far_in_future_received += 1;
                        continue;
                    }
                    self.stats.certs_received += 1;
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
                self.stats.old_certs_received += 1;
                continue;
            }
            if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
                self.stats.certs_too_far_in_future_received += 1;
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
        self.stats.total_pkts += num_pkts;
        (cert_groups, votes)
    }

    fn send_votes_and_certs(
        &self,
        votes: HashMap<VotePayloadToSign, (Vec<UnverifiedVotePayload>, Arc<BLSPubkeyToRankMap>)>,
        certs: HashMap<CertificateType, Vec<CertPayload>>,
    ) -> Result<(), ()> {
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
            let Ok(certificates) = self.recv_inputs(&mut datagrams_buffer) else {
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
            let ((certs, votes), _extract_us) =
                measure_us!(self.extract_msgs(&root_bank, &datagrams_buffer, certificates));
            if let Err(()) = self.send_votes_and_certs(votes, certs) {
                error!("vote sender certs sender channel disconnected: Exiting.");
                break;
            }
            let root_bank = self.sharable_banks.root();
            self.stats.maybe_report(root_bank.slot());
            self.maybe_prune(&root_bank);
        }
    }
}

fn max_admitted_vote_slot(root_slot: Slot, highest_parent_ready_slot: Slot) -> Slot {
    cmp::max(root_slot, highest_parent_ready_slot)
        .saturating_add(MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY)
}
