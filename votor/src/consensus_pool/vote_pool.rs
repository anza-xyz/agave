use {
    crate::common::Stake,
    agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::collections::{btree_map::Entry, BTreeMap},
    thiserror::Error,
};

#[derive(Debug, PartialEq, Eq, Error)]
pub(super) enum AddVoteError {
    #[error("duplicate vote")]
    Duplicate,
    #[error("slashable behavior")]
    Slash,
}

/// Container to store per slot votes.
struct Votes {
    /// The slot this instance of Votes is responsible for.
    slot: Slot,
    /// Skip votes are stored in map indexed by validator.
    skip: BTreeMap<Pubkey, VoteMessage>,
    /// Skip fallback votes are stored in map indexed by validator.
    skip_fallback: BTreeMap<Pubkey, VoteMessage>,
    /// Finalize votes are stored in map indexed by validator.
    finalize: BTreeMap<Pubkey, VoteMessage>,
    /// Notar votes are stored in map indexed by validator.
    notar: BTreeMap<Pubkey, VoteMessage>,
    /// A validator can vote notar fallback on upto 3 blocks.
    ///
    /// Per validator, we store a map of which block ids the validator has voted notar fallback on.
    notar_fallback: BTreeMap<Pubkey, BTreeMap<Hash, VoteMessage>>,
}

impl Votes {
    fn new(slot: Slot) -> Self {
        Self {
            slot,
            skip: BTreeMap::default(),
            skip_fallback: BTreeMap::default(),
            finalize: BTreeMap::default(),
            notar: BTreeMap::default(),
            notar_fallback: BTreeMap::default(),
        }
    }
    /// Adds votes.
    ///
    /// Checks for different types of slashable behavior and duplicate votes returning appropriate errors.
    fn add_vote(&mut self, voter: Pubkey, vote: VoteMessage) -> Result<(), AddVoteError> {
        assert_eq!(self.slot, vote.vote.slot());
        match vote.vote {
            Vote::Notarize(notar) => {
                if self.skip.contains_key(&voter) {
                    return Err(AddVoteError::Slash);
                }
                match self.notar.entry(voter) {
                    Entry::Occupied(e) => {
                        if e.get().vote.block_id().unwrap() == &notar.block_id {
                            Err(AddVoteError::Duplicate)
                        } else {
                            Err(AddVoteError::Slash)
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert(vote);
                        Ok(())
                    }
                }
            }
            Vote::NotarizeFallback(nf) => {
                if self.finalize.contains_key(&voter) {
                    return Err(AddVoteError::Slash);
                }
                match self.notar_fallback.entry(voter) {
                    Entry::Vacant(e) => {
                        let mut map = BTreeMap::new();
                        map.insert(nf.block_id, vote);
                        e.insert(map);
                        Ok(())
                    }
                    Entry::Occupied(mut e) => {
                        let map = e.get_mut();
                        let len = map.len();
                        match map.entry(nf.block_id) {
                            Entry::Vacant(map_e) => {
                                if len == 3 {
                                    Err(AddVoteError::Slash)
                                } else {
                                    map_e.insert(vote);
                                    Ok(())
                                }
                            }
                            Entry::Occupied(_) => Err(AddVoteError::Duplicate),
                        }
                    }
                }
            }
            Vote::Skip(_) => {
                if self.notar.contains_key(&voter) || self.finalize.contains_key(&voter) {
                    return Err(AddVoteError::Slash);
                }
                match self.skip.entry(voter) {
                    Entry::Occupied(_) => Err(AddVoteError::Duplicate),
                    Entry::Vacant(e) => {
                        e.insert(vote);
                        Ok(())
                    }
                }
            }
            Vote::SkipFallback(_) => {
                if self.finalize.contains_key(&voter) {
                    return Err(AddVoteError::Slash);
                }
                match self.skip_fallback.entry(voter) {
                    Entry::Occupied(_) => Err(AddVoteError::Duplicate),
                    Entry::Vacant(e) => {
                        e.insert(vote);
                        Ok(())
                    }
                }
            }
            Vote::Finalize(_) => {
                if self.skip.contains_key(&voter) || self.skip_fallback.contains_key(&voter) {
                    return Err(AddVoteError::Slash);
                }
                if let Some(map) = self.notar_fallback.get(&voter) {
                    assert!(!map.is_empty());
                    return Err(AddVoteError::Slash);
                }
                match self.finalize.entry(voter) {
                    Entry::Occupied(_) => Err(AddVoteError::Duplicate),
                    Entry::Vacant(e) => {
                        e.insert(vote);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Get votes for the corresponding [`VoteType`] and block id.
    ///
    // TODO: figure out how to return an iterator here instead which would require `CertificateBuilder::aggregate()` to accept an iterator.
    fn get_votes(&self, vote: &Vote) -> Vec<VoteMessage> {
        match vote {
            Vote::Finalize(_) => self.finalize.values().cloned().collect(),
            Vote::Notarize(notar) => self
                .notar
                .values()
                .filter(|vote| vote.vote.block_id().unwrap() == &notar.block_id)
                .cloned()
                .collect(),
            Vote::NotarizeFallback(nf) => self
                .notar_fallback
                .values()
                .filter_map(|map| map.get(&nf.block_id))
                .cloned()
                .collect(),
            Vote::Skip(_) => self.skip.values().cloned().collect(),
            Vote::SkipFallback(_) => self.skip_fallback.values().cloned().collect(),
        }
    }
}

/// Container to store the total stakes for different types of votes.
struct Stakes {
    slot: Slot,
    /// Total stake that has voted skip.
    skip: Stake,
    /// Total stake that has voted skil fallback.
    skip_fallback: Stake,
    /// Total stake that has voted finalize.
    finalize: Stake,
    /// Stake that has voted notar.
    ///
    /// Different validators may vote notar for different blocks, so this tracks stake per block id.
    notar: BTreeMap<Hash, Stake>,
    /// Stake that has voted notar fallback.
    ///
    /// A single validator may vote for upto 3 blocks and different validators can vote for different blocks.
    /// Hence, this tracks stake per block id.
    notar_fallback: BTreeMap<Hash, Stake>,
}

impl Stakes {
    fn new(slot: Slot) -> Self {
        Self {
            slot,
            skip: 0,
            skip_fallback: 0,
            finalize: 0,
            notar: BTreeMap::default(),
            notar_fallback: BTreeMap::default(),
        }
    }

    /// Updates the corresponding stake after a vote has been successfully added to the pool.
    ///
    /// Returns the total stake of the corresponding type (and block id in case of notar or notar-fallback) after the update.
    fn add_stake(&mut self, voter_stake: Stake, vote: &Vote) -> Stake {
        assert_eq!(self.slot, vote.slot());
        match vote {
            Vote::Notarize(notar) => {
                let stake = self.notar.entry(notar.block_id).or_default();
                *stake = (*stake).saturating_add(voter_stake);
                *stake
            }
            Vote::NotarizeFallback(nf) => {
                let stake = self.notar_fallback.entry(nf.block_id).or_default();
                *stake = (*stake).saturating_add(voter_stake);
                *stake
            }
            Vote::Skip(_) => {
                self.skip = self.skip.saturating_add(voter_stake);
                self.skip
            }
            Vote::SkipFallback(_) => {
                self.skip_fallback = self.skip_fallback.saturating_add(voter_stake);
                self.skip_fallback
            }
            Vote::Finalize(_) => {
                self.finalize = self.finalize.saturating_add(voter_stake);
                self.finalize
            }
        }
    }

    /// Get the stake corresponding to the [`VoteType`] and block id.
    fn get_stake(&self, vote: &Vote) -> Stake {
        match vote {
            Vote::Notarize(notar) => *self.notar.get(&notar.block_id).unwrap_or(&0),
            Vote::NotarizeFallback(nf) => *self.notar_fallback.get(&nf.block_id).unwrap_or(&0),
            Vote::Skip(_) => self.skip,
            Vote::SkipFallback(_) => self.skip_fallback,
            Vote::Finalize(_) => self.finalize,
        }
    }
}

/// Container to store per slot votes and associated stake.
pub(super) struct VotePool {
    /// The slot this instance of the pool is responsible for.
    slot: Slot,
    /// Stores seen votes.
    votes: Votes,
    /// Stores total stake that voted.
    stakes: Stakes,
}

impl VotePool {
    pub(super) fn new(slot: Slot) -> Self {
        Self {
            slot,
            votes: Votes::new(slot),
            stakes: Stakes::new(slot),
        }
    }

    /// Adds a vote to the pool.
    ///
    /// On success, returns the total stake of the corresponding vote type.
    pub(super) fn add_vote(
        &mut self,
        voter: Pubkey,
        voter_stake: Stake,
        msg: VoteMessage,
    ) -> Result<Stake, AddVoteError> {
        assert_eq!(self.slot, msg.vote.slot());
        let vote = msg.vote;
        self.votes.add_vote(voter, msg)?;
        Ok(self.stakes.add_stake(voter_stake, &vote))
    }

    /// Returns the [`Stake`] corresponding to the specific [`Vote`].
    pub(super) fn get_stake(&self, vote: &Vote) -> Stake {
        self.stakes.get_stake(vote)
    }

    /// Returns a list of votes corresponding to the specific [`Vote`].
    pub(super) fn get_votes(&self, vote: &Vote) -> Vec<VoteMessage> {
        self.votes.get_votes(vote)
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

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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
            Err(AddVoteError::Slash)
        ));

        let mut votes = Votes::new(slot);
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
        let vote0 = Vote::new_notarization_vote(slot, Hash::new_unique());
        let vote1 = Vote::new_notarization_vote(slot, Hash::new_unique());
        assert_eq!(stakes.add_stake(stake0, &vote0), stake0);
        assert_eq!(stakes.add_stake(stake1, &vote1), stake1);
        assert_eq!(stakes.get_stake(&vote0), stake0);
        assert_eq!(stakes.get_stake(&vote1), stake1);

        let mut stakes = Stakes::new(slot);
        let stake0 = 10;
        let stake1 = 20;
        let vote0 = Vote::new_notarization_fallback_vote(slot, Hash::new_unique());
        let vote1 = Vote::new_notarization_fallback_vote(slot, Hash::new_unique());
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
        let returned_votes = vote_pool.get_votes(&Vote::new_finalization_vote(slot));
        assert_eq!(returned_votes.len(), 1);
        assert_eq!(returned_votes[0], vote_message);
    }
}
