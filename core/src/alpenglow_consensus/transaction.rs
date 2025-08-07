use {
    alpenglow_vote::{bls_message::VoteMessage, vote::Vote},
    solana_bls_signatures::Signature as BLSSignature,
    solana_transaction::versioned::VersionedTransaction,
};

pub trait AlpenglowVoteTransaction: Clone + std::fmt::Debug {
    fn new_for_test(signature: BLSSignature, vote: Vote, rank: usize) -> Self;
}

impl AlpenglowVoteTransaction for VersionedTransaction {
    fn new_for_test(_signature: BLSSignature, _vote: Vote, _rank: usize) -> Self {
        Self::default()
    }
}

impl AlpenglowVoteTransaction for VoteMessage {
    fn new_for_test(signature: BLSSignature, vote: Vote, rank: usize) -> Self {
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
        }
    }
}
