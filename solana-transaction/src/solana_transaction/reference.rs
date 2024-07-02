use {
    crate::SolanaTransaction,
    solana_sdk::{hash::Hash, signature::Signature, transaction::VersionedTransaction},
};

// For any type that implements `TransactionTrait`, a reference to that type
// should also implement `TransactionTrait`.
impl<T: SolanaTransaction> SolanaTransaction for &T {
    fn signature(&self) -> &Signature {
        SolanaTransaction::signature(*self)
    }

    fn signatures(&self) -> &[Signature] {
        SolanaTransaction::signatures(*self)
    }

    fn message_hash(&self) -> &Hash {
        SolanaTransaction::message_hash(*self)
    }

    fn is_simple_vote_transaction(&self) -> bool {
        SolanaTransaction::is_simple_vote_transaction(*self)
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        SolanaTransaction::to_versioned_transaction(*self)
    }
}
