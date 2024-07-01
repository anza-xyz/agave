use {
    crate::SolanaMessage,
    solana_sdk::{
        hash::Hash,
        packet::Packet,
        signature::Signature,
        transaction::{TransactionAccountLocks, TransactionError, VersionedTransaction},
    },
};

mod reference;
mod sanitized_transaction;

pub trait SolanaTransaction: SolanaMessage {
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

    /// Validate and return the account keys locked by this transaction
    // TODO: Remove this once all uses are gone. (Allocation inside of this)
    fn get_account_locks(
        &self,
        tx_account_lock_limit: usize,
    ) -> Result<TransactionAccountLocks, TransactionError> {
        self.validate_account_locks(tx_account_lock_limit)?;
        Ok(self.get_account_locks_unchecked())
    }

    /// Return the account keys locked by this transaction without validation
    // TODO: Remove this once all uses are gone. (Allocation inside of this)
    fn get_account_locks_unchecked(&self) -> TransactionAccountLocks;

    /// Make a versioned transaction copy of the transaction.
    // TODO: get rid of this once PoH supports generic transactions
    fn to_versioned_transaction(&self) -> VersionedTransaction;

    // Default implementation is to convert to versioned tx then fill packet
    // Needed to support transaction forwarding.
    fn to_packet(&self) -> Packet {
        let mut packet = Packet::default();
        let versioned_tx = self.to_versioned_transaction();
        packet
            .populate_packet(None, &versioned_tx)
            .expect("populate a packet");
        packet
    }
}
