use {
    agave_bls_sigverify::sig_verified_messages::VoteAggregate, agave_votor_messages::vote::Vote,
    bitvec::vec::BitVec, solana_bls_signatures::Signature as BLSSignature,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
};

#[derive(Debug, Clone)]
pub(super) struct PoolVoteAggregate {
    vote: Vote,
    signature: BLSSignature,
    original_ranks: BitVec<u8>,
    updated_stake: u64,
    updated_ranks: BitVec<u8>,
}

impl PoolVoteAggregate {
    pub(super) fn new(aggregate: &VoteAggregate) -> Self {
        // TODO: VoteAggregate should implement a deconstructor
        Self {
            vote: *aggregate.vote(),
            signature: *aggregate.signature(),
            original_ranks: aggregate.ranks().clone(),
            updated_stake: aggregate.stake().get(),
            updated_ranks: aggregate.ranks().clone(),
        }
    }

    pub(super) fn subtract_ranks(&mut self, rank_map: &BLSPubkeyToRankMap, ranks: &BitVec<u8>) {
        for rank in ranks.iter_ones() {
            self.subtract_rank(rank_map, rank);
        }
    }

    pub(super) fn subtract_rank(&mut self, rank_map: &BLSPubkeyToRankMap, rank: usize) {
        // TODO: handle out of bounds access
        if self.updated_ranks[rank] {
            let stake_entry = rank_map.get_pubkey_stake_entry(rank).unwrap();
            self.updated_stake = self.updated_stake.saturating_sub(stake_entry.stake.get());
            self.updated_ranks.set(rank, false);
        }
    }

    pub(super) fn vote(&self) -> &Vote {
        &self.vote
    }

    /// Returns the ranks of all the validators included in the aggregate.
    pub(super) fn signed_ranks(&self) -> &BitVec<u8> {
        &self.original_ranks
    }

    pub(super) fn signature(&self) -> &BLSSignature {
        &self.signature
    }

    pub(super) fn stake(&self) -> u64 {
        self.updated_stake
    }
}
