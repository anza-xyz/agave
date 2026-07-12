use crate::{stats::SigVerifyCertStats, utils::send_certs_to_pool};
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;

use {
    crate::{
        bls_sigverifier::{NUM_SLOTS_FOR_VERIFY, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        sig_verified_messages::VoteAggregate,
        stats::SigVerifyVoteStats,
    },
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, VoteMessage},
        fraction::Fraction,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    bitvec::vec::BitVec,
    rayon::{
        ThreadPool,
        iter::{Either, IntoParallelIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        AsSignatureProjective, BlsError, PreparedHashedMessage, Signature as BLSSignature,
        SignatureProjective, VerifySignature,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine},
    },
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    solana_signer_store::{EncodeError, encode_base2, encode_base3},
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{
        collections::{BTreeSet, HashMap, HashSet, hash_map::Entry as HashMapEntry},
        num::NonZero,
        sync::Arc,
    },
    thiserror::Error,
};

mod process;
mod verify;

const MAX_NOTAR_FALLBACK_PER_VALIDATOR: usize = 3;

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
struct VerifiedVotePayload {
    vote_aggregate: VoteAggregate,
    sender_vote_account_pubkeys: Vec<Pubkey>,
}

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVotePayload {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<BlsPubkeyAffine>,
    pub sender_vote_account_pubkey: Pubkey,
    pub sender_identity_pubkey: Pubkey,
    pub prepared_payload: Option<Arc<PreparedHashedMessage>>,
}

impl UnverifiedVotePayload {
    fn verify(self, root_bank: &Bank) -> Option<VerifiedVotePayload> {
        let is_verified = if let Some(prepared_payload) = self.prepared_payload.as_deref() {
            self.sender_bls_pubkey
                .verify_signature_prepared(&self.vote_message.signature, prepared_payload)
                .is_ok()
        } else {
            let payload =
                get_vote_payload_to_sign(self.vote_message.vote, self.vote_message.shred_version);
            self.sender_bls_pubkey
                .verify_signature(&self.vote_message.signature, &payload)
                .is_ok()
        };
        let vote_msg = VoteMessage {
            vote: self.vote_message.vote,
            signature: self.vote_message.signature,
            rank: self.vote_message.rank,
        };
        let vote_aggregate = VoteAggregate::new_from_verified_vote(root_bank, vote_msg);
        is_verified.then_some(VerifiedVotePayload {
            vote_aggregate,
            sender_vote_account_pubkeys: vec![self.sender_vote_account_pubkey],
        })
    }
}

#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum VotePoolAddVoteError {
    #[error("Signature aggregation failed with {0}")]
    SignatureAggregationFailed(BlsError),
    #[error("could not find rank")]
    NoRankFound,
    #[error("encoding failed with {0:?}")]
    EncodingFailed(EncodeError),
}

fn get_validators(
    rank_map: &BLSPubkeyToRankMap,
    aggregate: &VoteAggregate,
) -> impl Iterator<Item = Result<Pubkey, VotePoolAddVoteError>> {
    aggregate
        .ranks()
        .iter_ones()
        .map(|rank| match rank_map.get_pubkey_stake_entry(rank) {
            None => Err(VotePoolAddVoteError::NoRankFound),
            Some(e) => Ok(e.vote_account_pubkey),
        })
}

fn get_validator(
    rank_map: &BLSPubkeyToRankMap,
    msg: &UnverifiedVoteMessage,
) -> Result<Pubkey, VotePoolAddVoteError> {
    match rank_map.get_pubkey_stake_entry(msg.rank as usize) {
        None => Err(VotePoolAddVoteError::NoRankFound),
        Some(e) => Ok(e.vote_account_pubkey),
    }
}

fn default_bitvec(max_validators: usize) -> BitVec<u8> {
    BitVec::repeat(false, max_validators)
}

#[derive(Debug)]
struct VoteEntry {
    ranks: BitVec<u8>,
    signature: SignatureProjective,
    stake: u64,
}

impl VoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            ranks: default_bitvec(max_validators),
            signature: SignatureProjective::identity(),
            stake: 0,
        }
    }

    fn new_with_aggregate(max_validators: usize, aggregate: &VoteAggregate) -> Self {
        let mut ranks = default_bitvec(max_validators);
        ranks |= aggregate.ranks();
        Self {
            ranks,
            signature: aggregate.signature().try_as_projective().unwrap(),
            stake: aggregate.stake().get(),
        }
    }

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<(), VotePoolAddVoteError> {
        self.signature
            .aggregate_with(std::iter::once(aggregate.signature()))
            .map_err(VotePoolAddVoteError::SignatureAggregationFailed)?;
        self.ranks |= aggregate.ranks();
        self.stake = self.stake.saturating_add(aggregate.stake().get());
        Ok(())
    }

    fn try_build_cert(
        &self,
        cert_type: CertificateType,
        total_stake: NonZero<u64>,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        if completed_certs.contains(&cert_type) {
            return Ok(None);
        }
        let observed_fraction = Fraction::new(self.stake, total_stake);
        if observed_fraction < cert_type.threshold() {
            return Ok(None);
        }
        let mut ranks = self.ranks.clone();
        let signature = self.signature;
        let new_len = ranks.last_one().map_or(0, |i| i.saturating_add(1));
        ranks.resize(new_len, false);
        let bitmap = encode_base2(&ranks).map_err(VotePoolAddVoteError::EncodingFailed)?;
        let signature = BLSSignature::from(signature);
        Ok(Some(Certificate {
            cert_type,
            signature,
            bitmap,
        }))
    }
}

struct NotarVoteEntry {
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    ranks: BitVec<u8>,
}

impl NotarVoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<(), VotePoolAddVoteError> {
        let block_id = *aggregate.vote().block_id().unwrap();
        match self.entries.entry(block_id) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_aggregate(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                e.insert(entry);
            }
        };
        self.ranks |= aggregate.ranks();
        Ok(())
    }
}

struct GenesisVoteEntry {
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    ranks: BitVec<u8>,
}

impl GenesisVoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<(), VotePoolAddVoteError> {
        let block_id = *aggregate.vote().block_id().unwrap();
        match self.entries.entry(block_id) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_aggregate(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                e.insert(entry);
            }
        };
        self.ranks |= aggregate.ranks();
        Ok(())
    }
}

struct NotarFallbackVoteEntry {
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    validators: HashMap<Pubkey, HashSet<Hash>>,
    ranks: BitVec<u8>,
}

impl NotarFallbackVoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            validators: HashMap::with_capacity(max_validators),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_aggregate(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        aggregate: &VoteAggregate,
    ) -> Result<(), VotePoolAddVoteError> {
        let mut validators = vec![];
        for validator in get_validators(rank_map, aggregate) {
            let validator = validator?;
            validators.push(validator);
        }
        match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_aggregate(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                e.insert(entry);
            }
        };
        for validator in validators {
            self.validators
                .entry(validator)
                .or_default()
                .insert(*aggregate.vote().block_id().unwrap());
        }
        self.ranks |= aggregate.ranks();
        Ok(())
    }
}

struct SlotEntry {
    skip: VoteEntry,
    skip_fallback: VoteEntry,
    finalize: VoteEntry,
    notar: NotarVoteEntry,
    notar_fallback: NotarFallbackVoteEntry,
    genesis: GenesisVoteEntry,
}

impl SlotEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            skip: VoteEntry::new(max_validators),
            skip_fallback: VoteEntry::new(max_validators),
            finalize: VoteEntry::new(max_validators),
            notar: NotarVoteEntry::new(max_validators),
            notar_fallback: NotarFallbackVoteEntry::new(max_validators),
            genesis: GenesisVoteEntry::new(max_validators),
        }
    }

    #[must_use]
    fn wants_vote(&self, rank_map: &BLSPubkeyToRankMap, msg: &UnverifiedVoteMessage) -> bool {
        let rank = msg.rank as usize;
        match msg.vote {
            Vote::Notarize(_) => {
                !(self.skip.ranks[rank]
                    || self.genesis.ranks[rank]
                    || self.notar_fallback.ranks[rank]
                    || self.notar.ranks[rank])
            }
            Vote::NotarizeFallback(vote) => {
                if self.finalize.ranks[rank] || self.genesis.ranks[rank] || self.notar.ranks[rank] {
                    return false;
                }
                // TODO: remove unwrap
                let validator = get_validator(rank_map, msg).unwrap();
                if let Some(block_id_set) = self.notar_fallback.validators.get(&validator)
                    && (block_id_set.contains(&vote.block.block_id)
                        || block_id_set.len() >= MAX_NOTAR_FALLBACK_PER_VALIDATOR)
                {
                    return false;
                }
                true
            }
            Vote::Skip(_) => {
                !(self.notar.ranks[rank]
                    || self.finalize.ranks[rank]
                    || self.genesis.ranks[rank]
                    || self.skip_fallback.ranks[rank]
                    || self.skip.ranks[rank])
            }
            Vote::SkipFallback(_) => {
                !(self.finalize.ranks[rank]
                    || self.skip.ranks[rank]
                    || self.genesis.ranks[rank]
                    || self.skip_fallback.ranks[rank])
            }
            Vote::Finalize(_) => {
                !(self.skip.ranks[rank]
                    || self.skip_fallback.ranks[rank]
                    || self.notar_fallback.ranks[rank]
                    || self.genesis.ranks[rank]
                    || self.finalize.ranks[rank])
            }
            Vote::Genesis(_) => {
                !(self.skip.ranks[rank]
                    || self.notar_fallback.ranks[rank]
                    || self.notar.ranks[rank]
                    || self.finalize.ranks[rank]
                    || self.skip_fallback.ranks[rank]
                    || self.genesis.ranks[rank])
            }
        }
    }

    /// Adds votes and if some certs can be produced and they are not already included in the completed certs, produces them.
    pub(super) fn add_aggregate(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Vec<Certificate>, VotePoolAddVoteError> {
        match aggregate.vote() {
            Vote::Notarize(_) => self.notar.add_aggregate(aggregate),
            Vote::NotarizeFallback(_) => self.notar_fallback.add_aggregate(rank_map, aggregate),
            Vote::Skip(_) => self.skip.add_aggregate(aggregate),
            Vote::SkipFallback(_) => self.skip_fallback.add_aggregate(aggregate),
            Vote::Finalize(_) => self.finalize.add_aggregate(aggregate),
            Vote::Genesis(_) => self.genesis.add_aggregate(aggregate),
        }?;
        let certs = self.try_produce_certs(rank_map.total_stake(), aggregate, completed_certs)?;
        Ok(certs)
    }

    fn try_produce_notar_fallback_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::NotarizeFallback(block);
        match (
            self.notar.entries.get(&block.block_id),
            self.notar_fallback.entries.get(&block.block_id),
        ) {
            (None, None) => Ok(None),
            (Some(entry), None) | (None, Some(entry)) => {
                entry.try_build_cert(cert_type, total_stake, completed_certs)
            }
            (Some(notar_entry), Some(nf_entry)) => try_build_from_entries(
                cert_type,
                total_stake,
                notar_entry,
                nf_entry,
                completed_certs,
            ),
        }
    }

    fn try_produce_finalize_cert(
        &mut self,
        total_stake: NonZero<u64>,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Finalize(aggregate.vote().slot());
        self.finalize
            .try_build_cert(cert_type, total_stake, completed_certs)
    }

    fn try_produce_finalize_fast_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let Some(entry) = self.notar.entries.get(&block.block_id) else {
            return Ok(None);
        };
        let cert_type = CertificateType::FinalizeFast(block);
        entry.try_build_cert(cert_type, total_stake, completed_certs)
    }

    fn try_produce_notar_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Notarize(block);
        let Some(entry) = self.notar.entries.get(&block.block_id) else {
            return Ok(None);
        };
        entry.try_build_cert(cert_type, total_stake, completed_certs)
    }

    fn try_produce_skip_cert(
        &mut self,
        total_stake: NonZero<u64>,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Skip(aggregate.vote().slot());
        match (self.skip.stake > 0, self.skip_fallback.stake > 0) {
            (false, false) => Ok(None),
            (true, false) => self
                .skip
                .try_build_cert(cert_type, total_stake, completed_certs),
            (false, true) => {
                self.skip_fallback
                    .try_build_cert(cert_type, total_stake, completed_certs)
            }
            (true, true) => try_build_from_entries(
                cert_type,
                total_stake,
                &self.skip,
                &self.skip_fallback,
                completed_certs,
            ),
        }
    }

    fn try_produce_genesis_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Genesis(block);
        let Some(entry) = self.genesis.entries.get(&block.block_id) else {
            return Ok(None);
        };
        entry.try_build_cert(cert_type, total_stake, completed_certs)
    }

    fn try_produce_certs(
        &mut self,
        total_stake: NonZero<u64>,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeSet<CertificateType>,
    ) -> Result<Vec<Certificate>, VotePoolAddVoteError> {
        match aggregate.vote() {
            Vote::Notarize(notar) => Ok([
                self.try_produce_notar_fallback_cert(total_stake, notar.block, completed_certs)?,
                self.try_produce_notar_cert(total_stake, notar.block, completed_certs)?,
                self.try_produce_finalize_fast_cert(total_stake, notar.block, completed_certs)?,
            ]
            .into_iter()
            .flatten()
            .collect()),
            Vote::NotarizeFallback(nf) => Ok(self
                .try_produce_notar_fallback_cert(total_stake, nf.block, completed_certs)?
                .into_iter()
                .collect()),
            Vote::Finalize(_) => Ok(self
                .try_produce_finalize_cert(total_stake, aggregate, completed_certs)?
                .into_iter()
                .collect()),
            Vote::Skip(_) | Vote::SkipFallback(_) => Ok(self
                .try_produce_skip_cert(total_stake, aggregate, completed_certs)?
                .into_iter()
                .collect()),
            Vote::Genesis(genesis) => Ok(self
                .try_produce_genesis_cert(total_stake, genesis.block, completed_certs)?
                .into_iter()
                .collect()),
        }
    }
}

pub(super) struct VotePool {
    cluster_info: Arc<ClusterInfo>,
    banlist: Arc<SimpleQosBanlist>,
    leader_schedule: Arc<LeaderScheduleCache>,
    // TODO: need to purge old entries
    entries: HashMap<Slot, SlotEntry>,
    stats: SigVerifyVoteStats,
    // TODO: need to purge old entries
    completed_certs: BTreeSet<CertificateType>,
}

impl VotePool {
    pub(super) fn new(
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        banlist: Arc<SimpleQosBanlist>,
    ) -> Self {
        Self {
            cluster_info,
            leader_schedule,
            banlist,
            entries: HashMap::new(),
            stats: SigVerifyVoteStats::default(),
            completed_certs: BTreeSet::new(),
        }
    }

    // TODO: if a vote is needed just for rewards, do not send it to the vote pool.
    pub(super) fn wants_vote<'a>(
        &mut self,
        rank_map_cache: &mut HashMap<Epoch, &'a BLSPubkeyToRankMap>,
        msg: UnverifiedVoteMessage,
        sender_identity_pubkey: Pubkey,
        root_bank: &'a Bank,
    ) -> Option<UnverifiedVotePayload> {
        let vote_slot = msg.vote.slot();
        let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
        let root_slot = root_bank.slot();
        if vote_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
            return None;
        }

        let rank_map = match rank_map_cache.entry(vote_epoch) {
            HashMapEntry::Occupied(entry) => *entry.into_mut(),
            HashMapEntry::Vacant(entry) => {
                let Some(rank_map) = root_bank.get_rank_map(vote_slot) else {
                    // TODO: fix
                    // self.stats.discard_vote_no_epoch_stakes += 1;
                    return None;
                };
                *entry.insert(rank_map)
            }
        };

        let stake_entry = rank_map
            .get_pubkey_stake_entry(msg.rank.into())
            .or_else(|| {
                // TODO: fix
                // self.stats.discard_vote_invalid_rank += 1;
                None
            })?;

        let msg = UnverifiedVotePayload {
            vote_message: msg,
            sender_bls_pubkey: stake_entry.bls_pubkey,
            sender_vote_account_pubkey: stake_entry.vote_account_pubkey,
            sender_identity_pubkey,
            prepared_payload: None,
        };

        if vote_slot > root_slot
            // Genesis votes should be allowed on the TowerBFT root
         || (msg.vote_message.vote.is_genesis_vote() && vote_slot >= root_slot)
        {
            match self.entries.get(&vote_slot) {
                None => return Some(msg),
                Some(entry) => {
                    if entry.wants_vote(rank_map, &msg.vote_message) {
                        return Some(msg);
                    }
                }
            }
        }
        if rewards_wants_vote(
            &self.cluster_info,
            &self.leader_schedule,
            root_bank.slot(),
            &msg.vote_message.vote,
        ) {
            return Some(msg);
        }
        // TODO: fix
        // self.stats.num_old_votes_received += 1;
        None
    }

    pub(super) fn verify_and_send_votes(
        &mut self,
        unverified_votes: HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>>,
        root_bank: &Bank,
        rank_map_cache: HashMap<Epoch, &BLSPubkeyToRankMap>,
        thread_pool: &ThreadPool,
        channels: &SigVerifierChannels,
    ) -> Result<(), SigVerifyVoteError> {
        // TODO: this should maintain its own stats and call maybe_report on success.
        // TODO: when this is called, there aren't any conflicting votes so a straight up aggregate.

        let mut certs = vec![];
        for (vote_payload_to_sign, unverified_votes) in unverified_votes {
            let verified_votes = self.verify_votes(
                root_bank,
                vote_payload_to_sign,
                unverified_votes,
                thread_pool,
            );
            for verified_vote in &verified_votes {
                let vote_slot = verified_vote.vote_aggregate.vote.slot();
                let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
                let rank_map = rank_map_cache.get(&vote_epoch).unwrap();
                // TODO: fix unwrap
                let mut built_certs = self
                    .add_aggregate(rank_map, &verified_vote.vote_aggregate)
                    .unwrap();
                certs.append(&mut built_certs);
            }
            self.process_verified_votes(root_bank, verified_votes, channels)?;
        }

        // TODO: fix unwrap
        send_certs_to_pool(
            certs,
            &channels.channel_to_pool,
            // TODO: fix stats
            &mut SigVerifyCertStats::default(),
        )
        .unwrap();
        Ok(())
    }

    // TODO: this should be called by the sigverifier when it is exiting.
    pub(super) fn do_report_stats(&self) {}

    fn add_aggregate(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        aggregate: &VoteAggregate,
    ) -> Result<Vec<Certificate>, VotePoolAddVoteError> {
        let max_validators = rank_map.len();
        let slot_entry = self
            .entries
            .entry(aggregate.vote.slot())
            .or_insert_with(|| SlotEntry::new(max_validators));
        slot_entry.add_aggregate(rank_map, aggregate, &self.completed_certs)
    }

    fn verify_individual_votes(
        &self,
        root_bank: &Bank,
        unverified_votes: Vec<UnverifiedVotePayload>,
        thread_pool: &ThreadPool,
    ) -> (Vec<VerifiedVotePayload>, Vec<Pubkey>) {
        thread_pool.install(|| {
            unverified_votes
                .into_par_iter()
                .partition_map(|unverified_vote| {
                    let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                    match unverified_vote.verify(root_bank) {
                        Some(vote) => Either::Left(vote),
                        None => Either::Right(sender_identity_pubkey),
                    }
                })
        })
    }
}

/// Build a [`Certificate`] from two bitmaps.
fn try_build_from_entries(
    cert_type: CertificateType,
    total_stake: NonZero<u64>,
    entry0: &VoteEntry,
    entry1: &VoteEntry,
    completed_certs: &BTreeSet<CertificateType>,
) -> Result<Option<Certificate>, VotePoolAddVoteError> {
    if completed_certs.contains(&cert_type) {
        return Ok(None);
    }
    let observed_fraction = Fraction::new(entry0.stake.saturating_add(entry1.stake), total_stake);
    if observed_fraction < cert_type.threshold() {
        return Ok(None);
    }
    let mut signature0 = entry0.signature;
    let mut ranks0 = entry0.ranks.clone();
    let signature1 = entry1.signature;
    let mut ranks1 = entry1.ranks.clone();
    let last_one_0 = ranks0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = ranks1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_length = last_one_0.max(last_one_1);
    ranks0.resize(new_length, false);
    ranks1.resize(new_length, false);
    let bitmap = encode_base3(&ranks0, &ranks1).map_err(VotePoolAddVoteError::EncodingFailed)?;
    signature0
        .aggregate_with(std::iter::once(&signature1))
        .map_err(VotePoolAddVoteError::SignatureAggregationFailed)?;
    Ok(Some(Certificate {
        cert_type,
        signature: signature0.into(),
        bitmap,
    }))
}
