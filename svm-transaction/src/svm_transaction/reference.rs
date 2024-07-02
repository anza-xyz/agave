use {
    crate::SVMTransaction,
    solana_sdk::{hash::Hash, signature::Signature, transaction::VersionedTransaction},
};

// For any type that implements `TransactionTrait`, a reference to that type
// should also implement `TransactionTrait`.
impl<T: SVMTransaction> SVMTransaction for &T {
    fn signature(&self) -> &Signature {
        SVMTransaction::signature(*self)
    }

    fn signatures(&self) -> &[Signature] {
        SVMTransaction::signatures(*self)
    }

    fn message_hash(&self) -> &Hash {
        SVMTransaction::message_hash(*self)
    }

    fn is_simple_vote_transaction(&self) -> bool {
        SVMTransaction::is_simple_vote_transaction(*self)
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        SVMTransaction::to_versioned_transaction(*self)
    }
}
