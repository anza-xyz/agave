use {
    crate::svm_transaction::SVMTransaction,
    solana_sdk::{
        hash::Hash,
        signature::Signature,
        transaction::{SanitizedTransaction, VersionedTransaction},
    },
};

impl SVMTransaction for SanitizedTransaction {
    fn signature(&self) -> &Signature {
        SanitizedTransaction::signature(self)
    }

    fn signatures(&self) -> &[Signature] {
        SanitizedTransaction::signatures(self)
    }

    fn message_hash(&self) -> &Hash {
        SanitizedTransaction::message_hash(self)
    }

    fn is_simple_vote_transaction(&self) -> bool {
        SanitizedTransaction::is_simple_vote_transaction(self)
    }

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        SanitizedTransaction::to_versioned_transaction(self)
    }
}
