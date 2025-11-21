//! Container to store received votes and associated stakes and construct certificates from them.
//!
//! Implements various checks for invalid votes as defined by the Alpenglow paper e.g. lemma 20 and 22.
//! Further detects duplicate votes which are defined as identical vote from the same sender received multiple times.

use {
    crate::common::Stake,
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType, VoteMessage},
        vote::Vote,
    },
    bitvec::prelude::*,
    solana_bls_signatures::{BlsError, Signature as BLSSignature, SignatureProjective},
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_signer_store::{encode_base2, encode_base3, EncodeError},
    std::collections::{btree_map::Entry, BTreeMap, BTreeSet},
    thiserror::Error,
};

/// As per the Alpenglow paper, a validator is allowed to vote notar fallback on at most 3 different block id for a given slot.
const MAX_NOTAR_FALLBACK_PER_VALIDATOR: usize = 3;

/// Different types of error that can happen when adding a vote to the pool.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum AddVoteError {
    #[error("duplicate vote")]
    Duplicate,
    /// These are invalid votes as defined in the Alpenglow paper e.g. lemma 20 and 22.
    #[error("invalid votes")]
    Invalid,
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
}

/// Different types of errors that can be returned when building a certificate.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum BuildCertError {
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
    #[error("Encoding failed: {0:?}")]
    Encode(EncodeError),
}

fn build_sig_bitmap(
    signature: &SignatureProjective,
    mut bitmap: BitVec<u8, Lsb0>,
    fallback: Option<(&SignatureProjective, BitVec<u8, Lsb0>)>,
) -> Result<(BLSSignature, Vec<u8>), BuildCertError> {
    match fallback {
        None => {
            let new_len = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
            bitmap.resize(new_len, false);
            let bitmap = encode_base2(&bitmap).map_err(BuildCertError::Encode)?;
            Ok((signature.into(), bitmap))
        }
        Some((fallback_sig, mut fallback_bitmap)) => {
            let last_one_0 = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
            let last_one_1 = fallback_bitmap
                .last_one()
                .map_or(0, |i| i.saturating_add(1));
            let new_len = last_one_0.max(last_one_1);
            bitmap.resize(new_len, false);
            fallback_bitmap.resize(new_len, false);
            let bitmap = encode_base3(&bitmap, &fallback_bitmap).map_err(BuildCertError::Encode)?;
            let signature = SignatureProjective::aggregate([signature, fallback_sig].into_iter())?;
            Ok((signature.into(), bitmap))
        }
    }
}

/// Maximum number of validators in a certificate
///
/// There are around 1500 validators currently. For a clean power-of-two
/// implementation, we should choose either 2048 or 4096. Choose a more
/// conservative number 4096 for now. During build() we will cut off end
/// of the bitmaps if the tail contains only zeroes, so actual bitmap
/// length will be less than or equal to this number.
const MAXIMUM_VALIDATORS: usize = 4096;

fn default_bitvec() -> BitVec<u8, Lsb0> {
    BitVec::repeat(false, MAXIMUM_VALIDATORS)
}

/// A container for storing various per slot and maybe per block id data.
struct PoolEntry {
    /// In progress signature aggregate
    signature: SignatureProjective,
    /// In progress bitvec of ranks
    bitmap: BitVec<u8, Lsb0>,
    /// Accumulated votes.
    votes: BTreeMap<Pubkey, VoteMessage>,
    /// Accumulated stake.
    stake: Stake,
}

impl PoolEntry {
    /// Adds the give vote checking for duplicates and returning the accumulated stake on success.
    fn add_vote(
        &mut self,
        voter: Pubkey,
        stake: Stake,
        vote: VoteMessage,
    ) -> Result<Stake, AddVoteError> {
        match self.votes.entry(voter) {
            Entry::Occupied(_) => Err(AddVoteError::Duplicate),
            Entry::Vacant(e) => {
                self.signature
                    .aggregate_with(std::iter::once(&vote.signature))?;
                self.bitmap.set(vote.rank as usize, true);
                self.stake = self.stake.saturating_add(stake);
                e.insert(vote);
                Ok(self.stake)
            }
        }
    }
}

impl Default for PoolEntry {
    fn default() -> Self {
        Self {
            signature: SignatureProjective::identity(),
            bitmap: default_bitvec(),
            votes: BTreeMap::default(),
            stake: 0,
        }
    }
}

/// Special pool entry for notar fallback votes.
#[derive(Default)]
struct NotarFallbackPoolEntry {
    /// In a given slot, we can see multiple block ids, stores per block id entries.
    entries: BTreeMap<Hash, PoolEntry>,
    /// Additionally stores how many times voters have voted to enforce checks.
    voted: BTreeMap<Pubkey, usize>,
}

impl NotarFallbackPoolEntry {
    /// Adds vote checking for duplicate or invalid votes, returning accumulated stake for the given block id on success.
    fn add_vote(
        &mut self,
        voter: Pubkey,
        block_id: Hash,
        stake: Stake,
        vote: VoteMessage,
    ) -> Result<Stake, AddVoteError> {
        match self.voted.entry(voter) {
            Entry::Vacant(e) => self
                .entries
                .entry(block_id)
                .or_default()
                .add_vote(voter, stake, vote)
                .inspect(|_stake| {
                    e.insert(1);
                }),
            Entry::Occupied(mut e) => {
                if e.get() < &MAX_NOTAR_FALLBACK_PER_VALIDATOR {
                    self.entries
                        .entry(block_id)
                        .or_default()
                        .add_vote(voter, stake, vote)
                        .inspect(|_stake| {
                            let cnt = e.get_mut();
                            *cnt = (*cnt).saturating_add(1);
                        })
                } else if self.entries.contains_key(&block_id) {
                    Err(AddVoteError::Duplicate)
                } else {
                    Err(AddVoteError::Invalid)
                }
            }
        }
    }
}

/// Specical pool entry for notar votes.
#[derive(Default)]
struct NotarPoolEntry {
    /// Different votes may vote for different block ids, store per block id entries.
    entries: BTreeMap<Hash, PoolEntry>,
    /// Stores which voters have voted already.
    voted: BTreeSet<Pubkey>,
}

impl NotarPoolEntry {
    /// Adds vote checking for duplicate and invalid votes, returning accumulated stake for the block id on success.
    fn add_vote(
        &mut self,
        voter: Pubkey,
        stake: Stake,
        block_id: Hash,
        vote: VoteMessage,
    ) -> Result<Stake, AddVoteError> {
        if self.voted.contains(&voter) {
            // haven't seen any notar votes from this voter, can safely add vote
            self.entries
                .entry(block_id)
                .or_default()
                .add_vote(voter, stake, vote)
                .inspect(|_stake| {
                    self.voted.insert(voter);
                })
        } else if self.entries.contains_key(&block_id) {
            Err(AddVoteError::Duplicate)
        } else {
            Err(AddVoteError::Invalid)
        }
    }

    /// Returns a signature and rank bitmap suitable for building a fast finalization certificate.
    fn build_fast_finalize(
        &self,
        block_id: Hash,
        total_stake: f64,
    ) -> Option<Result<(BLSSignature, Vec<u8>), BuildCertError>> {
        self.entries.get(&block_id).and_then(|pool_entry| {
            (pool_entry.stake as f64 / total_stake > 0.8).then_some(build_sig_bitmap(
                &pool_entry.signature,
                pool_entry.bitmap.clone(),
                None,
            ))
        })
    }

    /// Returns a signature and rank bitmap suitable for building a notarization certificate.
    fn build_notarize(
        &self,
        block_id: Hash,
        total_stake: f64,
    ) -> Option<Result<(BLSSignature, Vec<u8>), BuildCertError>> {
        self.entries.get(&block_id).and_then(|pool_entry| {
            (pool_entry.stake as f64 / total_stake > 0.6).then_some(build_sig_bitmap(
                &pool_entry.signature,
                pool_entry.bitmap.clone(),
                None,
            ))
        })
    }
}

/// Container to store per slot votes.
#[derive(Default)]
pub(super) struct VotePool {
    skip: PoolEntry,
    skip_fallback: PoolEntry,
    finalize: PoolEntry,
    notar: NotarPoolEntry,
    notar_fallback: NotarFallbackPoolEntry,
}

impl VotePool {
    /// Adds votes.
    ///
    /// Checks for different types of invalid and duplicate votes returning appropriate errors.
    pub(super) fn add_vote(
        &mut self,
        voter: Pubkey,
        stake: Stake,
        vote: VoteMessage,
    ) -> Result<Stake, AddVoteError> {
        match vote.vote {
            Vote::Notarize(notar) => {
                if self.skip.votes.contains_key(&voter) {
                    return Err(AddVoteError::Invalid);
                }
                self.notar.add_vote(voter, stake, notar.block_id, vote)
            }
            Vote::NotarizeFallback(notar_fallback) => {
                if self.finalize.votes.contains_key(&voter) {
                    return Err(AddVoteError::Invalid);
                }
                self.notar_fallback
                    .add_vote(voter, notar_fallback.block_id, stake, vote)
            }
            Vote::Skip(_) => {
                if self.notar.voted.contains(&voter) || self.finalize.votes.contains_key(&voter) {
                    return Err(AddVoteError::Invalid);
                }
                self.skip.add_vote(voter, stake, vote)
            }
            Vote::SkipFallback(_) => {
                if self.finalize.votes.contains_key(&voter) {
                    return Err(AddVoteError::Invalid);
                }
                self.skip_fallback.add_vote(voter, stake, vote)
            }
            Vote::Finalize(_) => {
                if self.skip.votes.contains_key(&voter)
                    || self.skip_fallback.votes.contains_key(&voter)
                {
                    return Err(AddVoteError::Invalid);
                }
                if self.notar_fallback.voted.contains_key(&voter) {
                    return Err(AddVoteError::Invalid);
                }
                self.finalize.add_vote(voter, stake, vote)
            }
        }
    }

    /// If enough stake has accumulated to build the given [`CertificateType`], tries to build it.
    pub(super) fn build_cert(
        &self,
        cert_type: CertificateType,
        total_stake: Stake,
    ) -> Option<Result<Certificate, BuildCertError>> {
        let total_stake = total_stake as f64;
        match cert_type {
            CertificateType::Finalize(_) => (self.finalize.stake as f64 / total_stake > 0.6)
                .then_some(
                    build_sig_bitmap(&self.finalize.signature, self.finalize.bitmap.clone(), None)
                        .map(|(signature, bitmap)| Certificate {
                            cert_type,
                            signature,
                            bitmap,
                        }),
                ),
            CertificateType::FinalizeFast(_, block_id) => self
                .notar
                .build_fast_finalize(block_id, total_stake)
                .map(|res| {
                    res.map(|(signature, bitmap)| Certificate {
                        cert_type,
                        signature,
                        bitmap,
                    })
                }),
            CertificateType::Notarize(_, block_id) => {
                self.notar.build_notarize(block_id, total_stake).map(|res| {
                    res.map(|(signature, bitmap)| Certificate {
                        cert_type,
                        signature,
                        bitmap,
                    })
                })
            }
            CertificateType::NotarizeFallback(_, block_id) => {
                let (notar_stake, notar_sig_bitmap) = match self.notar.entries.get(&block_id) {
                    None => (0, None),
                    Some(e) => (e.stake, Some((&e.signature, &e.bitmap))),
                };
                let (nf_stake, nf_sig_bitmap) = match self.notar_fallback.entries.get(&block_id) {
                    None => (0, None),
                    Some(e) => (e.stake, Some((&e.signature, &e.bitmap))),
                };
                let accumulated_stake = notar_stake.saturating_add(nf_stake);

                (accumulated_stake as f64 / total_stake > 0.6).then_some({
                    let (signature, bitmap) = match notar_sig_bitmap {
                        None => (&SignatureProjective::identity(), default_bitvec()),
                        Some((sig, bitmap)) => (sig, bitmap.clone()),
                    };
                    build_sig_bitmap(
                        signature,
                        bitmap,
                        nf_sig_bitmap.map(|(s, b)| (s, b.clone())),
                    )
                    .map(|(signature, bitmap)| Certificate {
                        cert_type,
                        signature,
                        bitmap,
                    })
                })
            }
            CertificateType::Skip(_) => {
                let accumulated_stake =
                    self.skip.stake.saturating_add(self.skip_fallback.stake) as f64;
                let fallback = (self.skip_fallback.stake != 0).then_some((
                    &self.skip_fallback.signature,
                    self.skip_fallback.bitmap.clone(),
                ));
                (accumulated_stake / total_stake > 0.6).then_some(
                    build_sig_bitmap(&self.skip.signature, self.skip.bitmap.clone(), fallback).map(
                        |(signature, bitmap)| Certificate {
                            cert_type,
                            signature,
                            bitmap,
                        },
                    ),
                )
            }
        }
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Duplicate)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Duplicate)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Duplicate)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Duplicate)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Invalid)
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
            Err(AddVoteError::Duplicate)
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
    }
}
