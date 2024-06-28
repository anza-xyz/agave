use {
    crate::TransactionTrait,
    solana_sdk::{
        hash::Hash,
        signature::Signature,
        transaction::{TransactionAccountLocks, VersionedTransaction},
    },
};

// For any type that implements `TransactionTrait`, a reference to that type
// should also implement `TransactionTrait`.
impl<T: TransactionTrait> TransactionTrait for &T {
    fn signature(&self) -> &Signature {
        TransactionTrait::signature(*self)
    }

    fn signatures(&self) -> &[Signature] {
        TransactionTrait::signatures(*self)
    }

    fn message_hash(&self) -> &Hash {
        TransactionTrait::message_hash(*self)
    }

    fn is_simple_vote_transaction(&self) -> bool {
        TransactionTrait::is_simple_vote_transaction(*self)
    }

    fn get_account_locks_unchecked(&self) -> TransactionAccountLocks {
        TransactionTrait::get_account_locks_unchecked(*self)
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        TransactionTrait::to_versioned_transaction(*self)
    }
}
