use {
    agave_votor_messages::{
        consensus_message::BlockId, reward_certificate::NUM_SLOTS_FOR_REWARD,
        unverified_vote_message::UnverifiedVoteMessage, vote::Vote,
    },
    bitvec::vec::BitVec,
    smallvec::SmallVec,
    solana_clock::Slot,
    std::collections::HashMap,
};

const MAX_NOTAR_FALLBACK_ENTRIES: usize = 3;

pub(crate) enum VotePoolError {
    Invalid,
    Duplicate,
}

fn default_bitvec(max_validators: usize) -> BitVec<u8> {
    BitVec::repeat(false, max_validators)
}

struct SlotEntry {
    skip: BitVec<u8>,
    skip_fallback: BitVec<u8>,
    finalize: BitVec<u8>,
    genesis: Vec<Option<BlockId>>,
    notar: Vec<Option<BlockId>>,
    notar_fallback: Vec<SmallVec<[BlockId; MAX_NOTAR_FALLBACK_ENTRIES]>>,
}

impl SlotEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            skip: default_bitvec(max_validators),
            skip_fallback: default_bitvec(max_validators),
            finalize: default_bitvec(max_validators),
            genesis: vec![None; max_validators],
            notar: vec![None; max_validators],
            notar_fallback: vec![SmallVec::new(); max_validators],
        }
    }

    fn try_add_vote(
        &mut self,
        msg: &UnverifiedVoteMessage,
        rank: usize,
        max_validators: usize,
    ) -> Result<(), VotePoolError> {
        debug_assert!(rank < max_validators);
        match &msg.vote {
            Vote::Skip(_) => {
                if self.notar[rank].is_some()
                    || self.finalize[rank]
                    || self.skip_fallback[rank]
                    || self.genesis[rank].is_some()
                {
                    return Err(VotePoolError::Invalid);
                }
                if self.skip.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::SkipFallback(_) => {
                if self.finalize[rank] || self.skip[rank] || self.genesis[rank].is_some() {
                    return Err(VotePoolError::Invalid);
                }
                if self.skip_fallback.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::Finalize(_) => {
                if self.skip[rank]
                    || self.skip_fallback[rank]
                    || !self.notar_fallback[rank].is_empty()
                    || self.genesis[rank].is_some()
                {
                    return Err(VotePoolError::Invalid);
                }
                if self.finalize.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::Genesis(genesis) => {
                if self.skip[rank]
                    || self.skip_fallback[rank]
                    || self.finalize[rank]
                    || self.notar[rank].is_some()
                    || !self.notar_fallback[rank].is_empty()
                {
                    return Err(VotePoolError::Invalid);
                }
                match self.genesis[rank] {
                    None => {
                        self.genesis[rank] = Some(genesis.block.block_id);
                        Ok(())
                    }
                    Some(block_id) => {
                        if block_id == genesis.block.block_id {
                            Err(VotePoolError::Duplicate)
                        } else {
                            Err(VotePoolError::Invalid)
                        }
                    }
                }
            }
            Vote::Notarize(notar) => {
                if self.skip[rank]
                    || self.genesis[rank].is_some()
                    || self.notar_fallback[rank].contains(&notar.block.block_id)
                {
                    return Err(VotePoolError::Invalid);
                }
                match self.notar[rank] {
                    None => {
                        self.notar[rank] = Some(notar.block.block_id);
                        Ok(())
                    }
                    Some(block_id) => {
                        if block_id == notar.block.block_id {
                            Err(VotePoolError::Duplicate)
                        } else {
                            Err(VotePoolError::Invalid)
                        }
                    }
                }
            }
            Vote::NotarizeFallback(nf) => {
                if self.notar_fallback[rank].contains(&nf.block.block_id) {
                    return Err(VotePoolError::Duplicate);
                }
                if self.finalize[rank]
                    || self.genesis[rank].is_some()
                    || self.notar_fallback[rank].len() >= MAX_NOTAR_FALLBACK_ENTRIES
                {
                    return Err(VotePoolError::Invalid);
                }
                if let Some(block_id) = &self.notar[rank]
                    && block_id == &nf.block.block_id
                {
                    return Err(VotePoolError::Invalid);
                }
                self.notar_fallback[rank].push(nf.block.block_id);
                Ok(())
            }
        }
    }

    /// Undoes the state transition of a successful [`SlotEntry::try_add_vote`].
    ///
    /// Votes are admitted to the pool at ingestion, before signature
    /// verification runs, so that duplicates and conflicting votes can be
    /// rejected cheaply. A vote whose verification subsequently failed was
    /// never a valid vote, so its entry must be released: otherwise a later
    /// correctly-signed retransmission of the same vote is dropped as a
    /// duplicate, and a different vote for the same slot is rejected as a
    /// conflict and gets the sender banned again.
    ///
    /// Only the state set by the admission of this exact vote is cleared.
    /// Admission returned `Ok(())`, so no other vote's state can occupy the
    /// same fields: a second vote for the same (slot, rank, type, block) was
    /// rejected as a duplicate, and conflicting votes were rejected as
    /// invalid without mutating state.
    fn rollback_vote(&mut self, vote: &Vote, rank: usize) {
        if rank >= self.notar.len() {
            return;
        }
        match vote {
            Vote::Skip(_) => self.skip.set(rank, false),
            Vote::SkipFallback(_) => self.skip_fallback.set(rank, false),
            Vote::Finalize(_) => self.finalize.set(rank, false),
            Vote::Genesis(genesis) => {
                if self.genesis[rank].as_ref() == Some(&genesis.block.block_id) {
                    self.genesis[rank] = None;
                }
            }
            Vote::Notarize(notar) => {
                if self.notar[rank].as_ref() == Some(&notar.block.block_id) {
                    self.notar[rank] = None;
                }
            }
            Vote::NotarizeFallback(nf) => {
                if let Some(position) = self.notar_fallback[rank]
                    .iter()
                    .position(|block_id| *block_id == nf.block.block_id)
                {
                    self.notar_fallback[rank].swap_remove(position);
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct VotePool {
    entries: HashMap<Slot, SlotEntry>,
}

impl VotePool {
    pub(super) fn try_add_vote(
        &mut self,
        msg: &UnverifiedVoteMessage,
        rank: u16,
        max_validators: usize,
    ) -> Result<(), VotePoolError> {
        let rank = rank as usize;
        if rank >= max_validators {
            return Err(VotePoolError::Invalid);
        }
        let slot_entry = self
            .entries
            .entry(msg.vote.slot())
            .or_insert_with(|| SlotEntry::new(max_validators));
        slot_entry.try_add_vote(msg, rank, max_validators)
    }

    /// Releases the pool entry spent by a vote that was admitted by
    /// [`VotePool::try_add_vote`] but subsequently failed signature
    /// verification. See [`SlotEntry::rollback_vote`].
    pub(super) fn rollback_vote(&mut self, vote: &Vote, rank: usize) {
        let Some(slot_entry) = self.entries.get_mut(&vote.slot()) else {
            return;
        };
        slot_entry.rollback_vote(vote, rank);
    }

    pub(super) fn prune(&mut self, root_slot: Slot) {
        // To support rewards, we need to keep older notar and skip votes.
        // Simpler to keep all votes for older slots.
        let slot_to_keep = root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD);
        self.entries.retain(|slot, _| slot >= &slot_to_keep);
    }
}
