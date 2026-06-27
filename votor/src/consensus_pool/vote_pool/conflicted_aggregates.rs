use {
    crate::consensus_pool::vote_pool::has_common_bits,
    agave_bls_sigverify::sig_verified_messages::VoteAggregate,
    agave_votor_messages::vote::Vote,
    bitvec::vec::BitVec,
    solana_bls_signatures::SignatureProjective,
    solana_hash::Hash,
    std::collections::{HashMap, HashSet},
};

/// Tracks vote aggregates that conflicted with existing aggregates in the vote pool but should
/// still be considered when building a certificate.
#[derive(Debug, Default)]
pub(super) struct ConflictedAggregates {
    skip: HashSet<VoteAggregate>,
    skip_fallback: HashSet<VoteAggregate>,
    finalize: HashSet<VoteAggregate>,
    notar: HashMap<Hash, HashSet<VoteAggregate>>,
    notar_fallback: HashMap<Hash, HashSet<VoteAggregate>>,
    genesis: HashMap<Hash, HashSet<VoteAggregate>>,
}

impl ConflictedAggregates {
    pub(super) fn add_aggregate(&mut self, aggregate: VoteAggregate) {
        match aggregate.vote() {
            Vote::Notarize(notar) => {
                self.notar
                    .entry(notar.block.block_id)
                    .or_default()
                    .insert(aggregate);
            }
            Vote::NotarizeFallback(nf) => {
                self.notar_fallback
                    .entry(nf.block.block_id)
                    .or_default()
                    .insert(aggregate);
            }
            Vote::Genesis(genesis) => {
                self.genesis
                    .entry(genesis.block.block_id)
                    .or_default()
                    .insert(aggregate);
            }
            Vote::Finalize(_) => {
                self.finalize.insert(aggregate);
            }
            Vote::Skip(_) => {
                self.skip.insert(aggregate);
            }
            Vote::SkipFallback(_) => {
                self.skip_fallback.insert(aggregate);
            }
        }
    }

    pub(super) fn add_to_cert(
        &self,
        vote: &Vote,
        signature: &mut SignatureProjective,
        ranks: &mut BitVec<u8>,
    ) {
        match vote {
            Vote::Notarize(notar) => {
                if let Some(aggregates) = self.notar.get(&notar.block.block_id) {
                    for aggregate in aggregates {
                        if !has_common_bits(ranks, aggregate.ranks()) {
                            *ranks |= aggregate.ranks();
                            signature
                                .aggregate_with(std::iter::once(aggregate.signature()))
                                .unwrap();
                        }
                    }
                }
            }
            Vote::NotarizeFallback(nf) => {
                if let Some(aggregates) = self.notar_fallback.get(&nf.block.block_id) {
                    for aggregate in aggregates {
                        if !has_common_bits(ranks, aggregate.ranks()) {
                            *ranks |= aggregate.ranks();
                            signature
                                .aggregate_with(std::iter::once(aggregate.signature()))
                                .unwrap();
                        }
                    }
                }
            }
            Vote::Genesis(genesis) => {
                if let Some(aggregates) = self.genesis.get(&genesis.block.block_id) {
                    for aggregate in aggregates {
                        if !has_common_bits(ranks, aggregate.ranks()) {
                            *ranks |= aggregate.ranks();
                            signature
                                .aggregate_with(std::iter::once(aggregate.signature()))
                                .unwrap();
                        }
                    }
                }
            }
            Vote::Finalize(_) => {
                for aggregate in &self.finalize {
                    if !has_common_bits(ranks, aggregate.ranks()) {
                        *ranks |= aggregate.ranks();
                        signature
                            .aggregate_with(std::iter::once(aggregate.signature()))
                            .unwrap();
                    }
                }
            }
            Vote::Skip(_) => {
                for aggregate in &self.skip {
                    if !has_common_bits(ranks, aggregate.ranks()) {
                        *ranks |= aggregate.ranks();
                        signature
                            .aggregate_with(std::iter::once(aggregate.signature()))
                            .unwrap();
                    }
                }
            }
            Vote::SkipFallback(_) => {
                for aggregate in &self.skip_fallback {
                    if !has_common_bits(ranks, aggregate.ranks()) {
                        *ranks |= aggregate.ranks();
                        signature
                            .aggregate_with(std::iter::once(aggregate.signature()))
                            .unwrap();
                    }
                }
            }
        }
    }
}
