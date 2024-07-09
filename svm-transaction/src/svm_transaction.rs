use {
    crate::SVMMessage,
    solana_sdk::{hash::Hash, signature::Signature, transaction::VersionedTransaction},
};

mod sanitized_transaction;

pub trait SVMTransaction: SVMMessage {
    /// Get the first signature of the message.
    fn signature(&self) -> &Signature;

    /// Get all the signatures of the message.
    fn signatures(&self) -> &[Signature];

    /// Returns the message hash.
    // TODO: consider moving this to Message
    fn message_hash(&self) -> &Hash;

    /// Returns true if the transaction is a simple vote transaction.
    // TODO: consider moving this to Message
    fn is_simple_vote_transaction(&self) -> bool;

    /// Make a versioned transaction copy of the transaction.
    // TODO: get rid of this once PoH supports generic transactions
    fn to_versioned_transaction(&self) -> VersionedTransaction;
}
