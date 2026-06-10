use {
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, SigVerifiedVoteBatch},
        fraction::Fraction,
        migration::GENESIS_VOTE_THRESHOLD,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    solana_bls_signatures::{Signature as BLSSignature, SignatureProjective},
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

const SKIP_CERT_THRESHOLD: Fraction = Fraction::from_percentage(60);
const NOTAR_CERT_THRESHOLD: Fraction = Fraction::from_percentage(60);
const NOTAR_FALLBACK_CERT_THRESHOLD: Fraction = Fraction::from_percentage(60);
const FINALIZE_CERT_THRESHOLD: Fraction = Fraction::from_percentage(60);
const FAST_FINALIZE_CERT_THRESHOLD: Fraction = Fraction::from_percentage(80);

// TODO: return an iterator maybe?
fn get_validators(root_bank: &Bank, batch: &SigVerifiedVoteBatch) -> Vec<Pubkey> {
    let epoch_stakes = root_bank
        .epoch_stakes_from_slot(batch.vote().slot())
        .unwrap();
    let bls_pubkey_to_rank_map = epoch_stakes.bls_pubkey_to_rank_map();
    batch
        .ranks()
        .iter_ones()
        .map(|ind| {
            bls_pubkey_to_rank_map
                .get_pubkey_stake_entry(ind)
                .unwrap()
                .vote_account_pubkey
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum VotePoolAddVoteError {
    #[error("duplicate vote")]
    Duplicate,
    #[error("invalid votes")]
    Invalid,
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
            entries: HashMap::new(),
            ranks: BitVec::with_capacity(max_validators),
        }
    }

    fn add_vote(&mut self, batch: &SigVerifiedVoteBatch) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote().slot());
        if has_common_bits(&self.ranks, batch.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        let stake = match self.entries.entry(*batch.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(batch)?
            }
            HashMapEntry::Vacant(e) => {
                // TODO: initial VoteEntry so that we do not have to modify it.
                let mut entry = VoteEntry::new(self.max_validators);
                let stake = entry.add_vote(batch)?;
                e.insert(entry);
                stake
            }
        };
        self.ranks |= batch.ranks();
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
            entries: HashMap::new(),
            ranks: BitVec::with_capacity(max_validators),
        }
    }

    fn add_vote(&mut self, batch: &SigVerifiedVoteBatch) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote().slot());
        if has_common_bits(&self.ranks, batch.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        let stake = match self.entries.entry(*batch.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(batch)?
            }
            HashMapEntry::Vacant(e) => {
                // TODO: initial VoteEntry so that we do not have to modify it.
                let mut entry = VoteEntry::new(self.max_validators);
                let stake = entry.add_vote(batch)?;
                e.insert(entry);
                stake
            }
        };
        self.ranks |= batch.ranks();
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
            entries: HashMap::new(),
            validators: HashMap::with_capacity(max_validators),
            ranks: BitVec::with_capacity(max_validators),
        }
    }

    fn add_vote(
        &mut self,
        root_bank: &Bank,
        batch: &SigVerifiedVoteBatch,
    ) -> Result<u64, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote().slot());
        for validator in get_validators(root_bank, batch) {
            if let Some(set) = self.validators.get(&validator)
                && set.len() >= MAX_NOTAR_FALLBACK_PER_VALIDATOR
            {
                return Err(VotePoolAddVoteError::Invalid);
            }
        }
        let stake = match self.entries.entry(*batch.vote().block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let entry = e.get_mut();
                entry.add_vote(batch)?
            }
            HashMapEntry::Vacant(e) => {
                // TODO: initial VoteEntry so that we do not have to modify it.
                let mut entry = VoteEntry::new(self.max_validators);
                let stake = entry.add_vote(batch)?;
                e.insert(entry);
                stake
            }
        };
        for validator in get_validators(root_bank, batch) {
            self.validators
                .entry(validator)
                .or_default()
                .insert(*batch.vote().block_id().unwrap());
        }
        self.ranks |= batch.ranks();
        Ok(stake)
    }
}

struct VoteEntry {
    ranks: BitVec<u8>,
    signature: SignatureProjective,
    stake: u64,
}

impl VoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            ranks: BitVec::with_capacity(max_validators),
            signature: SignatureProjective::identity(),
            stake: 0,
        }
    }

    fn add_vote(&mut self, batch: &SigVerifiedVoteBatch) -> Result<u64, VotePoolAddVoteError> {
        if has_common_bits(&self.ranks, batch.ranks()) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        self.ranks |= batch.ranks();
        // TODO: handle signature error
        self.signature
            .aggregate_with(std::iter::once(batch.signature()))
            .unwrap();
        self.stake = self.stake.saturating_add(batch.stake().get());
        Ok(self.stake)
    }

    fn try_build_cert(
        &self,
        cert_type: CertificateType,
        threshold: Fraction,
        total_stake: NonZero<u64>,
    ) -> Option<Certificate> {
        let observed_fraction = Fraction::new(self.stake, total_stake);
        if observed_fraction < threshold {
            return None;
        }
        let new_len = self.ranks.last_one().map_or(0, |i| i.saturating_add(1));
        // TODO: can we avoid the clone somehow?
        let mut ranks = self.ranks.clone();
        ranks.resize(new_len, false);
        let bitmap = encode_base2(&ranks).unwrap();
        let signature = BLSSignature::from(self.signature);
        Some(Certificate {
            cert_type,
            signature,
            bitmap,
        })
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
        if completed_certs.contains_key(&cert_type) {
            return Ok(None);
        }
        match (
            self.notar.entries.get(&block.block_id),
            self.notar_fallback.entries.get(&block.block_id),
        ) {
            (None, None) => Ok(None),
            (Some(entry), None) | (None, Some(entry)) => {
                if let Some(cert) =
                    entry.try_build_cert(cert_type, NOTAR_FALLBACK_CERT_THRESHOLD, total_stake)
                {
                    return Ok(Some(cert));
                }
                Ok(None)
            }
            (Some(notar_entry), Some(nf_entry)) => {
                if let Some(cert) = try_build_from_entries(
                    cert_type,
                    NOTAR_FALLBACK_CERT_THRESHOLD,
                    total_stake,
                    notar_entry,
                    nf_entry,
                ) {
                    return Ok(Some(cert));
                }
                Ok(None)
            }
        }
    }

    fn try_produce_finalize_cert(
        &mut self,
        total_stake: NonZero<u64>,
        batch: &SigVerifiedVoteBatch,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Finalize(batch.vote().slot());
        if completed_certs.contains_key(&cert_type) {
            return Ok(None);
        }
        let observed_stake = self.finalize.stake;
        let observed_fraction = Fraction::new(observed_stake, total_stake);
        if observed_fraction < FINALIZE_CERT_THRESHOLD {
            return Ok(None);
        }
        // TODO: can we avoid the clone?
        let cert = build_cert_from_bitmap(
            cert_type,
            self.finalize.signature,
            self.finalize.ranks.clone(),
        )
        .unwrap();
        Ok(Some(cert))
    }

    fn try_produce_skip_cert(
        &mut self,
        total_stake: NonZero<u64>,
        batch: &SigVerifiedVoteBatch,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Skip(batch.vote().slot());
        if completed_certs.contains_key(&cert_type) {
            return Ok(None);
        }
        if let Some(cert) = self
            .skip
            .try_build_cert(cert_type, SKIP_CERT_THRESHOLD, total_stake)
        {
            return Ok(Some(cert));
        }
        if let Some(cert) =
            self.skip_fallback
                .try_build_cert(cert_type, SKIP_CERT_THRESHOLD, total_stake)
        {
            return Ok(Some(cert));
        }
        if let Some(cert) = try_build_from_entries(
            cert_type,
            SKIP_CERT_THRESHOLD,
            total_stake,
            &self.skip,
            &self.skip_fallback,
        ) {
            return Ok(Some(cert));
        }
        Ok(None)
    }

    fn try_produce_genesis_cert(
        &mut self,
        total_stake: NonZero<u64>,
        block: Block,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Option<Certificate>, VotePoolAddVoteError> {
        let cert_type = CertificateType::Genesis(block);
        if completed_certs.contains_key(&cert_type) {
            return Ok(None);
        }
        let Some(entry) = self.genesis.entries.get(&block.block_id) else {
            return Ok(None);
        };
        if let Some(cert) = entry.try_build_cert(cert_type, GENESIS_VOTE_THRESHOLD, total_stake) {
            return Ok(Some(cert));
        }
        Ok(None)
    }

    fn try_produce_certs(
        &mut self,
        total_stake: NonZero<u64>,
        batch: &SigVerifiedVoteBatch,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<Vec<Certificate>, VotePoolAddVoteError> {
        match batch.vote() {
            Vote::Notarize(_) => {
                // notar; nf; ff;
                unimplemented!();
            }
            Vote::NotarizeFallback(nf) => Ok(self
                .try_produce_notar_fallback_cert(total_stake, nf.block, completed_certs)?
                .into_iter()
                .collect()),
            Vote::Finalize(_) => Ok(self
                .try_produce_finalize_cert(total_stake, batch, completed_certs)?
                .into_iter()
                .collect()),
            Vote::Skip(_) | Vote::SkipFallback(_) => Ok(self
                .try_produce_skip_cert(total_stake, batch, completed_certs)?
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
        batch: &SigVerifiedVoteBatch,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<(u64, Vec<Certificate>), VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote().slot());
        let stake = match batch.vote() {
            Vote::Notarize(_) => {
                if has_common_bits(&self.skip.ranks, batch.ranks())
                    || has_common_bits(&self.genesis.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                if let Some(entry) = self
                    .notar_fallback
                    .entries
                    .get(batch.vote().block_id().unwrap())
                    && has_common_bits(&entry.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar.add_vote(batch)
            }
            Vote::NotarizeFallback(_) => {
                if has_common_bits(&self.finalize.ranks, batch.ranks())
                    || has_common_bits(&self.genesis.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                if let Some(entry) = self.notar.entries.get(batch.vote().block_id().unwrap())
                    && has_common_bits(&entry.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar_fallback.add_vote(root_bank, batch)
            }
            Vote::Skip(_) => {
                if has_common_bits(&self.notar.ranks, batch.ranks())
                    || has_common_bits(&self.finalize.ranks, batch.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, batch.ranks())
                    || has_common_bits(&self.genesis.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip.add_vote(batch)
            }
            Vote::SkipFallback(_) => {
                if has_common_bits(&self.finalize.ranks, batch.ranks())
                    || has_common_bits(&self.skip.ranks, batch.ranks())
                    || has_common_bits(&self.genesis.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip_fallback.add_vote(batch)
            }
            Vote::Finalize(_) => {
                if has_common_bits(&self.skip.ranks, batch.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, batch.ranks())
                    || has_common_bits(&self.notar_fallback.ranks, batch.ranks())
                    || has_common_bits(&self.genesis.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.finalize.add_vote(batch)
            }
            Vote::Genesis(_) => {
                if has_common_bits(&self.skip.ranks, batch.ranks())
                    || has_common_bits(&self.skip_fallback.ranks, batch.ranks())
                    || has_common_bits(&self.notar_fallback.ranks, batch.ranks())
                    || has_common_bits(&self.notar.ranks, batch.ranks())
                    || has_common_bits(&self.finalize.ranks, batch.ranks())
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.genesis.add_vote(batch)
            }
        }?;
        let certs = self.try_produce_certs(total_stake, batch, completed_certs)?;
        Ok((stake, certs))
    }
}

/// Build a [`Certificate`] from a single bitmap.
fn build_cert_from_bitmap(
    cert_type: CertificateType,
    signature: SignatureProjective,
    mut bitmap: BitVec<u8>,
) -> Result<Certificate, EncodeError> {
    let new_len = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
    bitmap.resize(new_len, false);
    let bitmap = encode_base2(&bitmap)?;
    Ok(Certificate {
        cert_type,
        signature: signature.into(),
        bitmap,
    })
}

/// Build a [`Certificate`] from two bitmaps.
fn try_build_from_entries(
    cert_type: CertificateType,
    threshold: Fraction,
    total_stake: NonZero<u64>,
    entry0: &VoteEntry,
    entry1: &VoteEntry,
) -> Option<Certificate> {
    let observed_fraction = Fraction::new(entry0.stake.saturating_add(entry1.stake), total_stake);
    if observed_fraction < threshold {
        return None;
    }
    // TODO: can we avoid the clones somehow?
    let mut bitmap0 = entry0.ranks.clone();
    let mut bitmap1 = entry1.ranks.clone();
    let last_one_0 = bitmap0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = bitmap1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_length = last_one_0.max(last_one_1);
    bitmap0.resize(new_length, false);
    bitmap1.resize(new_length, false);
    let bitmap = encode_base3(&bitmap0, &bitmap1).unwrap();
    let mut signature = entry0.signature;
    signature
        .aggregate_with(std::iter::once(&entry1.signature))
        .unwrap();
    Some(Certificate {
        cert_type,
        signature: signature.into(),
        bitmap,
    })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
        solana_bls_signatures::Signature as BLSSignature,
    };

    #[test]
    fn test_notar_failures() {
        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let slot = 1;

        let mut votes = VotePool::new(slot);
        let skip = VoteMessage {
            vote: Vote::new_skip_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, skip).unwrap();
        let notar = VoteMessage {
            vote: Vote::new_notarization_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, notar),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let notar = VoteMessage {
            vote: Vote::new_notarization_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        votes.add_vote(voter, notar).unwrap();
        let notar = VoteMessage {
            vote: Vote::new_notarization_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, notar),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let notar = VoteMessage {
            vote: Vote::new_notarization_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        votes.add_vote(voter, notar.clone()).unwrap();
        assert!(matches!(
            votes.add_vote(voter, notar),
            Err(VotePoolAddVoteError::Duplicate)
        ));
    }

    #[test]
    fn test_notar_fallback_failures() {
        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let slot = 1;

        let mut votes = VotePool::new(slot);
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, finalize).unwrap();
        let nf = VoteMessage {
            vote: Vote::new_notarization_fallback_vote(slot, Hash::default()),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, nf),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        for _ in 0..3 {
            let nf = VoteMessage {
                vote: Vote::new_notarization_fallback_vote(slot, Hash::new_unique()),
                signature,
                rank,
            };
            votes.add_vote(voter, nf).unwrap();
        }
        let nf = VoteMessage {
            vote: Vote::new_notarization_fallback_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, nf),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let nf = VoteMessage {
            vote: Vote::new_notarization_fallback_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        votes.add_vote(voter, nf.clone()).unwrap();
        assert!(matches!(
            votes.add_vote(voter, nf),
            Err(VotePoolAddVoteError::Duplicate)
        ));
    }

    #[test]
    fn test_skip_failures() {
        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let slot = 1;

        let mut votes = VotePool::new(slot);
        let notar = VoteMessage {
            vote: Vote::new_notarization_vote(slot, Hash::new_unique()),
            signature,
            rank,
        };
        votes.add_vote(voter, notar).unwrap();
        let skip = VoteMessage {
            vote: Vote::new_skip_vote(slot),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, skip),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, finalize).unwrap();
        let skip = VoteMessage {
            vote: Vote::new_skip_vote(slot),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, skip),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let skip = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, skip.clone()).unwrap();
        assert!(matches!(
            votes.add_vote(voter, skip),
            Err(VotePoolAddVoteError::Duplicate)
        ));
    }

    #[test]
    fn test_skip_fallback_failures() {
        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let slot = 1;

        let mut votes = VotePool::new(slot);
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, finalize).unwrap();
        let sf = VoteMessage {
            vote: Vote::new_skip_fallback_vote(slot),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, sf),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let sf = VoteMessage {
            vote: Vote::new_skip_fallback_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, sf.clone()).unwrap();
        assert!(matches!(
            votes.add_vote(voter, sf),
            Err(VotePoolAddVoteError::Duplicate)
        ));
    }

    #[test]
    fn test_finalize_failures() {
        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let slot = 1;

        let mut votes = VotePool::new(slot);
        let skip = VoteMessage {
            vote: Vote::new_skip_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, skip).unwrap();
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, finalize),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let sf = VoteMessage {
            vote: Vote::new_skip_fallback_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, sf).unwrap();
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        assert!(matches!(
            votes.add_vote(voter, finalize),
            Err(VotePoolAddVoteError::Invalid)
        ));

        let mut votes = VotePool::new(slot);
        let finalize = VoteMessage {
            vote: Vote::new_finalization_vote(slot),
            signature,
            rank,
        };
        votes.add_vote(voter, finalize.clone()).unwrap();
        assert!(matches!(
            votes.add_vote(voter, finalize),
            Err(VotePoolAddVoteError::Duplicate)
        ));
    }

    #[test]
    fn test_stakes() {
        let slot = 123;
        let stake = 54321;
        let mut stakes = Stakes::new(slot);
        let vote = Vote::new_skip_vote(slot);
        assert_eq!(stakes.add_stake(stake, &vote), stake);
        assert_eq!(stakes.get_stake(&vote), stake);

        let mut stakes = Stakes::new(slot);
        let vote = Vote::new_skip_fallback_vote(slot);
        assert_eq!(stakes.add_stake(stake, &vote), stake);
        assert_eq!(stakes.get_stake(&vote), stake);

        let mut stakes = Stakes::new(slot);
        let vote = Vote::new_finalization_vote(slot);
        assert_eq!(stakes.add_stake(stake, &vote), stake);
        assert_eq!(stakes.get_stake(&vote), stake);

        let mut stakes = Stakes::new(slot);
        let stake0 = 10;
        let stake1 = 20;
        let hash0 = Hash::new_unique();
        let hash1 = Hash::new_unique();
        let vote0 = Vote::new_notarization_vote(slot, hash0);
        let vote1 = Vote::new_notarization_vote(slot, hash1);
        assert_eq!(stakes.add_stake(stake0, &vote0), stake0);
        assert_eq!(stakes.add_stake(stake1, &vote1), stake1);
        assert_eq!(stakes.get_stake(&vote0), stake0);
        assert_eq!(stakes.get_stake(&vote1), stake1);

        let mut stakes = Stakes::new(slot);
        let stake0 = 10;
        let stake1 = 20;
        let hash0 = Hash::new_unique();
        let hash1 = Hash::new_unique();
        let vote0 = Vote::new_notarization_fallback_vote(slot, hash0);
        let vote1 = Vote::new_notarization_fallback_vote(slot, hash1);
        assert_eq!(stakes.add_stake(stake0, &vote0), stake0);
        assert_eq!(stakes.add_stake(stake1, &vote1), stake1);
        assert_eq!(stakes.get_stake(&vote0), stake0);
        assert_eq!(stakes.get_stake(&vote1), stake1);
    }

    #[test]
    fn test_vote_pool() {
        let slot = 1;
        let mut vote_pool = VotePool::new(slot);

        let voter = Pubkey::new_unique();
        let signature = BLSSignature::default();
        let rank = 1;
        let vote = Vote::new_finalization_vote(slot);
        let vote_message = VoteMessage {
            vote,
            signature,
            rank,
        };
        let stake = 12345;
        assert_eq!(
            vote_pool
                .add_vote(voter, stake, vote_message.clone())
                .unwrap(),
            stake
        );
        assert_eq!(vote_pool.get_stake(&vote), stake);
        let returned_votes = vote_pool.get_votes(&vote);
        assert_eq!(returned_votes.len(), 1);
        assert_eq!(returned_votes[0], vote_message);
    }
}
