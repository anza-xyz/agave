//! Test-only Byzantine vote mutations.
//!
//! A `VoteMutationSchedule` maps slots to a mutation. When the validator casts a
//! vote for a scheduled slot, its own consensus pool and vote history still record
//! the honest vote, but the vote it broadcasts is replaced by `mutate`'s output.
//! Mutated votes are signed with the validator's real BLS key.

use {
    agave_votor_messages::{consensus_message::Block, vote::Vote},
    solana_clock::Slot,
    solana_hash::Hash,
    std::{
        collections::BTreeMap,
        sync::{Arc, RwLock},
    },
};

/// The largest forward shift applied by `VoteMutationKind::BadSlot`. Kept small so the
/// mutated slot stays within an epoch whose rank map is known.
const MAX_BAD_SLOT_OFFSET: u64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoteMutationKind {
    /// Replace the vote with the same vote for a later slot.
    BadSlot,
    /// Replace the block_id of notarize(-fallback) votes with a bogus one.
    BadBlockId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoteMutation {
    pub kind: VoteMutationKind,
    /// Deterministically selects the bad slot offset and bogus block_id.
    pub seed: u64,
}

/// A dynamically configurable set of vote mutations, indexed by slot.
///
/// The handle is cloneable so a local-cluster test can retain a controller while
/// the validator's voting context owns another clone.
#[derive(Clone, Debug, Default)]
pub struct VoteMutationSchedule {
    mutations: Arc<RwLock<BTreeMap<Slot, VoteMutation>>>,
}

impl VoteMutationSchedule {
    /// Replaces the scheduled mutation for `slot`.
    pub fn set_slot_mutation(&self, slot: Slot, mutation: VoteMutation) {
        self.mutations.write().unwrap().insert(slot, mutation);
    }

    /// Removes the scheduled mutation for `slot`.
    pub fn clear_slot(&self, slot: Slot) {
        self.mutations.write().unwrap().remove(&slot);
    }

    pub(crate) fn mutation(&self, slot: Slot) -> Option<VoteMutation> {
        self.mutations.read().unwrap().get(&slot).copied()
    }
}

fn bogus_block(slot: Slot, seed: u64) -> Block {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    bytes[8..16].copy_from_slice(&slot.to_le_bytes());
    bytes[24..].copy_from_slice(b"byzvote!");
    Block {
        slot,
        block_id: Hash::new_from_array(bytes).into(),
    }
}

/// Returns the vote to broadcast in place of `vote`. Votes that a mutation does not
/// apply to are returned unchanged.
pub fn mutate(vote: Vote, mutation: VoteMutation) -> Vote {
    let VoteMutation { kind, seed } = mutation;
    match kind {
        VoteMutationKind::BadSlot => {
            let shift = |slot: Slot| slot.saturating_add(1 + seed % MAX_BAD_SLOT_OFFSET);
            let shift_block = |block: Block| Block {
                slot: shift(block.slot),
                ..block
            };
            match vote {
                Vote::Notarize(v) => Vote::new_notarization_vote(shift_block(v.block)),
                Vote::NotarizeFallback(v) => {
                    Vote::new_notarization_fallback_vote(shift_block(v.block))
                }
                Vote::Finalize(v) => Vote::new_finalization_vote(shift(v.slot)),
                Vote::Skip(v) => Vote::new_skip_vote(shift(v.slot)),
                Vote::SkipFallback(v) => Vote::new_skip_fallback_vote(shift(v.slot)),
                Vote::Genesis(_) => vote,
            }
        }
        VoteMutationKind::BadBlockId => match vote {
            Vote::Notarize(v) => Vote::new_notarization_vote(bogus_block(v.block.slot, seed)),
            Vote::NotarizeFallback(v) => {
                Vote::new_notarization_fallback_vote(bogus_block(v.block.slot, seed))
            }
            _ => vote,
        },
    }
}

#[cfg(test)]
mod tests {
    use {super::*, agave_votor_messages::consensus_message::BlockId};

    fn notar(slot: Slot) -> Vote {
        Vote::new_notarization_vote(Block {
            slot,
            block_id: BlockId::from(Hash::new_from_array([7; 32])),
        })
    }

    #[test]
    fn test_bad_slot_shifts_forward() {
        let mutation = VoteMutation {
            kind: VoteMutationKind::BadSlot,
            seed: 2,
        };
        let mutated = mutate(notar(10), mutation);
        assert_eq!(mutated.slot(), 13);
        assert_eq!(mutated.block_id(), notar(10).block_id());
        assert_eq!(
            mutate(Vote::new_skip_vote(10), mutation),
            Vote::new_skip_vote(13)
        );
    }

    #[test]
    fn test_bad_block_id_only_changes_notarize_votes() {
        let mutation = VoteMutation {
            kind: VoteMutationKind::BadBlockId,
            seed: 1,
        };
        let mutated = mutate(notar(10), mutation);
        assert_eq!(mutated.slot(), 10);
        assert_ne!(mutated.block_id(), notar(10).block_id());
        assert_eq!(mutate(notar(10), mutation), mutated);
        assert_eq!(
            mutate(Vote::new_skip_vote(10), mutation),
            Vote::new_skip_vote(10)
        );
    }
}
