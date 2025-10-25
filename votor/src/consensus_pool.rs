use {
    crate::{
        commitment::CommitmentError,
        common::{
            certificate_limits_and_vote_types, conflicting_types, vote_to_certificate_ids, Stake,
            VoteType, MAX_ENTRIES_PER_PUBKEY_FOR_NOTARIZE_LITE,
            MAX_ENTRIES_PER_PUBKEY_FOR_OTHER_TYPES,
        },
        consensus_pool::{
            parent_ready_tracker::ParentReadyTracker,
            slot_stake_counters::SlotStakeCounters,
            stats::ConsensusPoolStats,
            vote_certificate_builder::{CertificateError, VoteCertificateBuilder},
            vote_pool::{DuplicateBlockVotePool, SimpleVotePool, VotePool},
        },
        event::VotorEvent,
    },
    agave_votor_messages::{
        consensus_message::{
            Block, Certificate, CertificateMessage, CertificateType, ConsensusMessage, VoteMessage,
        },
        vote::Vote,
    },
    log::{error, trace},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::VersionedEpochStakes},
    std::{
        cmp::Ordering,
        collections::{BTreeMap, HashMap},
        sync::Arc,
    },
    thiserror::Error,
};

pub mod parent_ready_tracker;
mod slot_stake_counters;
mod stats;
mod vote_certificate_builder;
mod vote_pool;

pub type PoolId = (Slot, VoteType);

#[derive(Debug, Error, PartialEq)]
pub enum AddVoteError {
    #[error("Conflicting vote type: {0:?} vs existing {1:?} for slot: {2} pubkey: {3}")]
    ConflictingVoteType(VoteType, VoteType, Slot, Pubkey),

    #[error("Epoch stakes missing for epoch: {0}")]
    EpochStakesNotFound(Epoch),

    #[error("Unrooted slot")]
    UnrootedSlot,

    #[error("Slot in the future")]
    SlotInFuture,

    #[error("Certificate error: {0}")]
    Certificate(#[from] CertificateError),

    #[error("{0} channel disconnected")]
    ChannelDisconnected(String),

    #[error("Voting Service queue full")]
    VotingServiceChannelFull,

    #[error("Invalid rank: {0}")]
    InvalidRank(u16),
}

impl From<CommitmentError> for AddVoteError {
    fn from(_: CommitmentError) -> Self {
        AddVoteError::ChannelDisconnected("CommitmentSender".to_string())
    }
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

pub struct ConsensusPool {
    my_pubkey: Pubkey,
    // Vote pools to do bean counting for votes.
    vote_pools: BTreeMap<PoolId, VotePool>,
    /// Completed certificates
    completed_certificates: BTreeMap<Certificate, Arc<CertificateMessage>>,
    /// Tracks slots which have reached the parent ready condition:
    /// - They have a potential parent block with a NotarizeFallback certificate
    /// - All slots from the parent have a Skip certificate
    pub parent_ready_tracker: ParentReadyTracker,
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
    pub fn new_from_root_bank(my_pubkey: Pubkey, bank: &Bank) -> Self {
        // To account for genesis and snapshots we allow default block id until
        // block id can be serialized  as part of the snapshot
        let root_block = (bank.slot(), bank.block_id().unwrap_or_default());
        let parent_ready_tracker = ParentReadyTracker::new(my_pubkey, root_block);

        Self {
            my_pubkey,
            vote_pools: BTreeMap::new(),
            completed_certificates: BTreeMap::new(),
            highest_finalized_slot: None,
            highest_finalized_with_notarize: None,
            parent_ready_tracker,
            stats: ConsensusPoolStats::new(),
            slot_stake_counters_map: BTreeMap::new(),
        }
    }

    fn new_vote_pool(vote_type: VoteType) -> VotePool {
        match vote_type {
            VoteType::NotarizeFallback => VotePool::DuplicateBlockVotePool(
                DuplicateBlockVotePool::new(MAX_ENTRIES_PER_PUBKEY_FOR_NOTARIZE_LITE),
            ),
            VoteType::Notarize => VotePool::DuplicateBlockVotePool(DuplicateBlockVotePool::new(
                MAX_ENTRIES_PER_PUBKEY_FOR_OTHER_TYPES,
            )),
            _ => VotePool::SimpleVotePool(SimpleVotePool::default()),
        }
    }

    fn update_vote_pool(
        &mut self,
        vote: VoteMessage,
        validator_vote_key: Pubkey,
        validator_stake: Stake,
    ) -> Option<Stake> {
        let vote_type = VoteType::get_type(&vote.vote);
        let pool = self
            .vote_pools
            .entry((vote.vote.slot(), vote_type))
            .or_insert_with(|| Self::new_vote_pool(vote_type));
        match pool {
            VotePool::SimpleVotePool(pool) => {
                pool.add_vote(validator_vote_key, validator_stake, vote)
            }
            VotePool::DuplicateBlockVotePool(pool) => {
                pool.add_vote(validator_vote_key, validator_stake, vote)
            }
        }
    }

    /// For a new vote `slot` , `vote_type` checks if any
    /// of the related certificates are newly complete.
    /// For each newly constructed certificate
    /// - Insert it into `self.certificates`
    /// - Potentially update `self.highest_finalized_slot`,
    /// - If we have a new highest finalized slot, return it
    /// - update any newly created events
    fn update_certificates(
        &mut self,
        vote: &Vote,
        block_id: Option<Hash>,
        events: &mut Vec<VotorEvent>,
        total_stake: Stake,
    ) -> Result<Vec<Arc<CertificateMessage>>, AddVoteError> {
        let slot = vote.slot();
        let mut new_certificates_to_send = Vec::new();
        for cert_id in vote_to_certificate_ids(vote) {
            // If the certificate is already complete, skip it
            if self.completed_certificates.contains_key(&cert_id) {
                continue;
            }
            // Otherwise check whether the certificate is complete
            let (limit, vote_types) = certificate_limits_and_vote_types(cert_id);
            let accumulated_stake = vote_types
                .iter()
                .filter_map(|vote_type| {
                    Some(match self.vote_pools.get(&(slot, *vote_type))? {
                        VotePool::SimpleVotePool(pool) => pool.total_stake(),
                        VotePool::DuplicateBlockVotePool(pool) => {
                            pool.total_stake_by_block_id(block_id.as_ref().expect(
                                "Duplicate block pool for {vote_type:?} expects a block id for \
                                 certificate {cert_id:?}",
                            ))
                        }
                    })
                })
                .sum::<Stake>();
            if accumulated_stake as f64 / (total_stake as f64) < limit {
                continue;
            }
            let mut vote_certificate_builder = VoteCertificateBuilder::new(cert_id);
            vote_types.iter().for_each(|vote_type| {
                if let Some(vote_pool) = self.vote_pools.get(&(slot, *vote_type)) {
                    match vote_pool {
                        VotePool::SimpleVotePool(pool) => {
                            vote_certificate_builder.aggregate(pool.votes()).unwrap();
                        }
                        VotePool::DuplicateBlockVotePool(pool) => {
                            if let Some(votes) = pool.votes(block_id.as_ref().expect(
                                "Duplicate block pool for {vote_type:?} expects a block id for \
                                 certificate {cert_id:?}",
                            )) {
                                vote_certificate_builder.aggregate(votes).unwrap();
                            }
                        }
                    };
                }
            });
            let new_cert = Arc::new(vote_certificate_builder.build()?);
            self.insert_certificate(cert_id, new_cert.clone(), events);
            self.stats
                .incr_cert_type(new_cert.certificate.certificate_type(), true);
            new_certificates_to_send.push(new_cert);
        }
        Ok(new_certificates_to_send)
    }

    fn has_conflicting_vote(
        &self,
        slot: Slot,
        vote_type: VoteType,
        validator_vote_key: &Pubkey,
        block_id: &Option<Hash>,
    ) -> Option<VoteType> {
        for conflicting_type in conflicting_types(vote_type) {
            if let Some(pool) = self.vote_pools.get(&(slot, *conflicting_type)) {
                let is_conflicting = match pool {
                    // In a simple vote pool, just check if the validator previously voted at all. If so, that's a conflict
                    VotePool::SimpleVotePool(pool) => {
                        pool.has_prev_validator_vote(validator_vote_key)
                    }
                    // In a duplicate block vote pool, because some conflicts between things like Notarize and NotarizeFallback
                    // for different blocks are allowed, we need a more specific check.
                    // TODO: This can be made much cleaner/safer if VoteType carried the bank hash, block id so we
                    // could check which exact VoteType(blockid, bankhash) was the source of the conflict.
                    VotePool::DuplicateBlockVotePool(pool) => {
                        if let Some(block_id) = &block_id {
                            // Reject votes for the same block with a conflicting type, i.e.
                            // a NotarizeFallback vote for the same block as a Notarize vote.
                            pool.has_prev_validator_vote_for_block(validator_vote_key, block_id)
                        } else {
                            pool.has_prev_validator_vote(validator_vote_key)
                        }
                    }
                };
                if is_conflicting {
                    return Some(*conflicting_type);
                }
            }
        }
        None
    }

    fn insert_certificate(
        &mut self,
        cert_id: Certificate,
        cert: Arc<CertificateMessage>,
        events: &mut Vec<VotorEvent>,
    ) {
        trace!("{}: Inserting certificate {:?}", self.my_pubkey, cert_id);
        self.completed_certificates.insert(cert_id, cert);
        match cert_id {
            Certificate::NotarizeFallback(slot, block_id) => {
                self.parent_ready_tracker
                    .add_new_notar_fallback_or_stronger((slot, block_id), events);
            }
            Certificate::Skip(slot) => self.parent_ready_tracker.add_new_skip(slot, events),
            Certificate::Notarize(slot, block_id) => {
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
            Certificate::Finalize(slot) => {
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
            Certificate::FinalizeFast(slot, block_id) => {
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
        }
    }

    /// Adds the new vote the the certificate pool. If a new certificate is created
    /// as a result of this, send it via the `self.certificate_sender`
    ///
    /// Any new votor events that are a result of adding this new vote will be added
    /// to `events`.
    ///
    /// If this resulted in a new highest Finalize or FastFinalize certificate,
    /// return the slot
    pub fn add_message(
        &mut self,
        epoch_schedule: &EpochSchedule,
        epoch_stakes_map: &HashMap<Epoch, VersionedEpochStakes>,
        root_slot: Slot,
        my_vote_pubkey: &Pubkey,
        message: ConsensusMessage,
        events: &mut Vec<VotorEvent>,
    ) -> Result<(Option<Slot>, Vec<Arc<CertificateMessage>>), AddVoteError> {
        let current_highest_finalized_slot = self.highest_finalized_slot;
        let new_certficates_to_send = match message {
            ConsensusMessage::Vote(vote_message) => self.add_vote(
                epoch_schedule,
                epoch_stakes_map,
                root_slot,
                my_vote_pubkey,
                vote_message,
                events,
            )?,
            ConsensusMessage::Certificate(certificate_message) => {
                self.add_certificate(root_slot, &certificate_message, events)?
            }
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
    ) -> Result<Vec<Arc<CertificateMessage>>, AddVoteError> {
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
        let block_id = vote.block_id().map(|block_id| {
            if !matches!(vote, Vote::Notarize(_) | Vote::NotarizeFallback(_)) {
                panic!("expected Notarize or NotarizeFallback vote");
            }
            *block_id
        });
        let vote_type = VoteType::get_type(vote);
        if let Some(conflicting_type) =
            self.has_conflicting_vote(vote_slot, vote_type, &validator_vote_key, &block_id)
        {
            self.stats.conflicting_votes = self.stats.conflicting_votes.saturating_add(1);
            return Err(AddVoteError::ConflictingVoteType(
                vote_type,
                conflicting_type,
                vote_slot,
                validator_vote_key,
            ));
        }
        match self.update_vote_pool(vote_message, validator_vote_key, validator_stake) {
            None => {
                // No new vote pool entry was created, just return empty vec
                self.stats.exist_votes = self.stats.exist_votes.saturating_add(1);
                return Ok(vec![]);
            }
            Some(entry_stake) => {
                let fallback_vote_counters = self
                    .slot_stake_counters_map
                    .entry(vote_slot)
                    .or_insert_with(|| SlotStakeCounters::new(total_stake));
                fallback_vote_counters.add_vote(
                    vote,
                    entry_stake,
                    my_vote_pubkey == &validator_vote_key,
                    events,
                    &mut self.stats,
                );
            }
        }
        self.stats.incr_ingested_vote_type(vote_type);

        self.update_certificates(vote, block_id, events, total_stake)
    }

    fn add_certificate(
        &mut self,
        root_slot: Slot,
        certificate_message: &CertificateMessage,
        events: &mut Vec<VotorEvent>,
    ) -> Result<Vec<Arc<CertificateMessage>>, AddVoteError> {
        let certificate_id = certificate_message.certificate;
        self.stats.incoming_certs = self.stats.incoming_certs.saturating_add(1);
        if certificate_id.slot() < root_slot {
            self.stats.out_of_range_certs = self.stats.out_of_range_certs.saturating_add(1);
            return Err(AddVoteError::UnrootedSlot);
        }
        if self.completed_certificates.contains_key(&certificate_id) {
            self.stats.exist_certs = self.stats.exist_certs.saturating_add(1);
            return Ok(vec![]);
        }
        let new_certificate = Arc::new(certificate_message.clone());
        self.insert_certificate(certificate_id, new_certificate.clone(), events);

        self.stats
            .incr_cert_type(certificate_id.certificate_type(), false);

        Ok(vec![new_certificate])
    }

    /// Get the notarized block in `slot`
    pub fn get_notarized_block(&self, slot: Slot) -> Option<Block> {
        self.completed_certificates
            .iter()
            .find_map(|(cert_id, _)| match cert_id {
                Certificate::Notarize(s, block_id) if slot == *s => Some((*s, *block_id)),
                _ => None,
            })
    }

    #[cfg(test)]
    fn highest_notarized_slot(&self) -> Slot {
        // Return the max of CertificateType::Notarize and CertificateType::NotarizeFallback
        self.completed_certificates
            .iter()
            .filter_map(|(cert_id, _)| match cert_id {
                Certificate::Notarize(s, _) => Some(s),
                Certificate::NotarizeFallback(s, _) => Some(s),
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
            .filter_map(|(cert_id, _)| match cert_id {
                Certificate::Skip(s) => Some(s),
                _ => None,
            })
            .max()
            .copied()
            .unwrap_or(0)
    }

    pub fn highest_finalized_slot(&self) -> Slot {
        self.completed_certificates
            .iter()
            .filter_map(|(cert_id, _)| match cert_id {
                Certificate::Finalize(s) => Some(s),
                Certificate::FinalizeFast(s, _) => Some(s),
                _ => None,
            })
            .max()
            .copied()
            .unwrap_or(0)
    }

    /// Checks if any block in the slot `s` is finalized
    pub fn is_finalized(&self, slot: Slot) -> bool {
        self.completed_certificates.keys().any(|cert_id| {
            matches!(cert_id, Certificate::Finalize(s) | Certificate::FinalizeFast(s, _) if *s == slot)
        })
    }

    /// Checks if the any block in slot `slot` has received a `NotarizeFallback` certificate, if so return
    /// the size of the certificate
    #[cfg(test)]
    pub fn slot_has_notarized_fallback(&self, slot: Slot) -> bool {
        self.completed_certificates
            .iter()
            .any(|(cert_id, _)| matches!(cert_id, Certificate::NotarizeFallback(s,_) if *s == slot))
    }

    /// Checks if `slot` has a `Skip` certificate
    pub fn skip_certified(&self, slot: Slot) -> bool {
        self.completed_certificates
            .contains_key(&Certificate::Skip(slot))
    }

    #[cfg(test)]
    pub(crate) fn my_pubkey(&self) -> Pubkey {
        self.my_pubkey
    }

    #[cfg(test)]
    fn make_start_leader_decision(
        &self,
        my_leader_slot: Slot,
        parent_slot: Slot,
        first_alpenglow_slot: Slot,
    ) -> bool {
        // TODO: for GCE tests we WFSM on 1 so slot 1 is exempt
        let needs_notarization_certificate = parent_slot >= first_alpenglow_slot && parent_slot > 1;

        if needs_notarization_certificate
            && !self.slot_has_notarized_fallback(parent_slot)
            && !self.is_finalized(parent_slot)
        {
            error!("Missing notarization certificate {parent_slot}");
            return false;
        }

        let needs_skip_certificate =
            // handles cases where we are entering the alpenglow epoch, where the first
            // slot in the epoch will pass my_leader_slot == parent_slot
            my_leader_slot != first_alpenglow_slot &&
            my_leader_slot != parent_slot.saturating_add(1);

        if needs_skip_certificate {
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
    pub fn prune_old_state(&mut self, root_slot: Slot) {
        // `completed_certificates`` now only contains entries >= `slot`
        self.completed_certificates
            .retain(|cert_id, _| match cert_id {
                Certificate::Finalize(s)
                | Certificate::FinalizeFast(s, _)
                | Certificate::Notarize(s, _)
                | Certificate::NotarizeFallback(s, _)
                | Certificate::Skip(s) => s >= &root_slot,
            });
        self.vote_pools = self.vote_pools.split_off(&(root_slot, VoteType::Finalize));
        self.slot_stake_counters_map = self.slot_stake_counters_map.split_off(&root_slot);
        self.parent_ready_tracker.set_root(root_slot);
    }

    /// Updates the pubkey used for logging purposes only.
    /// This avoids the need to recreate the entire certificate pool since it's
    /// not distinguished by the pubkey.
    pub fn update_pubkey(&mut self, new_pubkey: Pubkey) {
        self.my_pubkey = new_pubkey;
        self.parent_ready_tracker.update_pubkey(new_pubkey);
    }

    pub fn maybe_report(&mut self) {
        self.stats.maybe_report();
    }

    pub fn get_certs_for_standstill(&self) -> Vec<Arc<CertificateMessage>> {
        let (highest_finalized_with_notarize_slot, has_fast_finalize) =
            self.highest_finalized_with_notarize.unwrap_or((0, false));
        self.completed_certificates
            .iter()
            .filter_map(|(cert_id, cert)| {
                let cert_to_send = match (
                    cert_id.slot().cmp(&highest_finalized_with_notarize_slot),
                    cert_id.certificate_type(),
                    has_fast_finalize,
                ) {
                    (Ordering::Greater, _, _)
                    | (
                        Ordering::Equal,
                        CertificateType::Finalize | CertificateType::Notarize,
                        false,
                    )
                    | (Ordering::Equal, CertificateType::FinalizeFast, true) => Some(cert.clone()),
                    (Ordering::Equal, CertificateType::FinalizeFast, false) => {
                        panic!("Should not happen while certificate pool is single threaded")
                    }
                    _ => None,
                };
                if cert_to_send.is_some() {
                    trace!("{}: Refreshing certificate {:?}", self.my_pubkey, cert_id);
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
        agave_votor_messages::consensus_message::{
            CertificateType, VoteMessage, BLS_KEYPAIR_DERIVE_SEED,
        },
        solana_bls_signatures::{
            keypair::Keypair as BLSKeypair, Pubkey as BLSPubkey, Signature as BLSSignature,
            VerifiableSignature,
        },
        solana_clock::Slot,
        solana_hash::Hash,
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
            ConsensusPool::new_from_root_bank(Pubkey::new_unique(), &root_bank),
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
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(validator_keypairs, &vote, rank),
                    &mut vec![]
                )
                .is_ok());
        }
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(validator_keypairs, &vote, 6),
                &mut vec![]
            )
            .is_ok());
        match vote {
            Vote::Notarize(vote) => assert_eq!(pool.highest_notarized_slot(), vote.slot()),
            Vote::NotarizeFallback(vote) => assert_eq!(pool.highest_notarized_slot(), vote.slot()),
            Vote::Skip(vote) => assert_eq!(pool.highest_skip_slot(), vote.slot()),
            Vote::SkipFallback(vote) => assert_eq!(pool.highest_skip_slot(), vote.slot()),
            Vote::Finalize(vote) => assert_eq!(pool.highest_finalized_slot(), vote.slot()),
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
            assert!(pool
                .add_message(
                    root_bank.epoch_schedule(),
                    root_bank.epoch_stakes_map(),
                    root_bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(keypairs, &vote, rank),
                    &mut vec![]
                )
                .is_ok());
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

    #[test_case(Vote::new_finalization_vote(5), vec![CertificateType::Finalize])]
    #[test_case(Vote::new_notarization_vote(6, Hash::new_unique()), vec![CertificateType::Notarize, CertificateType::NotarizeFallback])]
    #[test_case(Vote::new_notarization_fallback_vote(7, Hash::new_unique()), vec![CertificateType::NotarizeFallback])]
    #[test_case(Vote::new_skip_vote(8), vec![CertificateType::Skip])]
    #[test_case(Vote::new_skip_fallback_vote(9), vec![CertificateType::Skip])]
    fn test_add_vote_and_create_new_certificate_with_types(
        vote: Vote,
        expected_certificate_types: Vec<CertificateType>,
    ) {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let my_validator_ix = 5;
        let highest_slot_fn = match &vote {
            Vote::Finalize(_) => |pool: &ConsensusPool| pool.highest_finalized_slot(),
            Vote::Notarize(_) => |pool: &ConsensusPool| pool.highest_notarized_slot(),
            Vote::NotarizeFallback(_) => |pool: &ConsensusPool| pool.highest_notarized_slot(),
            Vote::Skip(_) => |pool: &ConsensusPool| pool.highest_skip_slot(),
            Vote::SkipFallback(_) => |pool: &ConsensusPool| pool.highest_skip_slot(),
        };
        let bank = bank_forks.read().unwrap().root_bank();
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, my_validator_ix),
                &mut vec![]
            )
            .is_ok());
        let slot = vote.slot();
        assert!(highest_slot_fn(&pool) < slot);
        // Same key voting again shouldn't make a certificate
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, my_validator_ix),
                &mut vec![]
            )
            .is_ok());
        assert!(highest_slot_fn(&pool) < slot);
        for rank in 0..4 {
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut vec![]
                )
                .is_ok());
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
        for cert_type in expected_certificate_types {
            assert!(certs_to_send.iter().any(|cert| {
                cert.certificate.certificate_type() == cert_type && cert.certificate.slot() == slot
            }));
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

    #[test_case(CertificateType::Finalize, Vote::new_finalization_vote(5))]
    #[test_case(
        CertificateType::FinalizeFast,
        Vote::new_notarization_vote(6, Hash::new_unique())
    )]
    #[test_case(
        CertificateType::Notarize,
        Vote::new_notarization_vote(6, Hash::new_unique())
    )]
    #[test_case(
        CertificateType::NotarizeFallback,
        Vote::new_notarization_fallback_vote(7, Hash::new_unique())
    )]
    #[test_case(CertificateType::Skip, Vote::new_skip_vote(8))]
    fn test_add_certificate_with_types(certificate_type: CertificateType, vote: Vote) {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();

        let certificate = Certificate::new(certificate_type, vote.slot(), vote.block_id().copied());

        let certificate_message = CertificateMessage {
            certificate,
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        let bank = bank_forks.read().unwrap().root_bank();
        let message = ConsensusMessage::Certificate(certificate_message.clone());
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
        if certificate_type == CertificateType::Finalize
            || certificate_type == CertificateType::FinalizeFast
        {
            assert_eq!(new_finalized_slot, Some(certificate.slot()));
        } else {
            assert!(new_finalized_slot.is_none());
        }
        assert_eq!(certs_to_send.len(), 1);
        assert_eq!(*certs_to_send[0], certificate_message);

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
                .any(|cert| { cert.certificate.certificate_type() == certificate_type }));
        }
    }

    #[test]
    fn test_add_vote_zero_stake() {
        let (_, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        assert_eq!(
            pool.add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Vote(VoteMessage {
                    vote: Vote::new_skip_vote(5),
                    rank: 100,
                    signature: BLSSignature::default(),
                }),
                &mut vec![]
            ),
            Err(AddVoteError::InvalidRank(100))
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
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, i),
                    &mut vec![]
                )
                .is_ok());
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
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 6),
                &mut vec![]
            )
            .is_ok());
        assert_eq!(pool.highest_skip_slot(), 2);

        assert_single_certificate_range(&pool, 2, 2);
        // 10% vote for skip 4
        let vote = Vote::new_skip_vote(4);
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 7),
                &mut vec![]
            )
            .is_ok());
        assert_eq!(pool.highest_skip_slot(), 4);

        assert_single_certificate_range(&pool, 2, 2);
        assert_single_certificate_range(&pool, 4, 4);
        // 10% vote for skip 3
        let vote = Vote::new_skip_vote(3);
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 8),
                &mut vec![]
            )
            .is_ok());
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
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 6),
                &mut vec![]
            )
            .is_ok());
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
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 6),
                &mut vec![]
            )
            .is_ok());
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
        solana_logger::setup();
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
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &my_vote_key,
                dummy_vote_message(&validator_keypairs, &vote, 0),
                &mut new_events
            )
            .is_ok());
        assert!(new_events.is_empty());

        // 40% notarized, should succeed
        for rank in 1..5 {
            let vote = Vote::new_notarization_vote(2, block_id);
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut new_events
                )
                .is_ok());
        }
        assert_eq!(new_events.len(), 1);
        if let VotorEvent::SafeToNotar((event_slot, event_block_id)) = new_events[0] {
            assert_eq!(block_id, event_block_id);
            assert_eq!(slot, event_slot);
        } else {
            panic!("Expected SafeToNotar event");
        }
        new_events.clear();

        // Create bank 3
        let slot = 3;
        let block_id = Hash::new_unique();

        // Add 20% notarize, but no vote from myself, should fail
        for rank in 1..3 {
            let vote = Vote::new_notarization_vote(3, block_id);
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut new_events
                )
                .is_ok());
        }
        assert!(new_events.is_empty());

        // Add a notarize from myself for some other block, but still not enough notar or skip, should fail.
        let vote = Vote::new_notarization_vote(3, Hash::new_unique());
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &my_vote_key,
                dummy_vote_message(&validator_keypairs, &vote, 0),
                &mut new_events
            )
            .is_ok());
        assert!(new_events.is_empty());

        // Now add 40% skip, should succeed
        // Funny thing is in this case we will also get SafeToSkip(3)
        for rank in 3..7 {
            let vote = Vote::new_skip_vote(3);
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut new_events
                )
                .is_ok());
        }
        assert_eq!(new_events.len(), 2);
        if let VotorEvent::SafeToSkip(event_slot) = new_events[0] {
            assert_eq!(slot, event_slot);
        } else {
            panic!("Expected SafeToSkip event");
        }
        if let VotorEvent::SafeToNotar((event_slot, event_block_id)) = new_events[1] {
            assert_eq!(block_id, event_block_id);
            assert_eq!(slot, event_slot);
        } else {
            panic!("Expected SafeToNotar event");
        }
        new_events.clear();

        // Add 20% notarization for another block, we should notify on new block_id
        // but not on the same block_id because we already sent the event
        let duplicate_block_id = Hash::new_unique();
        for rank in 7..9 {
            let vote = Vote::new_notarization_vote(3, duplicate_block_id);
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut new_events
                )
                .is_ok());
        }

        assert_eq!(new_events.len(), 1);
        if let VotorEvent::SafeToNotar((event_slot, event_block_id)) = new_events[0] {
            assert_eq!(duplicate_block_id, event_block_id);
            assert_eq!(slot, event_slot);
        } else {
            panic!("Expected SafeToNotar event");
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
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &my_vote_key,
                dummy_vote_message(&validator_keypairs, &vote, 0),
                &mut new_events
            )
            .is_ok());
        // Should still fail because there are no other votes.
        assert!(new_events.is_empty());
        // Add 50% skip, should succeed
        for rank in 1..6 {
            let vote = Vote::new_skip_vote(2);
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    dummy_vote_message(&validator_keypairs, &vote, rank),
                    &mut new_events
                )
                .is_ok());
        }
        assert_eq!(new_events.len(), 1);
        if let VotorEvent::SafeToSkip(event_slot) = new_events[0] {
            assert_eq!(slot, event_slot);
        } else {
            panic!("Expected SafeToSkip event");
        }
        new_events.clear();
        // Add 10% more notarize, will not send new SafeToSkip because the event was already sent
        let vote = Vote::new_notarization_vote(2, block_id);
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(&validator_keypairs, &vote, 6),
                &mut new_events
            )
            .is_ok());
        assert!(new_events.is_empty());
    }

    fn create_new_vote(vote_type: VoteType, slot: Slot) -> Vote {
        match vote_type {
            VoteType::Notarize => Vote::new_notarization_vote(slot, Hash::default()),
            VoteType::NotarizeFallback => {
                Vote::new_notarization_fallback_vote(slot, Hash::default())
            }
            VoteType::Skip => Vote::new_skip_vote(slot),
            VoteType::SkipFallback => Vote::new_skip_fallback_vote(slot),
            VoteType::Finalize => Vote::new_finalization_vote(slot),
        }
    }

    fn test_reject_conflicting_vote(
        pool: &mut ConsensusPool,
        bank: &Bank,
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote_type_1: VoteType,
        vote_type_2: VoteType,
        slot: Slot,
    ) {
        let vote_1 = create_new_vote(vote_type_1, slot);
        let vote_2 = create_new_vote(vote_type_2, slot);
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(validator_keypairs, &vote_1, 0),
                &mut vec![]
            )
            .is_ok());
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                dummy_vote_message(validator_keypairs, &vote_2, 0),
                &mut vec![]
            )
            .is_err());
    }

    #[test]
    fn test_reject_conflicting_votes_with_type() {
        let (validator_keypairs, mut pool, bank_forks) = create_initial_state();
        let mut slot = 2;
        for vote_type_1 in [
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
        ] {
            let conflicting_vote_types = conflicting_types(vote_type_1);
            for vote_type_2 in conflicting_vote_types {
                test_reject_conflicting_vote(
                    &mut pool,
                    &bank_forks.read().unwrap().root_bank(),
                    &validator_keypairs,
                    vote_type_1,
                    *vote_type_2,
                    slot,
                );
            }
            slot = slot.saturating_add(4);
        }
    }

    #[test]
    fn test_handle_new_root() {
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let bank_forks = create_bank_forks(&validator_keypairs);
        let mut pool = ConsensusPool::new_from_root_bank(
            Pubkey::new_unique(),
            &bank_forks.read().unwrap().root_bank(),
        );

        let root_bank = bank_forks.read().unwrap().root_bank();
        // Add a skip cert on slot 1 and finalize cert on slot 2
        let cert_1 = CertificateMessage {
            certificate: Certificate::new(CertificateType::Skip, 1, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                root_bank.epoch_schedule(),
                root_bank.epoch_stakes_map(),
                root_bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_1.clone()),
                &mut vec![]
            )
            .is_ok());
        let cert_2 = CertificateMessage {
            certificate: Certificate::new(
                CertificateType::FinalizeFast,
                2,
                Some(Hash::new_unique()),
            ),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                root_bank.epoch_schedule(),
                root_bank.epoch_stakes_map(),
                root_bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_2.clone()),
                &mut vec![]
            )
            .is_ok());
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
        let certificate = Certificate::new(CertificateType::Notarize, 2, Some(Hash::new_unique()));

        let cert = ConsensusMessage::Certificate(CertificateMessage {
            certificate,
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
        let cert_3 = CertificateMessage {
            certificate: Certificate::new(
                CertificateType::NotarizeFallback,
                3,
                Some(Hash::new_unique()),
            ),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        let bank = bank_forks.read().unwrap().root_bank();
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_3.clone()),
                &mut vec![]
            )
            .is_ok());
        let cert_4 = CertificateMessage {
            certificate: Certificate::new(CertificateType::Finalize, 4, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_4.clone()),
                &mut vec![]
            )
            .is_ok());
        // Should return both certificates
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 3
            && cert.certificate.certificate_type() == CertificateType::NotarizeFallback));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 4
            && cert.certificate.certificate_type() == CertificateType::Finalize));

        // Add Notarize cert on 5
        let cert_5 = CertificateMessage {
            certificate: Certificate::new(CertificateType::Notarize, 5, Some(Hash::new_unique())),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_5.clone()),
                &mut vec![]
            )
            .is_ok());

        // Add Finalize cert on 5
        let cert_5_finalize = CertificateMessage {
            certificate: Certificate::new(CertificateType::Finalize, 5, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_5_finalize.clone()),
                &mut vec![]
            )
            .is_ok());

        // Add FinalizeFast cert on 5
        let cert_5 = CertificateMessage {
            certificate: Certificate::new(
                CertificateType::FinalizeFast,
                5,
                Some(Hash::new_unique()),
            ),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_5.clone()),
                &mut vec![]
            )
            .is_ok());
        // Should return only FinalizeFast cert on 5
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 1);
        assert!(
            certs[0].certificate.slot() == 5
                && certs[0].certificate.certificate_type() == CertificateType::FinalizeFast
        );

        // Now add Notarize cert on 6
        let cert_6 = CertificateMessage {
            certificate: Certificate::new(CertificateType::Notarize, 6, Some(Hash::new_unique())),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_6.clone()),
                &mut vec![]
            )
            .is_ok());
        // Should return certs on 5 and 6
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 5
            && cert.certificate.certificate_type() == CertificateType::FinalizeFast));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 6
            && cert.certificate.certificate_type() == CertificateType::Notarize));

        // Add another Finalize cert on 6
        let cert_6_finalize = CertificateMessage {
            certificate: Certificate::new(CertificateType::Finalize, 6, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_6_finalize.clone()),
                &mut vec![]
            )
            .is_ok());
        // Add a NotarizeFallback cert on 6
        let cert_6_notarize_fallback = CertificateMessage {
            certificate: Certificate::new(
                CertificateType::NotarizeFallback,
                6,
                Some(Hash::new_unique()),
            ),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_6_notarize_fallback.clone()),
                &mut vec![]
            )
            .is_ok());
        // This should not be returned because 6 is the current highest finalized slot
        // only Notarize/Finalze/FinalizeFast should be returned
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 6
            && cert.certificate.certificate_type() == CertificateType::Finalize));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 6
            && cert.certificate.certificate_type() == CertificateType::Notarize));

        // Add another skip on 7
        let cert_7 = CertificateMessage {
            certificate: Certificate::new(CertificateType::Skip, 7, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_7.clone()),
                &mut vec![]
            )
            .is_ok());
        // Should return certs on 6 and 7
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 3);
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 6
            && cert.certificate.certificate_type() == CertificateType::Finalize));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 6
            && cert.certificate.certificate_type() == CertificateType::Notarize));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 7
            && cert.certificate.certificate_type() == CertificateType::Skip));

        // Add Finalize then Notarize cert on 8
        let cert_8_finalize = CertificateMessage {
            certificate: Certificate::new(CertificateType::Finalize, 8, None),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_8_finalize),
                &mut vec![]
            )
            .is_ok());
        let cert_8_notarize = CertificateMessage {
            certificate: Certificate::new(CertificateType::Notarize, 8, Some(Hash::new_unique())),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert_8_notarize),
                &mut vec![]
            )
            .is_ok());

        // Should only return certs on 8 now
        let certs = pool.get_certs_for_standstill();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 8
            && cert.certificate.certificate_type() == CertificateType::Finalize));
        assert!(certs.iter().any(|cert| cert.certificate.slot() == 8
            && cert.certificate.certificate_type() == CertificateType::Notarize));
    }

    #[test]
    fn test_new_parent_ready_with_certificates() {
        let (_, mut pool, bank_forks) = create_initial_state();
        let bank = bank_forks.read().unwrap().root_bank();
        let mut events = vec![];

        // Add a notarization cert on slot 1 to 3
        let hash = Hash::new_unique();
        for slot in 1..=3 {
            let cert = CertificateMessage {
                certificate: Certificate::new(CertificateType::Notarize, slot, Some(hash)),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    ConsensusMessage::Certificate(cert),
                    &mut events,
                )
                .is_ok());
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
            let cert = CertificateMessage {
                certificate: Certificate::new(CertificateType::FinalizeFast, slot, Some(hash)),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    ConsensusMessage::Certificate(cert),
                    &mut events,
                )
                .is_ok());
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
            let cert = CertificateMessage {
                certificate: Certificate::new(CertificateType::NotarizeFallback, slot, Some(hash)),
                signature: BLSSignature::default(),
                bitmap: Vec::new(),
            };
            assert!(pool
                .add_message(
                    bank.epoch_schedule(),
                    bank.epoch_stakes_map(),
                    bank.slot(),
                    &Pubkey::new_unique(),
                    ConsensusMessage::Certificate(cert),
                    &mut events,
                )
                .is_ok());
        }
        let cert = CertificateMessage {
            certificate: Certificate::new(CertificateType::FinalizeFast, 11, Some(hash)),
            signature: BLSSignature::default(),
            bitmap: Vec::new(),
        };
        assert!(pool
            .add_message(
                bank.epoch_schedule(),
                bank.epoch_stakes_map(),
                bank.slot(),
                &Pubkey::new_unique(),
                ConsensusMessage::Certificate(cert),
                &mut events,
            )
            .is_ok());
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

        assert!(
            vote_message
                .signature
                .verify(&bls_pubkey, &signed_message)
                .is_ok(),
            "BLS signature verification failed for VoteMessage"
        );
    }

    #[test]
    fn test_update_pubkey() {
        let new_pubkey = Pubkey::new_unique();
        let (_, mut pool, _) = create_initial_state();
        let old_pubkey = pool.my_pubkey();
        assert_eq!(pool.parent_ready_tracker.my_pubkey(), old_pubkey);
        assert_ne!(old_pubkey, new_pubkey);
        pool.update_pubkey(new_pubkey);
        assert_eq!(pool.my_pubkey(), new_pubkey);
        assert_eq!(pool.parent_ready_tracker.my_pubkey(), new_pubkey);
    }
}
