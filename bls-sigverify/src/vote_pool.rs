use {
    agave_votor_messages::{
        reward_certificate::NUM_SLOTS_FOR_REWARD, unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_hash::Hash,
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
    genesis: Vec<Option<Hash>>,
    notar: Vec<Option<Hash>>,
    notar_fallback: Vec<SmallVec<[Hash; MAX_NOTAR_FALLBACK_ENTRIES]>>,
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

    pub(super) fn prune(&mut self, root_slot: Slot) {
        // To support rewards, we need to keep older notar and skip votes.
        // Simpler to keep all votes for older slots.
        let slot_to_keep = root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD);
        self.entries.retain(|slot, _| slot >= &slot_to_keep);
    }
}
