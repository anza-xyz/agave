//! Defines ConsensusPool to store received and generated votes and certificates.

use {
    crate::{
        common::{certificate_limits_and_votes, vote_to_certificate_ids, Stake},
        consensus_pool::{
            certificate_builder::{BuildError as CertificateBuilderError, CertificateBuilder},
            parent_ready_tracker::ParentReadyTracker,
            slot_stake_counters::SlotStakeCounters,
            stats::ConsensusPoolStats,
            vote_pool::{AddVoteError as VotePoolError, VotePool},
        },
        event::VotorEvent,
    },
    agave_votor_messages::{
        consensus_message::{Block, Certificate, CertificateType, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    log::{error, trace},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::VersionedEpochStakes},
    std::{
        cmp::Ordering,
        collections::{BTreeMap, HashMap},
        sync::Arc,
    },
    thiserror::Error,
};

mod certificate_builder;
pub(crate) mod parent_ready_tracker;
mod slot_stake_counters;
mod stats;
mod vote_pool;

/// Different failure cases from calling `add_vote()`.
#[derive(Debug, Error)]
enum AddVoteError {
    #[error("Epoch stakes missing for epoch: {0}")]
    EpochStakesNotFound(Epoch),
    #[error("Unrooted slot")]
    UnrootedSlot,
    #[error("Certificate builder error: {0}")]
    CertificateBuilder(#[from] CertificateBuilderError),
    #[error("Invalid rank: {0}")]
    InvalidRank(u16),

    #[error("vote pool returned error: {0}")]
    VotePool(#[from] VotePoolError),
}

/// Different failure cases from calling `add_certificate()`.
#[derive(Debug, Error)]
enum AddCertError {
    #[error("Unrooted slot")]
    UnrootedSlot,
}

/// Different failure cases from calling `add_message()`.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum AddMessageError {
    #[error("internal failure {0}")]
    Internal(String),
}

fn get_key_and_stakes(
    epoch_schedule: &EpochSchedule,
    epoch_stakes_map: &HashMap<Epoch, VersionedEpochStakes>,
    slot: Slot,
    rank: u16,
) -> Result<(Pubkey, Stake, Stake), AddVoteError> {
    let epoch = epoch_schedule.get_epoch(slot);
    let epoch_stakes = epoch_stakes_map
        .get(&epoch)
        .ok_or(AddVoteError::EpochStakesNotFound(epoch))?;
    let Some((vote_key, _)) = epoch_stakes
        .bls_pubkey_to_rank_map()
        .get_pubkey(rank as usize)
    else {
        return Err(AddVoteError::InvalidRank(rank));
    };
    let stake = epoch_stakes.vote_account_stake(vote_key);
    if stake == 0 {
        // Since we have a valid rank, this should never happen, there is no rank for zero stake.
        panic!("Validator stake is zero for pubkey: {vote_key}");
    }
    Ok((*vote_key, stake, epoch_stakes.total_stake()))
}

/// Container to store received votes and certificates.
///
/// Based on received votes and certificates, generates new `VotorEvent`s and generates new certificates.
pub(crate) struct ConsensusPool {
    cluster_info: Arc<ClusterInfo>,
    // Storage for per slot votes.
    // Adding new votes in the vote uses the prior votes to check for invalid and duplicate votes.
    vote_pools: BTreeMap<Slot, VotePool>,
    /// Completed certificates
    completed_certificates: BTreeMap<CertificateType, Arc<Certificate>>,
    /// Tracks slots which have reached the parent ready condition:
    /// - They have a potential parent block with a NotarizeFallback certificate
    /// - All slots from the parent have a Skip certificate
    pub(crate) parent_ready_tracker: ParentReadyTracker,
    /// Highest slot that has a Finalized variant certificate
    highest_finalized_slot: Option<Slot>,
    /// Highest slot that has Finalize+Notarize or FinalizeFast, for use in standstill
    /// Also add a bool to indicate whether this slot has FinalizeFast certificate
    highest_finalized_with_notarize: Option<(Slot, bool)>,
    /// Stats for the certificate pool
    stats: ConsensusPoolStats,
    /// Slot stake counters, used to calculate safe_to_notar and safe_to_skip
    slot_stake_counters_map: BTreeMap<Slot, SlotStakeCounters>,
}

impl ConsensusPool {
    pub(crate) fn new_from_root_bank(cluster_info: Arc<ClusterInfo>, bank: &Bank) -> Self {
        // To account for genesis and snapshots we allow default block id until
        // block id can be serialized  as part of the snapshot
        let root_block = (bank.slot(), bank.block_id().unwrap_or_default());
        let parent_ready_tracker = ParentReadyTracker::new(cluster_info.clone(), root_block);

        Self {
            cluster_info,
            vote_pools: BTreeMap::new(),
            completed_certificates: BTreeMap::new(),
            highest_finalized_slot: None,
            highest_finalized_with_notarize: None,
            parent_ready_tracker,
            stats: ConsensusPoolStats::default(),
            slot_stake_counters_map: BTreeMap::new(),
        }
    }

    /// Builds new [`Certificate`]s that depend on votes of type [`Vote`] if enough stake has voted for them.
    fn build_certs(
        &mut self,
        vote: &Vote,
        total_stake: Stake,
    ) -> Result<Vec<Arc<Certificate>>, AddVoteError> {
        let Some(vote_pool) = self.vote_pools.get(&vote.slot()) else {
            return Ok(vec![]);
        };
        let mut new_certificates_to_send = Vec::new();
        for cert_type in vote_to_certificate_ids(vote) {
            // If the certificate is already complete, skip it
            if self.completed_certificates.contains_key(&cert_type) {
                continue;
            }
            // Otherwise check whether the certificate is complete
            let (limit, vote, fallback_vote) = certificate_limits_and_votes(&cert_type);
            let accumulated_stake = vote_pool
                .get_stake(&vote)
                .saturating_add(fallback_vote.map_or(0, |v| vote_pool.get_stake(&v)));

            if accumulated_stake as f64 / (total_stake as f64) < limit {
                continue;
            }
            let mut cert_builder = CertificateBuilder::new(cert_type);
            cert_builder.aggregate(&vote_pool.get_votes(&vote)).unwrap();
            if let Some(v) = fallback_vote {
                cert_builder.aggregate(&vote_pool.get_votes(&v)).unwrap();
            }
            let new_cert = Arc::new(cert_builder.build()?);
            self.stats.incr_cert_type(&new_cert.cert_type, true);
            new_certificates_to_send.push(new_cert);
        }
        Ok(new_certificates_to_send)
    }

    /// Inserts a new [`Certificate`].
    ///
    /// Based on the type of certificate being inserted, updates [`self.parent_ready_tracker`] and other metadata on self.
    fn insert_certificate(&mut self, cert: Arc<Certificate>, events: &mut Vec<VotorEvent>) {
        let cert_type = cert.cert_type;
        trace!(
            "{}: Inserting certificate {:?}",
            self.cluster_info.id(),
            cert_type
        );
        self.completed_certificates.insert(cert_type, cert);
        match cert_type {
            CertificateType::NotarizeFallback(slot, block_id) => {
                self.parent_ready_tracker
                    .add_new_notar_fallback_or_stronger((slot, block_id), events);
            }
            CertificateType::Skip(slot) => self.parent_ready_tracker.add_new_skip(slot, events),
            CertificateType::Notarize(slot, block_id) => {
                events.push(VotorEvent::BlockNotarized((slot, block_id)));
                self.parent_ready_tracker
                    .add_new_notar_fallback_or_stronger((slot, block_id), events);
                if self.is_finalized(slot) {
                    // It's fine to set FastFinalization to false here, because
                    // we will report correctly as long as we have FastFinalization cert.
                    events.push(VotorEvent::Finalized((slot, block_id), false));
                    if self
                        .highest_finalized_with_notarize
                        .is_none_or(|(s, _)| s < slot)
                    {
                        self.highest_finalized_with_notarize = Some((slot, false));
                    }
                }
            }
            CertificateType::Finalize(slot) => {
                if let Some(block) = self.get_notarized_block(slot) {
                    events.push(VotorEvent::Finalized(block, false));
                    if self
                        .highest_finalized_with_notarize
                        .is_none_or(|(s, _)| s < slot)
                    {
                        self.highest_finalized_with_notarize = Some((slot, false));
                    }
                }
                if self.highest_finalized_slot.is_none_or(|s| s < slot) {
                    self.highest_finalized_slot = Some(slot);
                }
            }
            CertificateType::FinalizeFast(slot, block_id) => {
                events.push(VotorEvent::Finalized((slot, block_id), true));
                self.parent_ready_tracker
                    .add_new_notar_fallback_or_stronger((slot, block_id), events);
                if self.highest_finalized_slot.is_none_or(|s| s < slot) {
                    self.highest_finalized_slot = Some(slot);
                }
                if self
                    .highest_finalized_with_notarize
                    .is_none_or(|(s, _)| s <= slot)
                {
                    self.highest_finalized_with_notarize = Some((slot, true));
                }
            }
            CertificateType::Genesis(_slot, _block_id) => {}
        }
    }

    /// Adds the new vote the the consensus pool. If a new certificate is created
    /// as a result of this, send it via the `self.certificate_sender`
    ///
    /// Any new votor events that are a result of adding this new vote will be added
    /// to `events`.
    ///
    /// If this resulted in a new highest Finalize or FastFinalize certificate,
    /// return the slot
    pub(crate) fn add_message(
        &mut self,
        epoch_schedule: &EpochSchedule,
        epoch_stakes_map: &HashMap<Epoch, VersionedEpochStakes>,
        root_slot: Slot,
        my_vote_pubkey: &Pubkey,
        message: ConsensusMessage,
        events: &mut Vec<VotorEvent>,
    ) -> Result<(Option<Slot>, Vec<Arc<Certificate>>), AddMessageError> {
        let current_highest_finalized_slot = self.highest_finalized_slot;
        let new_certficates_to_send = match message {
            ConsensusMessage::Vote(vote_message) => self
                .add_vote(
                    epoch_schedule,
                    epoch_stakes_map,
                    root_slot,
                    my_vote_pubkey,
                    vote_message,
                    events,
                )
                .map_err(|e| AddMessageError::Internal(e.to_string()))?,
            ConsensusMessage::Certificate(cert) => self
                .add_certificate(root_slot, cert, events)
                .map_err(|e| AddMessageError::Internal(e.to_string()))?,
        };
        // If we have a new highest finalized slot, return it
        let new_finalized_slot = if self.highest_finalized_slot > current_highest_finalized_slot {
            self.highest_finalized_slot
        } else {
            None
        };
        Ok((new_finalized_slot, new_certficates_to_send))
    }

    fn add_vote(
        &mut self,
        epoch_schedule: &EpochSchedule,
        epoch_stakes_map: &HashMap<Epoch, VersionedEpochStakes>,
        root_slot: Slot,
        my_vote_pubkey: &Pubkey,
        vote_message: VoteMessage,
        events: &mut Vec<VotorEvent>,
    ) -> Result<Vec<Arc<Certificate>>, AddVoteError> {
        let vote = &vote_message.vote;
        let rank = vote_message.rank;
        let vote_slot = vote.slot();
        let (validator_vote_key, validator_stake, total_stake) =
            get_key_and_stakes(epoch_schedule, epoch_stakes_map, vote_slot, rank)?;

        // Since we have a valid rank, this should never happen, there is no rank for zero stake.
        assert_ne!(
            validator_stake, 0,
            "Validator stake is zero for pubkey: {validator_vote_key}"
        );

        self.stats.incoming_votes = self.stats.incoming_votes.saturating_add(1);
        if vote_slot < root_slot {
            self.stats.out_of_range_votes = self.stats.out_of_range_votes.saturating_add(1);
            return Err(AddVoteError::UnrootedSlot);
        }
        let vote = vote_message.vote;
        match self
            .vote_pools
            .entry(vote_slot)
            .or_insert(VotePool::new(vote_slot))
            .add_vote(validator_vote_key, validator_stake, vote_message)
        {
            Ok(stake) => {
                let fallback_vote_counters = self
                    .slot_stake_counters_map
                    .entry(vote_slot)
                    .or_insert_with(|| SlotStakeCounters::new(total_stake));
                fallback_vote_counters.add_vote(
                    &vote,
                    stake,
                    my_vote_pubkey == &validator_vote_key,
                    events,
                    &mut self.stats,
                );
            }
            Err(e) => match e {
                vote_pool::AddVoteError::Duplicate => {
                    self.stats.exist_votes = self.stats.exist_votes.saturating_add(1);
                    return Ok(vec![]);
                }
                vote_pool::AddVoteError::Invalid => {
                    self.stats.invalid_votes = self.stats.invalid_votes.saturating_add(1);
                    return Err(e.into());
                }
            },
        }
        self.stats.incr_ingested_vote(&vote);
        self.build_certs(&vote, total_stake).inspect(|certs| {
            for cert in certs {
                self.insert_certificate(cert.clone(), events)
            }
        })
    }

    fn add_certificate(
        &mut self,
        root_slot: Slot,
        cert: Certificate,
        events: &mut Vec<VotorEvent>,
    ) -> Result<Vec<Arc<Certificate>>, AddCertError> {
        let cert_type = &cert.cert_type;
        self.stats.incoming_certs = self.stats.incoming_certs.saturating_add(1);
        if cert_type.slot() < root_slot {
            self.stats.out_of_range_certs = self.stats.out_of_range_certs.saturating_add(1);
            return Err(AddCertError::UnrootedSlot);
        }
        if self.completed_certificates.contains_key(cert_type) {
            self.stats.exist_certs = self.stats.exist_certs.saturating_add(1);
            return Ok(vec![]);
        }
        self.stats.incr_cert_type(cert_type, false);
        let cert = Arc::new(cert);
        self.insert_certificate(cert.clone(), events);
        Ok(vec![cert])
    }

    /// Get the notarized block in `slot`
    fn get_notarized_block(&self, slot: Slot) -> Option<Block> {
        self.completed_certificates
            .iter()
            .find_map(|(cert_type, _)| match cert_type {
                CertificateType::Notarize(s, block_id) if slot == *s => Some((*s, *block_id)),
                _ => None,
            })
    }

    #[cfg(test)]
    fn highest_notarized_slot(&self) -> Slot {
        // Return the max of CertificateType::Notarize and CertificateType::NotarizeFallback
        self.completed_certificates
            .iter()
            .filter_map(|(cert_type, _)| match cert_type {
                CertificateType::Notarize(s, _) => Some(s),
                CertificateType::NotarizeFallback(s, _) => Some(s),
                _ => None,
            })
            .max()
            .copied()
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn highest_skip_slot(&self) -> Slot {
        self.completed_certificates
            .iter()
            .filter_map(|(cert_type, _)| match cert_type {
                CertificateType::Skip(s) => Some(s),
                _ => None,
            })
            .max()
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn highest_finalized_slot(&self) -> Slot {
        self.completed_certificates
            .iter()
            .filter_map(|(cert_type, _)| match cert_type {
                CertificateType::Finalize(s) => Some(s),
                CertificateType::FinalizeFast(s, _) => Some(s),
                _ => None,
            })
            .max()
            .copied()
            .unwrap_or(0)
    }

    /// Checks if any block in the slot `s` is finalized
    fn is_finalized(&self, slot: Slot) -> bool {
        self.completed_certificates.keys().any(|cert_type| {
            matches!(cert_type, CertificateType::Finalize(s) | CertificateType::FinalizeFast(s, _) if *s == slot)
        })
    }

    /// Checks if the any block in slot `slot` has received a `NotarizeFallback` certificate, if so return
    /// the size of the certificate
    #[cfg(test)]
    fn slot_has_notarized_fallback(&self, slot: Slot) -> bool {
        self.completed_certificates.iter().any(
            |(cert_type, _)| matches!(cert_type, CertificateType::NotarizeFallback(s,_) if *s == slot),
        )
    }

    /// Checks if `slot` has a `Skip` certificate
    #[cfg(test)]
    fn skip_certified(&self, slot: Slot) -> bool {
        self.completed_certificates
            .contains_key(&CertificateType::Skip(slot))
    }

    #[cfg(test)]
    fn make_start_leader_decision(
        &self,
        my_leader_slot: Slot,
        parent_slot: Slot,
        first_alpenglow_slot: Slot,
    ) -> bool {
        // TODO: for GCE tests we WFSM on 1 so slot 1 is exempt
        let needs_notar_cert = parent_slot >= first_alpenglow_slot && parent_slot > 1;

        if needs_notar_cert
            && !self.slot_has_notarized_fallback(parent_slot)
            && !self.is_finalized(parent_slot)
        {
            error!("Missing notarization certificate {parent_slot}");
            return false;
        }

        let needs_skip_cert =
            // handles cases where we are entering the alpenglow epoch, where the first
            // slot in the epoch will pass my_leader_slot == parent_slot
            my_leader_slot != first_alpenglow_slot &&
            my_leader_slot != parent_slot.saturating_add(1);

        if needs_skip_cert {
            let begin_skip_slot = first_alpenglow_slot.max(parent_slot.saturating_add(1));
            for slot in begin_skip_slot..my_leader_slot {
                if !self.skip_certified(slot) {
                    error!(
                        "Missing skip certificate for {slot}, required for skip certificate from \
                         {begin_skip_slot} to build {my_leader_slot}"
                    );
                    return false;
                }
            }
        }

        true
    }

    /// Cleanup any old slots from the certificate pool
    pub(crate) fn prune_old_state(&mut self, root_slot: Slot) {
        // `completed_certificates`` now only contains entries >= `slot`
        self.completed_certificates
            .retain(|cert_type, _| match cert_type {
                CertificateType::Finalize(s)
                | CertificateType::FinalizeFast(s, _)
                | CertificateType::Notarize(s, _)
                | CertificateType::NotarizeFallback(s, _)
                | CertificateType::Skip(s)
                | CertificateType::Genesis(s, _) => s >= &root_slot,
            });
        self.vote_pools = self.vote_pools.split_off(&root_slot);
        self.slot_stake_counters_map = self.slot_stake_counters_map.split_off(&root_slot);
        self.parent_ready_tracker.set_root(root_slot);
    }

    pub(crate) fn maybe_report(&mut self) {
        self.stats.maybe_report();
    }

    pub(crate) fn get_certs_for_standstill(&self) -> Vec<Arc<Certificate>> {
        let (highest_finalized_with_notarize_slot, has_fast_finalize) =
            self.highest_finalized_with_notarize.unwrap_or((0, false));
        self.completed_certificates
            .iter()
            .filter_map(|(cert_type, cert)| {
                let cert_to_send = match (
                    cert_type.slot().cmp(&highest_finalized_with_notarize_slot),
                    cert_type,
                    has_fast_finalize,
                ) {
                    (Ordering::Greater, _, _)
                    | (
                        Ordering::Equal,
                        CertificateType::Finalize(_) | CertificateType::Notarize(_, _),
                        false,
                    )
                    | (Ordering::Equal, CertificateType::FinalizeFast(_, _), true) => {
                        Some(cert.clone())
                    }
                    (Ordering::Equal, CertificateType::FinalizeFast(_, _), false) => {
                        panic!("Should not happen while certificate pool is single threaded")
                    }
                    _ => None,
                };
                if cert_to_send.is_some() {
                    trace!(
                        "{}: Refreshing certificate {:?}",
                        self.cluster_info.id(),
                        cert_type
                    );
                }
                cert_to_send
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::consensus_message::{VoteMessage, BLS_KEYPAIR_DERIVE_SEED},
        solana_bls_signatures::{
            keypair::Keypair as BLSKeypair, Pubkey as BLSPubkey, Signature as BLSSignature,
            VerifiableSignature,
        },
        solana_clock::Slot,
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_runtime::{
            bank::{Bank, NewBankOptions},
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        std::sync::{Arc, RwLock},
        test_case::test_case,
    };

    fn new_cluster_info() -> Arc<ClusterInfo> {
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ))
    }

    fn dummy_vote_message(
        keypairs: &[ValidatorVoteKeypairs],
        vote: &Vote,
        rank: usize,
    ) -> ConsensusMessage {
        let bls_keypair =
            BLSKeypair::derive_from_signer(&keypairs[rank].vote_keypair, BLS_KEYPAIR_DERIVE_SEED)
                .unwrap();
        let signature: BLSSignature = bls_keypair
            .sign(bincode::serialize(vote).unwrap().as_slice())
            .into();
        ConsensusMessage::new_vote(*vote, signature, rank as u16)
    }

    fn create_bank(slot: Slot, parent: Arc<Bank>, pubkey: &Pubkey) -> Bank {
        Bank::new_from_parent_with_options(parent, pubkey, slot, NewBankOptions::default())
    }

    fn create_bank_forks(validator_keypairs: &[ValidatorVoteKeypairs]) -> Arc<RwLock<BankForks>> {
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        BankForks::new_rw_arc(bank0)
    }

    fn create_initial_state() -> (
        Vec<ValidatorVoteKeypairs>,
        ConsensusPool,
        Arc<RwLock<BankForks>>,
    ) {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let bank_forks = create_bank_forks(&validator_keypairs);
        let root_bank = bank_forks.read().unwrap().root_bank();
        (
            validator_keypairs,
            ConsensusPool::new_from_root_bank(new_cluster_info(), &root_bank),
            bank_forks,
        )
    }

    fn add_certificate(
        pool: &mut ConsensusPool,
        bank: &Bank,
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote: Vote,
    ) {
        for rank in 0..6 {
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(validator_keypairs, &vote, rank),
                &mut vec![],
            )
            .unwrap();
        }
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(validator_keypairs, &vote, 6),
            &mut vec![],
        )
        .unwrap();
        match vote {
            Vote::Notarize(vote) => assert_eq!(pool.highest_notarized_slot(), vote.slot),
            Vote::NotarizeFallback(vote) => assert_eq!(pool.highest_notarized_slot(), vote.slot),
            Vote::Skip(vote) => assert_eq!(pool.highest_skip_slot(), vote.slot),
            Vote::SkipFallback(vote) => assert_eq!(pool.highest_skip_slot(), vote.slot),
            Vote::Finalize(vote) => assert_eq!(pool.highest_finalized_slot(), vote.slot),
            Vote::Genesis(_) => {}
        }
    }

    fn add_skip_vote_range(
        pool: &mut ConsensusPool,
        root_bank: &Bank,
        start: Slot,
        end: Slot,
        keypairs: &[ValidatorVoteKeypairs],
        rank: usize,
    ) {
        for slot in start..=end {
            let vote = Vote::new_skip_vote(slot);
            pool.add_message(
                root_bank.epoch_schedule(),
                root_bank.epoch_stakes_map(),
                root_bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(keypairs, &vote, rank),
                &mut vec![],
            )
            .unwrap();
        }
    }

    #[test]
    fn test_make_decision_leader_does_not_start_if_notarization_missing() {
        let (_, pool, _) = create_initial_state();

        // No notarization set, pool is default
        let parent_slot = 2;
        let my_leader_slot = 3;
        let first_alpenglow_slot = 0;
        let decision =
            pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot);
        assert!(
            !decision,
            "Leader should not be allowed to start without notarization"
        );
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_1() {
        let (_, pool, _) = create_initial_state();

        // If parent_slot == 0, you don't need a notarization certificate
        // Because leader_slot == parent_slot + 1, you don't need a skip certificate
        let parent_slot = 0;
        let my_leader_slot = 1;
        let first_alpenglow_slot = 0;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_2() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // If parent_slot < first_alpenglow_slot, and parent_slot > 0
        // no notarization certificate is required, but a skip
        // certificate will be
        let parent_slot = 1;
        let my_leader_slot = 3;
        let first_alpenglow_slot = 2;

        assert!(!pool.make_start_leader_decision(
            my_leader_slot,
            parent_slot,
            first_alpenglow_slot,
        ));

        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_skip_vote(first_alpenglow_slot),
        );

        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_3() {
        let (_, pool, _) = create_initial_state();
        // If parent_slot == first_alpenglow_slot, and
        // first_alpenglow_slot > 0, you need a notarization certificate
        let parent_slot = 2;
        let my_leader_slot = 3;
        let first_alpenglow_slot = 2;
        assert!(!pool.make_start_leader_decision(
            my_leader_slot,
            parent_slot,
            first_alpenglow_slot,
        ));
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_4() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // If parent_slot < first_alpenglow_slot, and parent_slot == 0,
        // no notarization certificate is required, but a skip certificate will
        // be
        let parent_slot = 0;
        let my_leader_slot = 2;
        let first_alpenglow_slot = 1;

        assert!(!pool.make_start_leader_decision(
            my_leader_slot,
            parent_slot,
            first_alpenglow_slot,
        ));

        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_skip_vote(first_alpenglow_slot),
        );
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_5() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // Valid skip certificate for 1-9 exists
        for slot in 1..=9 {
            add_certificate(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                &validator_keypairs,
                Vote::new_skip_vote(slot),
            );
        }

        // Parent slot is equal to 0, so no notarization certificate required
        let my_leader_slot = 10;
        let parent_slot = 0;
        let first_alpenglow_slot = 0;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_first_alpenglow_slot_edge_case_6() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // Valid skip certificate for 1-9 exists
        for slot in 1..=9 {
            add_certificate(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                &validator_keypairs,
                Vote::new_skip_vote(slot),
            );
        }
        // Parent slot is less than first_alpenglow_slot, so no notarization certificate required
        let my_leader_slot = 10;
        let parent_slot = 4;
        let first_alpenglow_slot = 5;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_leader_does_not_start_if_skip_certificate_missing() {
        let (validator_keypairs, mut pool, _) = create_initial_state();

        let bank_forks = create_bank_forks(&validator_keypairs);
        let my_pubkey = validator_keypairs[0].vote_keypair.pubkey();

        // Create bank 5
        let bank = create_bank(5, bank_forks.read().unwrap().get(0).unwrap(), &my_pubkey);
        bank.freeze();
        bank_forks.write().unwrap().insert(bank);

        // Notarize slot 5
        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_notarization_vote(5, Hash::default()),
        );
        assert_eq!(pool.highest_notarized_slot(), 5);

        // No skip certificate for 6-10
        let my_leader_slot = 10;
        let parent_slot = 5;
        let first_alpenglow_slot = 0;
        let decision =
            pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot);
        assert!(
            !decision,
            "Leader should not be allowed to start if a skip certificate is missing"
        );
    }

    #[test]
    fn test_make_decision_leader_starts_when_no_skip_required() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // Notarize slot 5
        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_notarization_vote(5, Hash::default()),
        );
        assert_eq!(pool.highest_notarized_slot(), 5);

        // Leader slot is just +1 from notarized slot (no skip needed)
        let my_leader_slot = 6;
        let parent_slot = 5;
        let first_alpenglow_slot = 0;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_leader_starts_if_notarized_and_skips_valid() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // Notarize slot 5
        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_notarization_vote(5, Hash::default()),
        );
        assert_eq!(pool.highest_notarized_slot(), 5);

        // Valid skip certificate for 6-9 exists
        for slot in 6..=9 {
            add_certificate(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                &validator_keypairs,
                Vote::new_skip_vote(slot),
            );
        }

        let my_leader_slot = 10;
        let parent_slot = 5;
        let first_alpenglow_slot = 0;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_make_decision_leader_starts_if_skip_range_superset() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // Notarize slot 5
        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_notarization_vote(5, Hash::default()),
        );
        assert_eq!(pool.highest_notarized_slot(), 5);

        // Valid skip certificate for 4-9 exists
        // Should start leader block even if the beginning of the range is from
        // before your last notarized slot
        for slot in 4..=9 {
            add_certificate(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                &validator_keypairs,
                Vote::new_skip_fallback_vote(slot),
            );
        }

        let my_leader_slot = 10;
        let parent_slot = 5;
        let first_alpenglow_slot = 0;
        assert!(pool.make_start_leader_decision(my_leader_slot, parent_slot, first_alpenglow_slot));
    }

    #[test]
    fn test_add_vote_and_create_new_certificate_with_types() {
        let slot = 5;
        let vote = Vote::new_finalization_vote(slot);
        let cert_types = vec![CertificateType::Finalize(slot)];
        do_test_add_vote_and_create_new_certificate_with_types(vote, cert_types);

        let slot = 6;
        let block_id = Hash::new_unique();
        let vote = Vote::new_notarization_vote(slot, block_id);
        let cert_types = vec![
            CertificateType::Notarize(slot, block_id),
            CertificateType::NotarizeFallback(slot, block_id),
        ];
        do_test_add_vote_and_create_new_certificate_with_types(vote, cert_types);

        let slot = 7;
        let block_id = Hash::new_unique();
        let vote = Vote::new_notarization_fallback_vote(slot, block_id);
        let cert_types = vec![CertificateType::NotarizeFallback(slot, block_id)];
        do_test_add_vote_and_create_new_certificate_with_types(vote, cert_types);

        let slot = 8;
        let vote = Vote::new_skip_vote(slot);
        let cert_types = vec![CertificateType::Skip(slot)];
        do_test_add_vote_and_create_new_certificate_with_types(vote, cert_types);

        let slot = 9;
        let vote = Vote::new_skip_fallback_vote(slot);
        let cert_types = vec![CertificateType::Skip(slot)];
        do_test_add_vote_and_create_new_certificate_with_types(vote, cert_types);
    }

    fn do_test_add_vote_and_create_new_certificate_with_types(
        vote: Vote,
        expected_cert_types: Vec<CertificateType>,
    ) {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let my_validator_ix = 5;
        let highest_slot_fn = match &vote {
            Vote::Finalize(_) => |pool: &ConsensusPool| pool.highest_finalized_slot(),
            Vote::Notarize(_) => |pool: &ConsensusPool| pool.highest_notarized_slot(),
            Vote::NotarizeFallback(_) => |pool: &ConsensusPool| pool.highest_notarized_slot(),
            Vote::Skip(_) => |pool: &ConsensusPool| pool.highest_skip_slot(),
            Vote::SkipFallback(_) => |pool: &ConsensusPool| pool.highest_skip_slot(),
            Vote::Genesis(_) => |_pool: &ConsensusPool| 0,
        };
        let bank = bank_forks.read().unwrap().root_bank();
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, my_validator_ix),
            &mut vec![],
        )
        .unwrap();
        let slot = vote.slot();
        assert!(highest_slot_fn(&pool) < slot);
        // Same key voting again shouldn't make a certificate
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, my_validator_ix),
            &mut vec![],
        )
        .unwrap();
        assert!(highest_slot_fn(&pool) < slot);
        for rank in 0..4 {
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut vec![],
            )
            .unwrap();
        }
        assert!(highest_slot_fn(&pool) < slot);
        let new_validator_ix = 6;
        let (new_finalized_slot, certs_to_send) = pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, new_validator_ix),
                &mut vec![],
            )
            .unwrap();
        if vote.is_finalize() {
            assert_eq!(new_finalized_slot, Some(slot));
        } else {
            assert!(new_finalized_slot.is_none());
        }
        // Assert certs_to_send contains the expected certificate types
        for expected_cert_type in expected_cert_types {
            assert!(certs_to_send
                .iter()
                .any(|cert| { cert.cert_type == expected_cert_type }));
        }
        assert_eq!(highest_slot_fn(&pool), slot);
        // Now add the same certificate again, this should silently exit.
        for cert in certs_to_send {
            let (new_finalized_slot, certs_to_send) = pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    ConsensusMessage::Certificate((*cert).clone()),
                    &mut vec![],
                )
                .unwrap();
            assert!(new_finalized_slot.is_none());
            assert_eq!(certs_to_send, []);
        }
    }

    #[test_case(CertificateType::Finalize(5), Vote::new_finalization_vote(5))]
    #[test_case(
        CertificateType::FinalizeFast(6, Hash::default()),
        Vote::new_notarization_vote(6, Hash::default())
    )]
    #[test_case(
        CertificateType::Notarize(6, Hash::default()),
        Vote::new_notarization_vote(6, Hash::default())
    )]
    #[test_case(
        CertificateType::NotarizeFallback(7, Hash::default()),
        Vote::new_notarization_fallback_vote(7, Hash::default())
    )]
    #[test_case(CertificateType::Skip(8), Vote::new_skip_vote(8))]
    fn test_add_certificate_with_types(cert_type: CertificateType, vote: Vote) {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        let cert = Certificate {
            cert_type,
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        let bank = bank_forks.read().unwrap().root_bank();
        let message = ConsensusMessage::Certificate(cert.clone());
        // Add the certificate to the pool
        let (new_finalized_slot, certs_to_send) = pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                message.clone(),
                &mut vec![],
            )
            .unwrap();
        // Because this is the first certificate of this type, it should be sent out.
        if matches!(cert_type, CertificateType::Finalize(_))
            || matches!(cert_type, CertificateType::FinalizeFast(_, _))
        {
            assert_eq!(new_finalized_slot, Some(cert_type.slot()));
        } else {
            assert!(new_finalized_slot.is_none());
        }
        assert_eq!(certs_to_send.len(), 1);
        assert_eq!(*certs_to_send[0], cert);

        // Adding the cert again will not trigger another send
        let (new_finalized_slot, certs_to_send) = pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                message,
                &mut vec![],
            )
            .unwrap();
        assert!(new_finalized_slot.is_none());
        assert_eq!(certs_to_send, []);

        // Now add the vote from everyone else, this will not trigger a certificate send
        for rank in 0..validator_keypairs.len() {
            let (_, certs_to_send) = pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut vec![],
                )
                .unwrap();
            assert!(!certs_to_send
                .iter()
                .any(|cert| { cert.cert_type == cert_type }));
        }
    }

    #[test]
    fn test_add_vote_zero_stake() {
        let (_, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        let err = pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Vote(VoteMessage {
                    vote: Vote::new_skip_vote(5),
                    rank: 100,
                    signature: BLSSignature::default(),
                }),
                &mut vec![],
            )
            .unwrap_err();
        assert_eq!(
            err,
            AddMessageError::Internal("Invalid rank: 100".to_string())
        );
    }

    fn assert_single_certificate_range(
        pool: &ConsensusPool,
        exp_range_start: Slot,
        exp_range_end: Slot,
    ) {
        for i in exp_range_start..=exp_range_end {
            assert!(pool.skip_certified(i));
        }
    }

    #[test]
    fn test_consecutive_slots() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        add_certificate(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            &validator_keypairs,
            Vote::new_skip_vote(15),
        );
        assert_eq!(pool.highest_skip_slot(), 15);

        let bank = bank_forks.read().unwrap().root_bank();
        for i in 0..validator_keypairs.len() {
            let slot = (i as u64).saturating_add(16);
            let vote = Vote::new_skip_vote(slot);
            // These should not extend the skip range
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, i),
                &mut vec![],
            )
            .unwrap();
        }

        assert_single_certificate_range(&pool, 15, 15);
    }

    #[test]
    fn test_multi_skip_cert() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // We have 10 validators, 40% voted for (5, 15)
        for rank in 0..4 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                15,
                &validator_keypairs,
                rank,
            );
        }
        // 30% voted for (5, 8)
        for rank in 4..7 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                8,
                &validator_keypairs,
                rank,
            );
        }
        // The rest voted for (11, 15)
        for rank in 7..10 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                11,
                15,
                &validator_keypairs,
                rank,
            );
        }
        // Test slots from 5 to 15, [5, 8] and [11, 15] should be certified, the others aren't
        for slot in 5..9 {
            assert!(pool.skip_certified(slot));
        }
        for slot in 9..11 {
            assert!(!pool.skip_certified(slot));
        }
        for slot in 11..=15 {
            assert!(pool.skip_certified(slot));
        }
    }

    #[test]
    fn test_add_multiple_votes() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        // 10 validators, half vote for (5, 15), the other (20, 30)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                15,
                &validator_keypairs,
                rank,
            );
        }
        for rank in 5..10 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                20,
                30,
                &validator_keypairs,
                rank,
            );
        }
        assert_eq!(pool.highest_skip_slot(), 0);

        // Now the first half vote for (5, 30)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                30,
                &validator_keypairs,
                rank,
            );
        }
        assert_single_certificate_range(&pool, 20, 30);
    }

    #[test]
    fn test_add_multiple_disjoint_votes() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        // 50% of the validators vote for (1, 10)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                1,
                10,
                &validator_keypairs,
                rank,
            );
        }
        let bank = bank_forks.read().unwrap().root_bank();
        // 10% vote for skip 2
        let vote = Vote::new_skip_vote(2);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 6),
            &mut vec![],
        )
        .unwrap();
        assert_eq!(pool.highest_skip_slot(), 2);

        assert_single_certificate_range(&pool, 2, 2);
        // 10% vote for skip 4
        let vote = Vote::new_skip_vote(4);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 7),
            &mut vec![],
        )
        .unwrap();
        assert_eq!(pool.highest_skip_slot(), 4);

        assert_single_certificate_range(&pool, 2, 2);
        assert_single_certificate_range(&pool, 4, 4);
        // 10% vote for skip 3
        let vote = Vote::new_skip_vote(3);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 8),
            &mut vec![],
        )
        .unwrap();
        assert_eq!(pool.highest_skip_slot(), 4);
        assert_single_certificate_range(&pool, 2, 4);
        assert!(pool.skip_certified(3));
        // Let the last 10% vote for (3, 10) now
        add_skip_vote_range(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            3,
            10,
            &validator_keypairs,
            8,
        );
        assert_eq!(pool.highest_skip_slot(), 10);
        assert_single_certificate_range(&pool, 2, 10);
        assert!(pool.skip_certified(7));
    }

    #[test]
    fn test_update_existing_singleton_vote() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        // 50% voted on (1, 6)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                1,
                6,
                &validator_keypairs,
                rank,
            );
        }
        let bank = bank_forks.read().unwrap().root_bank();
        // Range expansion on a singleton vote should be ok
        let vote = Vote::new_skip_vote(1);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 6),
            &mut vec![],
        )
        .unwrap();
        assert_eq!(pool.highest_skip_slot(), 1);
        add_skip_vote_range(
            &mut pool,
            &bank_forks.read().unwrap().root_bank(),
            1,
            6,
            &validator_keypairs,
            6,
        );
        assert_eq!(pool.highest_skip_slot(), 6);
        assert_single_certificate_range(&pool, 1, 6);
    }

    #[test]
    fn test_update_existing_vote() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        // 50% voted for (10, 25)
        for rank in 0..5 {
            add_skip_vote_range(&mut pool, &bank, 10, 25, &validator_keypairs, rank);
        }

        add_skip_vote_range(&mut pool, &bank, 10, 20, &validator_keypairs, 6);
        assert_eq!(pool.highest_skip_slot(), 20);
        assert_single_certificate_range(&pool, 10, 20);

        // AlreadyExists, silently fail
        let vote = Vote::new_skip_vote(20);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 6),
            &mut vec![],
        )
        .unwrap();
    }

    #[test]
    fn test_threshold_not_reached() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        // half voted (5, 15) and the other half voted (20, 30)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                15,
                &validator_keypairs,
                rank,
            );
        }
        for rank in 5..10 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                20,
                30,
                &validator_keypairs,
                rank,
            );
        }
        for slot in 5..31 {
            assert!(!pool.skip_certified(slot));
        }
    }

    #[test]
    fn test_update_and_skip_range_certify() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        // half voted (5, 15) and the other half voted (10, 30)
        for rank in 0..5 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                5,
                15,
                &validator_keypairs,
                rank,
            );
        }
        for rank in 5..10 {
            add_skip_vote_range(
                &mut pool,
                &bank_forks.read().unwrap().root_bank(),
                10,
                30,
                &validator_keypairs,
                rank,
            );
        }
        for slot in 5..10 {
            assert!(!pool.skip_certified(slot));
        }
        for slot in 16..31 {
            assert!(!pool.skip_certified(slot));
        }
        assert_single_certificate_range(&pool, 10, 15);
    }

    #[test]
    fn test_safe_to_notar() {
        agave_logger::setup();
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        let (my_vote_key, _, _) =
            get_key_and_stakes(bank.epoch_schedule(), bank.epoch_stakes_map(), 0, 0).unwrap();

        // Create bank 2
        let slot = 2;
        let block_id = Hash::new_unique();

        // Add a skip from myself.
        let vote = Vote::new_skip_vote(2);
        let mut new_events = vec![];
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &my_vote_key,
            dummy_vote_message(&validator_keypairs, &vote, 0),
            &mut new_events,
        )
        .unwrap();
        assert!(new_events.is_empty());

        // 40% notarized, should succeed
        for rank in 1..5 {
            let vote = Vote::new_notarization_vote(2, block_id);
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut new_events,
            )
            .unwrap();
        }
        assert_eq!(new_events.len(), 1);
        match new_events[0] {
            VotorEvent::SafeToNotar((event_slot, event_block_id)) => {
                assert_eq!(block_id, event_block_id);
                assert_eq!(slot, event_slot);
            }
            _ => {
                panic!("Expected SafeToNotar event");
            }
        }
        new_events.clear();

        // Create bank 3
        let slot = 3;
        let block_id = Hash::new_unique();

        // Add 20% notarize, but no vote from myself, should fail
        for rank in 1..3 {
            let vote = Vote::new_notarization_vote(3, block_id);
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut new_events,
            )
            .unwrap();
        }
        assert!(new_events.is_empty());

        // Add a notarize from myself for some other block, but still not enough notar or skip, should fail.
        let vote = Vote::new_notarization_vote(3, Hash::new_unique());
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &my_vote_key,
            dummy_vote_message(&validator_keypairs, &vote, 0),
            &mut new_events,
        )
        .unwrap();
        assert!(new_events.is_empty());

        // Now add 40% skip, should succeed
        // Funny thing is in this case we will also get SafeToSkip(3)
        for rank in 3..7 {
            let vote = Vote::new_skip_vote(3);
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut new_events,
            )
            .unwrap();
        }
        assert_eq!(new_events.len(), 2);
        match new_events[0] {
            VotorEvent::SafeToSkip(event_slot) => {
                assert_eq!(slot, event_slot);
            }
            _ => {
                panic!("Expected SafeToSkip event");
            }
        }
        match new_events[1] {
            VotorEvent::SafeToNotar((event_slot, event_block_id)) => {
                assert_eq!(block_id, event_block_id);
                assert_eq!(slot, event_slot);
            }
            _ => panic!("Expected SafeToNotar event"),
        }
        new_events.clear();

        // Add 20% notarization for another block, we should notify on new block_id
        // but not on the same block_id because we already sent the event
        let duplicate_block_id = Hash::new_unique();
        for rank in 7..9 {
            let vote = Vote::new_notarization_vote(3, duplicate_block_id);
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut new_events,
            )
            .unwrap();
        }

        assert_eq!(new_events.len(), 1);
        match new_events[0] {
            VotorEvent::SafeToNotar((event_slot, event_block_id)) => {
                assert_eq!(duplicate_block_id, event_block_id);
                assert_eq!(slot, event_slot);
            }
            _ => panic!("Expected SafeToNotar event"),
        }
    }

    #[test]
    fn test_safe_to_skip() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        let (my_vote_key, _, _) =
            get_key_and_stakes(bank.epoch_schedule(), bank.epoch_stakes_map(), 0, 0).unwrap();
        let slot = 2;
        let mut new_events = vec![];

        // Add a notarize from myself.
        let block_id = Hash::new_unique();
        let vote = Vote::new_notarization_vote(2, block_id);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &my_vote_key,
            dummy_vote_message(&validator_keypairs, &vote, 0),
            &mut new_events,
        )
        .unwrap();
        // Should still fail because there are no other votes.
        assert!(new_events.is_empty());
        // Add 50% skip, should succeed
        for rank in 1..6 {
            let vote = Vote::new_skip_vote(2);
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, rank),
                &mut new_events,
            )
            .unwrap();
        }
        assert_eq!(new_events.len(), 1);
        match new_events[0] {
            VotorEvent::SafeToSkip(event_slot) => assert_eq!(slot, event_slot),
            _ => panic!("Expected SafeToSkip event"),
        }
        new_events.clear();
        // Add 10% more notarize, will not send new SafeToSkip because the event was already sent
        let vote = Vote::new_notarization_vote(2, block_id);
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            dummy_vote_message(&validator_keypairs, &vote, 6),
            &mut new_events,
        )
        .unwrap();
        assert!(new_events.is_empty());
    }

    #[test]
    fn test_handle_new_root() {
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let bank_forks = create_bank_forks(&validator_keypairs);
        let mut pool = ConsensusPool::new_from_root_bank(
            new_cluster_info(),
            &bank_forks.read().unwrap().root_bank(),
        );

        let root_bank = bank_forks.read().unwrap().root_bank();
        // Add a skip cert on slot 1 and finalize cert on slot 2
        let cert_1 = Certificate {
            cert_type: CertificateType::Skip(1),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            root_bank.epoch_schedule(),
            root_bank.epoch_stakes_map(),
            root_bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_1.clone()),
            &mut vec![],
        )
        .unwrap();
        let cert_2 = Certificate {
            cert_type: CertificateType::FinalizeFast(2, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            root_bank.epoch_schedule(),
            root_bank.epoch_stakes_map(),
            root_bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_2.clone()),
            &mut vec![],
        )
        .unwrap();
        assert!(pool.skip_certified(1));
        assert!(pool.is_finalized(2));

        let new_bank = Arc::new(create_bank(2, root_bank, &Pubkey::new_unique()));
        pool.prune_old_state(new_bank.slot());
        // Check that cert for 1 is gone, but cert for 2 is still there
        assert!(!pool.skip_certified(1));
        assert!(pool.is_finalized(2));
        let new_bank = Arc::new(create_bank(3, new_bank, &Pubkey::new_unique()));
        pool.prune_old_state(new_bank.slot());
        // Now both certs should be gone
        assert!(!pool.skip_certified(1));
        assert!(!pool.is_finalized(2));
        // Send a vote on slot 1, it should be rejected
        let vote = Vote::new_skip_vote(1);
        assert!(pool
            .add_message(
                new_bank.epoch_schedule(),
                new_bank.epoch_stakes_map(),
                new_bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 0),
                &mut vec![]
            )
            .is_err());

        // Send a cert on slot 2, it should be rejected
        let cert_type = CertificateType::Notarize(2, Hash::new_unique());
        let cert = ConsensusMessage::Certificate(Certificate {
            cert_type,
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        });
        assert!(pool
            .add_message(
                new_bank.epoch_schedule(),
                new_bank.epoch_stakes_map(),
                new_bank.slot(),
                &Pubkey::new_unique(),
                cert,
                &mut vec![]
            )
            .is_err());
    }

    #[test]
    fn test_get_certs_for_standstill() {
        let (_, mut pool, bank_forks) = create_initial_state();

        // Should return empty vector if no certificates
        assert!(pool.get_certs_for_standstill().is_empty());

        // Add notar-fallback cert on 3 and finalize cert on 4
        let cert_3 = Certificate {
            cert_type: CertificateType::NotarizeFallback(3, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        let bank = bank_forks.read().unwrap().root_bank();
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_3.clone()),
            &mut vec![],
        )
        .unwrap();
        let cert_4 = Certificate {
            cert_type: CertificateType::Finalize(4),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_4.clone()),
            &mut vec![],
        )
        .unwrap();
        // Should return both certificates
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 3
            && matches!(cert.cert_type, CertificateType::NotarizeFallback(_, _))));
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 4
            && matches!(cert.cert_type, CertificateType::Finalize(_))));

        // Add Notarize cert on 5
        let cert_5 = Certificate {
            cert_type: CertificateType::Notarize(5, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_5.clone()),
            &mut vec![],
        )
        .unwrap();

        // Add Finalize cert on 5
        let cert_5_finalize = Certificate {
            cert_type: CertificateType::Finalize(5),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_5_finalize.clone()),
            &mut vec![],
        )
        .unwrap();

        // Add FinalizeFast cert on 5
        let cert_5 = Certificate {
            cert_type: CertificateType::FinalizeFast(5, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_5.clone()),
            &mut vec![],
        )
        .unwrap();
        // Should return only FinalizeFast cert on 5
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 1);
        assert!(
            certs[0].cert_type.slot() == 5
                && matches!(certs[0].cert_type, CertificateType::FinalizeFast(_, _))
        );

        // Now add Notarize cert on 6
        let cert_6 = Certificate {
            cert_type: CertificateType::Notarize(6, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_6.clone()),
            &mut vec![],
        )
        .unwrap();
        // Should return certs on 5 and 6
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 5
            && matches!(cert.cert_type, CertificateType::FinalizeFast(_, _))));
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 6
            && matches!(cert.cert_type, CertificateType::Notarize(_, _))));

        // Add another Finalize cert on 6
        let cert_6_finalize = Certificate {
            cert_type: CertificateType::Finalize(6),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_6_finalize.clone()),
            &mut vec![],
        )
        .unwrap();
        // Add a NotarizeFallback cert on 6
        let cert_6_notarize_fallback = Certificate {
            cert_type: CertificateType::NotarizeFallback(6, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_6_notarize_fallback.clone()),
            &mut vec![],
        )
        .unwrap();
        // This should not be returned because 6 is the current highest finalized slot
        // only Notarize/Finalze/FinalizeFast should be returned
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 6
            && matches!(cert.cert_type, CertificateType::Finalize(_))));
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 6
            && matches!(cert.cert_type, CertificateType::Notarize(_, _))));

        // Add another skip on 7
        let cert_7 = Certificate {
            cert_type: CertificateType::Skip(7),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_7.clone()),
            &mut vec![],
        )
        .unwrap();
        // Should return certs on 6 and 7
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 3);
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 6
            && matches!(cert.cert_type, CertificateType::Finalize(_))));
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 6
            && matches!(cert.cert_type, CertificateType::Notarize(_, _))));
        assert!(certs
            .iter()
            .any(|cert| cert.cert_type.slot() == 7
                && matches!(cert.cert_type, CertificateType::Skip(_))));

        // Add Finalize then Notarize cert on 8
        let cert_8_finalize = Certificate {
            cert_type: CertificateType::Finalize(8),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_8_finalize),
            &mut vec![],
        )
        .unwrap();
        let cert_8_notarize = Certificate {
            cert_type: CertificateType::Notarize(8, Hash::new_unique()),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert_8_notarize),
            &mut vec![],
        )
        .unwrap();

        // Should only return certs on 8 now
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 8
            && matches!(cert.cert_type, CertificateType::Finalize(_))));
        assert!(certs.iter().any(|cert| cert.cert_type.slot() == 8
            && matches!(cert.cert_type, CertificateType::Notarize(_, _))));
    }

    #[test]
    fn test_new_parent_ready_with_certificates() {
        let (_, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        let mut events = vec![];

        // Add a notarization cert on slot 1 to 3
        let hash = Hash::new_unique();
        for slot in 1..=3 {
            let cert = Certificate {
                cert_type: CertificateType::Notarize(slot, hash),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert),
                &mut events,
            )
            .unwrap();
        }
        // events should now contain ParentReady for slot 4
        error!("Events: {events:?}");
        assert!(events
            .iter()
            .any(|event| matches!(event, VotorEvent::ParentReady {
                slot: 4,
                parent_block: (3, h)
            } if h == &hash)));
        events.clear();

        // Also works if we add FinalizeFast for slot 4 to 7
        for slot in 4..=7 {
            let cert = Certificate {
                cert_type: CertificateType::FinalizeFast(slot, hash),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert),
                &mut events,
            )
            .unwrap();
        }
        // events should now contain ParentReady for slot 8
        error!("Events: {events:?}");
        assert!(events
            .iter()
            .any(|event| matches!(event, VotorEvent::ParentReady {
                slot: 8,
                parent_block: (7, h)
            } if h == &hash)));
        events.clear();

        // NotarizeFallback on slot 8 to 10 and FinalizeFast on slot 11
        for slot in 8..=10 {
            let cert = Certificate {
                cert_type: CertificateType::NotarizeFallback(slot, hash),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert),
                &mut events,
            )
            .unwrap();
        }
        let cert = Certificate {
            cert_type: CertificateType::FinalizeFast(11, hash),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        pool.add_message(
            bank.epoch_schedule(),
            bank.epoch_stakes_map(),
            bank.slot(),
            &Pubkey::new_unique(),
            ConsensusMessage::Certificate(cert),
            &mut events,
        )
        .unwrap();
        // events should now contain ParentReady for slot 12
        error!("Events: {events:?}");
        assert!(events
            .iter()
            .any(|event| matches!(event, VotorEvent::ParentReady {
            slot: 12,
            parent_block: (11, h)
        } if h == &hash)));
    }

    #[test]
    fn test_vote_message_signature_verification() {
        let (validator_keypairs, _, _) = create_initial_state();
        let rank_to_test = 3;
        let vote = Vote::new_notarization_vote(42, Hash::new_unique());

        let consensus_message = dummy_vote_message(&validator_keypairs, &vote, rank_to_test);
        let ConsensusMessage::Vote(vote_message) = consensus_message else {
            panic!("Expected Vote message")
        };

        let validator_vote_keypair = &validator_keypairs[rank_to_test].vote_keypair;
        let bls_keypair =
            BLSKeypair::derive_from_signer(validator_vote_keypair, BLS_KEYPAIR_DERIVE_SEED)
                .unwrap();
        let bls_pubkey: BLSPubkey = bls_keypair.public;

        let signed_message = bincode::serialize(&vote).unwrap();

        vote_message
            .signature
            .verify(&bls_pubkey, &signed_message)
            .expect("BLS signature verification failed for VoteMessage");
    }
}
