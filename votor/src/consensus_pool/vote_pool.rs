use {
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::SigVerifiedVoteBatch,
        vote::{Vote, VoteType},
    },
    bitvec::vec::BitVec,
    solana_bls_signatures::SignatureProjective,
    solana_clock::Slot,
    solana_hash::Hash,
    std::{
        collections::{BTreeMap, HashMap, hash_map::Entry as HashMapEntry},
        num::NonZero,
        sync::Arc,
    },
    thiserror::Error,
};

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

    fn add_vote(
        &mut self,
        batch: &SigVerifiedVoteBatch,
    ) -> Result<NonZero<u64>, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote.slot());
        for entry in self.entries.values() {
            if has_common_bits(&batch.ranks, &entry.ranks) {
                return Err(VotePoolAddVoteError::Duplicate);
            }
        }
        let stake = match self.entries.entry(*batch.vote.block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let vote_pool_entry = e.get_mut();
                vote_pool_entry.add_vote(batch)?
            }
            HashMapEntry::Vacant(e) => {
                // TODO: initial VoteEntry so that we do not have to modify it.
                let mut entry = VoteEntry::new(self.max_validators);
                let stake = entry.add_vote(batch)?;
                e.insert(entry);
                stake
            }
        };
        self.ranks |= &batch.ranks;
        Ok(stake)
    }
}

struct GenesisVoteEntry {
    slot: Slot,
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
}

impl GenesisVoteEntry {
    fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            max_validators,
            entries: HashMap::new(),
        }
    }

    fn add_vote(
        &mut self,
        batch: &SigVerifiedVoteBatch,
    ) -> Result<NonZero<u64>, VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote.slot());
        for entry in self.entries.values() {
            if has_common_bits(&batch.ranks, &entry.ranks) {
                return Err(VotePoolAddVoteError::Duplicate);
            }
        }
        let stake = match self.entries.entry(*batch.vote.block_id().unwrap()) {
            HashMapEntry::Occupied(mut e) => {
                let vote_pool_entry = e.get_mut();
                vote_pool_entry.add_vote(batch)?
            }
            HashMapEntry::Vacant(e) => {
                // TODO: initial VoteEntry so that we do not have to modify it.
                let mut entry = VoteEntry::new(self.max_validators);
                let stake = entry.add_vote(batch)?;
                e.insert(entry);
                stake
            }
        };
        Ok(stake)
    }
}

struct NotarFallbackVoteEntry {
    slot: Slot,
    max_validators: usize,
    entries: HashMap<Hash, VoteEntry>,
    ranks: BitVec<u8>,
}

impl NotarFallbackVoteEntry {
    fn new(slot: Slot, max_validators: usize) -> Self {
        Self {
            slot,
            max_validators,
            entries: HashMap::new(),
            ranks: BitVec::with_capacity(max_validators),
        }
    }

    fn add_vote(
        &mut self,
        _batch: &SigVerifiedVoteBatch,
    ) -> Result<NonZero<u64>, VotePoolAddVoteError> {
        unimplemented!()
        // debug_assert_eq!(self.slot, batch.vote.slot());
        // for entry in self.entries.values() {
        //     if has_common_bits(&batch.ranks, &entry.ranks) {
        //         return Err(VotePoolAddVoteError::Duplicate);
        //     }
        // }
        // match self.entries.entry(*batch.vote.block_id().unwrap()) {
        //     HashMapEntry::Occupied(mut e) => {
        //         let vote_pool_entry = e.get_mut();
        //         vote_pool_entry.add_vote(&batch)?;
        //     }
        //     HashMapEntry::Vacant(e) => {
        //         // TODO: initial VoteEntry so that we do not have to modify it.
        //         let mut entry = VoteEntry::new(self.max_validators);
        //         entry.add_vote(&batch)?;
        //         e.insert(entry);
        //     }
        // }
        // self.ranks |= batch.ranks;
        // Ok(())
    }
}

struct VoteEntry {
    ranks: BitVec<u8>,
    signature: SignatureProjective,
    stake: Option<NonZero<u64>>,
}

impl VoteEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            ranks: BitVec::with_capacity(max_validators),
            signature: SignatureProjective::identity(),
            stake: None,
        }
    }

    fn add_vote(
        &mut self,
        batch: &SigVerifiedVoteBatch,
    ) -> Result<NonZero<u64>, VotePoolAddVoteError> {
        if has_common_bits(&self.ranks, &batch.ranks) {
            return Err(VotePoolAddVoteError::Duplicate);
        }
        self.ranks |= &batch.ranks;
        // TODO: handle signature error
        self.signature
            .aggregate_with(std::iter::once(&batch.signature))
            .unwrap();
        match &mut self.stake {
            None => {
                self.stake = Some(batch.stake);
                Ok(batch.stake)
            }
            Some(s) => {
                *s = s.checked_add(batch.stake.get()).unwrap();
                Ok(*s)
            }
        }
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
    max_validators: usize,
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
            max_validators,
            slot,
            skip: VoteEntry::new(max_validators),
            skip_fallback: VoteEntry::new(max_validators),
            finalize: VoteEntry::new(max_validators),
            notar: NotarVoteEntry::new(slot, max_validators),
            notar_fallback: NotarFallbackVoteEntry::new(slot, max_validators),
            genesis: GenesisVoteEntry::new(slot, max_validators),
        }
    }

    /// Adds votes and if some certs can be produced and they are not already included in the completed certs, produces them.
    pub(super) fn add_vote(
        &mut self,
        batch: &SigVerifiedVoteBatch,
        _completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<(NonZero<u64>, Vec<Certificate>), VotePoolAddVoteError> {
        debug_assert_eq!(self.slot, batch.vote.slot());
        let _stake = match batch.vote {
            Vote::Notarize(_) => {
                if has_common_bits(&batch.ranks, &self.skip.ranks) {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar.add_vote(batch)
            }
            Vote::Skip(_) => {
                if has_common_bits(&self.notar.ranks, &batch.ranks)
                    || has_common_bits(&self.finalize.ranks, &batch.ranks)
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip.add_vote(batch)
            }
            Vote::SkipFallback(_) => {
                if has_common_bits(&self.finalize.ranks, &batch.ranks) {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.skip_fallback.add_vote(batch)
            }
            Vote::Finalize(_) => {
                if has_common_bits(&self.skip.ranks, &batch.ranks)
                    || has_common_bits(&self.skip_fallback.ranks, &batch.ranks)
                    || has_common_bits(&self.notar_fallback.ranks, &batch.ranks)
                {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.finalize.add_vote(batch)
            }
            Vote::Genesis(_) => self.genesis.add_vote(batch),
            Vote::NotarizeFallback(_) => {
                if has_common_bits(&self.finalize.ranks, &batch.ranks) {
                    return Err(VotePoolAddVoteError::Invalid);
                }
                self.notar_fallback.add_vote(batch)
            }
        }?;
        unimplemented!()
    }
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
