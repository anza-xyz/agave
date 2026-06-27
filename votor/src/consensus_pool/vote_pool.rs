use {
    crate::consensus_pool::vote_pool::conflicted_aggregates::ConflictedAggregates,
    agave_bls_sigverify::sig_verified_messages::VoteAggregate,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::Block,
        fraction::Fraction,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    solana_bls_signatures::{
        AsSignatureProjective, BlsError, Signature as BLSSignature, SignatureProjective,
    },
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

mod conflicted_aggregates;

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

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        if has_common_bits(&self.ranks, aggregate.ranks()) {
            return Err(VotePoolAddVoteError::Invalid);
        }
        let stake = match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_aggregate(aggregate)?
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

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, aggregate.vote().slot());
        if has_common_bits(&self.ranks, aggregate.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        let stake = match self.entries.entry(*aggregate.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_aggregate(aggregate)?
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

    fn add_aggregate(
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
                entry.add_aggregate(aggregate)?
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
    conflicted: ConflictedAggregates,
}

impl VoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            ranks: default_bitvec(max_validators),
            signature: SignatureProjective::identity(),
            stake: 0,
            conflicted: ConflictedAggregates::default(),
        }
    }

    fn new_with_aggregate(max_validators: usize, aggregate: &VoteAggregate) -> Self {
        let mut ranks = default_bitvec(max_validators);
        ranks |= aggregate.ranks();
        Self {
            ranks,
            signature: aggregate.signature().try_as_projective().unwrap(),
            stake: aggregate.stake().get(),
            conflicted: ConflictedAggregates::default(),
        }
    }

    fn add_aggregate(&mut self, aggregate: &VoteAggregate) -> Result<u64, VotePoolAddVoteError> {
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
        vote: &Vote,
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
        let mut ranks = self.ranks.clone();
        let mut signature = self.signature;
        self.conflicted
            .add_to_cert(vote, &mut signature, &mut ranks);
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
            (Some(entry), None) | (None, Some(entry)) => entry.try_build_cert(
                cert_type,
                &Vote::new_notarization_fallback_vote(block),
                total_stake,
                completed_certs,
            ),
            (Some(notar_entry), Some(nf_entry)) => try_build_from_entries(
                cert_type,
                total_stake,
                notar_entry,
                nf_entry,
                &Vote::new_notarization_vote(block),
                &Vote::new_notarization_fallback_vote(block),
                completed_certs,
            ),
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
            .try_build_cert(cert_type, aggregate.vote(), total_stake, completed_certs)
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
        entry.try_build_cert(
            cert_type,
            &Vote::new_notarization_vote(block),
            total_stake,
            completed_certs,
        )
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
        entry.try_build_cert(
            cert_type,
            &Vote::new_notarization_vote(block),
            total_stake,
            completed_certs,
        )
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
            (true, false) => self.skip.try_build_cert(
                cert_type,
                &Vote::new_skip_vote(aggregate.vote().slot()),
                total_stake,
                completed_certs,
            ),
            (false, true) => self.skip_fallback.try_build_cert(
                cert_type,
                &Vote::new_skip_fallback_vote(aggregate.vote().slot()),
                total_stake,
                completed_certs,
            ),
            (true, true) => {
                let slot = aggregate.vote().slot();
                try_build_from_entries(
                    cert_type,
                    total_stake,
                    &self.skip,
                    &self.skip_fallback,
                    &Vote::new_skip_vote(slot),
                    &Vote::new_skip_fallback_vote(slot),
                    completed_certs,
                )
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
        entry.try_build_cert(
            cert_type,
            &Vote::new_genesis_vote(block),
            total_stake,
            completed_certs,
        )
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
    pub(super) fn add_aggregate(
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
                self.notar.add_aggregate(aggregate)
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
                self.notar_fallback.add_aggregate(root_bank, aggregate)
            }
            Vote::Skip(_) => {
                if has_common_bits(&self.notar.ranks, aggregate.ranks())
                    || has_common_bits(&self.finalize.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip.add_aggregate(aggregate)
            }
            Vote::SkipFallback(_) => {
                if has_common_bits(&self.finalize.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip_fallback.add_aggregate(aggregate)
            }
            Vote::Finalize(_) => {
                if has_common_bits(&self.skip.ranks, aggregate.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.notar_fallback.ranks, aggregate.ranks())
                    || has_common_bits(&self.genesis.ranks, aggregate.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.finalize.add_aggregate(aggregate)
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
                self.genesis.add_aggregate(aggregate)
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
    vote0: &Vote,
    vote1: &Vote,
    completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
) -> Result<Option<Certificate>, VotePoolAddVoteError> {
    if completed_certs.contains_key(&cert_type) {
        return Ok(None);
    }
    let observed_fraction = Fraction::new(entry0.stake.saturating_add(entry1.stake), total_stake);
    if observed_fraction < cert_type.threshold() {
        return Ok(None);
    }
    let mut signature0 = entry0.signature;
    let mut ranks0 = entry0.ranks.clone();
    entry0
        .conflicted
        .add_to_cert(vote0, &mut signature0, &mut ranks0);
    let mut signature1 = entry1.signature;
    let mut ranks1 = entry1.ranks.clone();
    entry0
        .conflicted
        .add_to_cert(vote1, &mut signature1, &mut ranks1);
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::consensus_pool::{
            get_total_stake,
            tests::{conflicting_types, create_bank_forks},
        },
        agave_votor_messages::{
            consensus_message::{BLS_KEYPAIR_DERIVE_SEED, VoteMessage},
            vote::VoteType,
            wire::get_vote_payload_to_sign,
        },
        solana_bls_signatures::Keypair as BLSKeypair,
        solana_runtime::{bank_forks::BankForks, genesis_utils::ValidatorVoteKeypairs},
        std::sync::RwLock,
    };

    fn create_new_vote(vote_type: VoteType, slot: Slot, block_id: Hash) -> Vote {
        match vote_type {
            VoteType::Notarize => Vote::new_notarization_vote(Block { slot, block_id }),
            VoteType::NotarizeFallback => {
                Vote::new_notarization_fallback_vote(Block { slot, block_id })
            }
            VoteType::Skip => Vote::new_skip_vote(slot),
            VoteType::SkipFallback => Vote::new_skip_fallback_vote(slot),
            VoteType::Finalize => Vote::new_finalization_vote(slot),
            VoteType::Genesis => Vote::new_genesis_vote(Block { slot, block_id }),
        }
    }

    struct TestContext {
        validators: Vec<ValidatorVoteKeypairs>,
        _bank_forks: Arc<RwLock<BankForks>>,
        bank: Arc<Bank>,
        pool: VotePool,
        total_stake: NonZero<u64>,
        slot: Slot,
        block_id: Hash,
        shred_version: u16,
    }

    impl TestContext {
        fn new() -> Self {
            let max_validators = 10;
            let validators = (0..max_validators)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let bank_forks = create_bank_forks(&validators);
            let bank = bank_forks.read().unwrap().root_bank();
            let slot = 12;
            let total_stake = get_total_stake(&bank, slot).unwrap();
            let block_id = Hash::new_unique();
            let pool = VotePool::new(slot, max_validators);
            let shred_version = 1233;
            Self {
                validators,
                _bank_forks: bank_forks,
                bank,
                pool,
                slot,
                block_id,
                shred_version,
                total_stake,
            }
        }

        fn new_vote(&self, vote: Vote, rank: u16) -> VoteAggregate {
            let bls_keypair = BLSKeypair::derive_from_signer(
                &self.validators[rank as usize].vote_keypair,
                BLS_KEYPAIR_DERIVE_SEED,
            )
            .unwrap();
            let payload = get_vote_payload_to_sign(vote, self.shred_version);
            let signature = bls_keypair.sign(&payload).into();
            let msg = VoteMessage {
                vote,
                signature,
                rank,
            };
            VoteAggregate::new_from_verified_vote(&self.bank, msg)
        }
    }

    #[test]
    fn validate_conflicting_votes() {
        for src in [
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
            VoteType::Genesis,
        ] {
            for conflicting in conflicting_types(src) {
                let mut ctx = TestContext::new();
                let conflicting = create_new_vote(*conflicting, ctx.slot, ctx.block_id);
                let conflicting = ctx.new_vote(conflicting, 1);
                let src = create_new_vote(src, ctx.slot, ctx.block_id);
                let src = ctx.new_vote(src, 1);
                ctx.pool
                    .add_aggregate(&ctx.bank, ctx.total_stake, &conflicting, &BTreeMap::new())
                    .unwrap();
                let err = ctx
                    .pool
                    .add_aggregate(&ctx.bank, ctx.total_stake, &src, &BTreeMap::new())
                    .unwrap_err();
                assert!(matches!(err, VotePoolAddVoteError::Invalid));
            }

            let mut ctx = TestContext::new();
            let src = create_new_vote(src, ctx.slot, ctx.block_id);
            let src = ctx.new_vote(src, 1);
            ctx.pool
                .add_aggregate(&ctx.bank, ctx.total_stake, &src, &BTreeMap::new())
                .unwrap();
            let err = ctx
                .pool
                .add_aggregate(&ctx.bank, ctx.total_stake, &src, &BTreeMap::new())
                .unwrap_err();
            assert!(
                matches!(err, VotePoolAddVoteError::Duplicate)
                    || matches!(err, VotePoolAddVoteError::Invalid)
            );
        }
    }

    #[test]
    fn validate_max_notar_votes_per_validator() {
        let mut ctx = TestContext::new();
        for _ in 0..MAX_NOTAR_FALLBACK_PER_VALIDATOR {
            let notar = create_new_vote(VoteType::NotarizeFallback, ctx.slot, Hash::new_unique());
            let notar = ctx.new_vote(notar, 1);
            ctx.pool
                .add_aggregate(&ctx.bank, ctx.total_stake, &notar, &BTreeMap::new())
                .unwrap();
        }
        let notar = create_new_vote(VoteType::NotarizeFallback, ctx.slot, Hash::new_unique());
        let notar = ctx.new_vote(notar, 1);
        let err = ctx
            .pool
            .add_aggregate(&ctx.bank, ctx.total_stake, &notar, &BTreeMap::new())
            .unwrap_err();
        assert!(matches!(err, VotePoolAddVoteError::Invalid));
    }
}
