use {
    agave_bls_sigverify::sig_verified_messages::VoteAggregate,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::Block,
        fraction::Fraction,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    solana_bls_signatures::{BlsError, Signature as BLSSignature, SignatureProjective},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_signer_store::{EncodeError, encode_base2, encode_base3},
    std::{
        collections::{BTreeMap, HashMap, HashSet, hash_map::Entry as HashMapEntry},
        num::NonZero,
        sync::Arc,
    },
    thiserror::Error,
};

const MAX_NOTAR_FALLBACK_PER_VALIDATOR: usize = 3;

fn default_bitvec(max_validators: usize) -> BitVec<u8> {
    BitVec::repeat(false, max_validators)
}

fn get_validators(
    root_bank: &Bank,
    aggregate: &VoteAggregate,
) -> Result<impl Iterator<Item = Result<Pubkey, VotePoolAddVoteError>>, VotePoolAddVoteError> {
    let vote_slot = aggregate.vote().slot();
    let rank_map = match root_bank.epoch_stakes_from_slot(vote_slot) {
        None => {
            return Err(VotePoolAddVoteError::NoEpochStakes {
                root_slot: root_bank.slot(),
                vote_slot,
            });
        }
        Some(epoch_stakes) => epoch_stakes.bls_pubkey_to_rank_map(),
    };
    Ok(aggregate
        .ranks()
        .iter_ones()
        .map(|rank| match rank_map.get_pubkey_stake_entry(rank) {
            None => Err(VotePoolAddVoteError::NoRankFound),
            Some(e) => Ok(e.vote_account_pubkey),
        }))
}

#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum VotePoolAddVoteError {
    #[error("duplicate vote")]
    Duplicate,
    #[error("invalid votes")]
    Invalid,
    #[error("Signature aggregation failed with {0}")]
    SignatureAggregationFailed(BlsError),
    #[error("in root_slot:{root_slot}, didn't find epoch stakes for vote_slot:{vote_slot}")]
    NoEpochStakes { root_slot: Slot, vote_slot: Slot },
    #[error("could not find rank")]
    NoRankFound,
    #[error("encoding failed with {0:?}")]
    EncodingFailed(EncodeError),
}

struct NotarVoteEntry {
    slot: Slot,
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    ranks: BitVec<u8>,
}

impl NotarVoteEntry {
    fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_vote(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        if has_common_bits(&self.ranks, aggregate.ranks()) {
            // TODO: is it important to figure out if this is invalid or duplicate vote?
            return Err(VotePoolAddVoteError::Invalid);
        }
        let stake = match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                let stake = entry.stake;
                e.insert(entry);
                stake
            }
        };
        self.ranks |= aggregate.ranks();
        Ok(stake)
    }
}

struct GenesisVoteEntry {
    slot: Slot,
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    ranks: BitVec<u8>,
}

impl GenesisVoteEntry {
    fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_vote(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        if has_common_bits(&self.ranks, aggregate.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        let stake = match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                let stake = entry.stake;
                e.insert(entry);
                stake
            }
        };
        self.ranks |= aggregate.ranks();
        Ok(stake)
    }
}

struct NotarFallbackVoteEntry {
    slot: Slot,
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    validators: HashMap<Pubkey, HashSet<Hash>>,
    ranks: BitVec<u8>,
}

impl NotarFallbackVoteEntry {
    fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            max_validators,
            entries: HashMap::with_capacity(MAX_NOTAR_FALLBACK_PER_VALIDATOR),
            validators: HashMap::with_capacity(max_validators),
            ranks: default_bitvec(max_validators),
        }
    }

    fn add_vote(
        &mut self,
        root_bank: &Bank,
        aggregate: &VoteAggregate,
    ) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        let mut validators = vec![];
        for validator in get_validators(root_bank, aggregate)? {
            let validator = validator?;
            if let Some(set) = self.validators.get(&validator)
                && set.len() >= MAX_NOTAR_FALLBACK_PER_VALIDATOR
            {
                return Err(VotePoolAddVoteError::Invalid);
            }
            validators.push(validator);
        }
        let stake = match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(aggregate)?
            }
            HashMapEntry::Vacant(e) => {
                let entry = VoteEntry::new_with_aggregate(self.max_validators, aggregate);
                let stake = entry.stake;
                e.insert(entry);
                stake
            }
        };
        for validator in validators {
            self.validators
                .entry(validator)
                .or_default()
                .insert(*aggregate.vote().block_id().unwrap());
        }
        self.ranks |= aggregate.ranks();
        Ok(stake)
    }
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
            signature: *aggregate.signature(),
            stake: aggregate.stake().get(),
        }
    }

    fn add_vote(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
        if has_common_bits(&self.ranks, aggregate.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        self.signature
            .aggregate_with(std::iter::once(aggregate.signature()))
            .map_err(VotePoolAddVoteError::SignatureAggregationFailed)?;
        self.ranks |= aggregate.ranks();
        self.stake = self.stake.saturating_add(aggregate.stake().get());
        Ok(self.stake)
    }

    fn try_build_cert(
        &self,
        cert_type: CertificateType,
        total_stake: NonZero<u64>,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        if completed_certs.contains_key(&cert_type) {
            return Ok(None);
        }
        let observed_fraction = Fraction::new(self.stake, total_stake);
        if observed_fraction < cert_type.threshold() {
            return Ok(None);
        }
        let new_len = self.ranks.last_one().map_or(0, |i| i.saturating_add(1));
        let mut ranks = self.ranks.clone();
        ranks.resize(new_len, false);
        let bitmap = encode_base2(&ranks).map_err(VotePoolAddVoteError::EncodingFailed)?;
        let signature = BLSSignature::from(self.signature);
        Ok(Some(Certificate {
            cert_type,
            signature,
            bitmap,
        }))
    }
}

fn has_common_bits(a: &BitVec<u8>, b: &BitVec<u8>) -> bool {
    assert_eq!(a.len(), b.len());
    a.as_raw_slice()
        .iter()
        .zip(b.as_raw_slice())
        .any(|(&x, &y)| (x & y) != 0)
}

pub(super) struct VotePool {
    slot: Slot,
    skip: VoteEntry,
    skip_fallback: VoteEntry,
    finalize: VoteEntry,
    notar: NotarVoteEntry,
    notar_fallback: NotarFallbackVoteEntry,
    genesis: GenesisVoteEntry,
}

impl VotePool {
    pub(super) fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            skip: VoteEntry::new(max_validators),
            skip_fallback: VoteEntry::new(max_validators),
            finalize: VoteEntry::new(max_validators),
            notar: NotarVoteEntry::new(slot, max_validators),
            notar_fallback: NotarFallbackVoteEntry::new(slot, max_validators),
            genesis: GenesisVoteEntry::new(slot, max_validators),
        }
    }

    fn try_produce_notar_fallback_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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
            (Some(notar_entry), Some(nf_entry)) => {
                try_build_from_entries(cert_type, total_stake, notar_entry, nf_entry)
            }
        }
    }

    fn try_produce_finalize_cert(
        &mut self,
        total_stake: NonZero<u64>,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Finalize(aggregate.vote().slot());
        self.finalize
            .try_build_cert(cert_type, total_stake, completed_certs)
    }

    fn try_produce_finalize_fast_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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
            (true, true) => {
                try_build_from_entries(cert_type, total_stake, &self.skip, &self.skip_fallback)
            }
        }
    }

    fn try_produce_genesis_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
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

    /// Adds votes and if some certs can be produced and they are not already included in the completed certs, produces them.
    pub(super) fn add_vote(
        &mut self,
        root_bank: &Bank,
        total_stake: NonZero<u64>,
        aggregate: &VoteAggregate,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<(u64, Vec<Certificate>), VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        let stake = match aggregate.vote() {
            Vote::Notarize(_) => {
                if has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                if let Some(entry) = self
                    .notar_fallback
                    .entries
                    .get(aggregate.vote().block_id().unwrap())
                    && has_common_bits(&entry.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar.add_vote(aggregate)
            }
            Vote::NotarizeFallback(_) => {
                if has_common_bits(&self.finalize.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                if let Some(entry) = self.notar.entries.get(aggregate.vote().block_id().unwrap())
                    && has_common_bits(&entry.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar_fallback.add_vote(root_bank, aggregate)
            }
            Vote::Skip(_) => {
                if has_common_bits(&self.notar.ranks, aggregate.ranks())
                    || has_common_bits(&self.finalize.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip.add_vote(aggregate)
            }
            Vote::SkipFallback(_) => {
                if has_common_bits(&self.finalize.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip_fallback.add_vote(aggregate)
            }
            Vote::Finalize(_) => {
                if has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.notar_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.finalize.add_vote(aggregate)
            }
            Vote::Genesis(_) => {
                if has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.notar_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.notar.ranks, aggregate.ranks())
                    || has_common_bits(&self.finalize.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.genesis.add_vote(aggregate)
            }
        }?;
        let certs = self.try_produce_certs(total_stake, aggregate, completed_certs)?;
        Ok((stake, certs))
    }
}

/// Build a [`Certificate`] from two bitmaps.
fn try_build_from_entries(
    cert_type: CertificateType,
    total_stake: NonZero<u64>,
    entry0: &VoteEntry,
    entry1: &VoteEntry,
) -> Result<Option<Certificate>, VotePoolAddVoteError> {
    let observed_fraction = Fraction::new(entry0.stake.saturating_add(entry1.stake), total_stake);
    if observed_fraction < cert_type.threshold() {
        return Ok(None);
    }
    let mut bitmap0 = entry0.ranks.clone();
    let mut bitmap1 = entry1.ranks.clone();
    let last_one_0 = bitmap0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = bitmap1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_length = last_one_0.max(last_one_1);
    bitmap0.resize(new_length, false);
    bitmap1.resize(new_length, false);
    let bitmap = encode_base3(&bitmap0, &bitmap1).map_err(VotePoolAddVoteError::EncodingFailed)?;
    let mut signature = entry0.signature;
    signature
        .aggregate_with(std::iter::once(&entry1.signature))
        .map_err(VotePoolAddVoteError::SignatureAggregationFailed)?;
    Ok(Some(Certificate {
        cert_type,
        signature: signature.into(),
        bitmap,
    }))
}
