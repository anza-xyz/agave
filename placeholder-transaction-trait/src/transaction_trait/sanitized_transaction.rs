use {
    crate::TransactionTrait,
    solana_sdk::{
        hash::Hash,
        signature::Signature,
        transaction::{SanitizedTransaction, TransactionAccountLocks, VersionedTransaction},
    },
};

impl TransactionTrait for SanitizedTransaction {
    fn signature(&self) -> &Signature {
        self.signatures().first().unwrap()
    }

    fn signatures(&self) -> &[Signature] {
        self.signatures()
    }

    fn message_hash(&self) -> &Hash {
        self.message_hash()
    }

    fn is_simple_vote_transaction(&self) -> bool {
        self.is_simple_vote_transaction()
    }

    fn get_account_locks_unchecked(&self) -> TransactionAccountLocks {
        SanitizedTransaction::get_account_locks_unchecked(self)
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        SanitizedTransaction::to_versioned_transaction(self)
    }
}
