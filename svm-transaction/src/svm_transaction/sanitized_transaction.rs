use {
    crate::SVMTransaction,
    solana_sdk::{
        hash::Hash,
        signature::Signature,
        transaction::{SanitizedTransaction, VersionedTransaction},
    },
};

impl SVMTransaction for SanitizedTransaction {
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

    fn to_versioned_transaction(&self) -> VersionedTransaction {
        SanitizedTransaction::to_versioned_transaction(self)
    }
}
