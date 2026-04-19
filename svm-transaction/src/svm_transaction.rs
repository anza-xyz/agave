use {
    crate::svm_message::SVMMessage, solana_signature::Signature,
    solana_transaction::versioned::TransactionVersion,
};

mod sanitized_transaction;

pub trait SVMTransaction: SVMMessage {
    /// Get the transaction version.
    fn version(&self) -> TransactionVersion;

    /// Get the first signature of the message.
    fn signature(&self) -> &Signature;

    /// Get all the signatures of the message.
    fn signatures(&self) -> &[Signature];
}
