use {
    solana_sdk::{
        hash::Hash,
        message::{v0::LoadedAddresses, SanitizedMessage},
        signature::Signature,
        transaction::{
            Result, SanitizedTransaction, TransactionAccountLocks, VersionedTransaction,
        },
    },
    std::time::Instant,
};

/// Sanitized transaction with optional start_time
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ExtendedSanitizedTransaction {
    transaction: SanitizedTransaction,
    start_time: Option<Instant>,
}

impl From<SanitizedTransaction> for ExtendedSanitizedTransaction {
    fn from(value: SanitizedTransaction) -> Self {
        Self {
            transaction: value,
            start_time: None,
        }
    }
}

impl ExtendedSanitizedTransaction {
    pub fn new(transaction: SanitizedTransaction, start_time: Option<Instant>) -> Self {
        Self {
            transaction,
            start_time,
        }
    }

    pub fn transaction(&self) -> &SanitizedTransaction {
        &self.transaction
    }

    pub fn start_time(&self) -> &Option<Instant> {
        &self.start_time
    }

    /// Return the first signature for this transaction.
    ///
    /// Notes:
    ///
    /// Sanitized transactions must have at least one signature because the
    /// number of signatures must be greater than or equal to the message header
    /// value `num_required_signatures` which must be greater than 0 itself.
    #[inline(always)]
    pub fn signature(&self) -> &Signature {
        self.transaction.signature()
    }

    /// Return the list of signatures for this transaction
    #[inline(always)]
    pub fn signatures(&self) -> &[Signature] {
        self.transaction.signatures()
    }

    /// Return the signed message
    #[inline(always)]
    pub fn message(&self) -> &SanitizedMessage {
        self.transaction.message()
    }

    /// Return the hash of the signed message
    #[inline(always)]
    pub fn message_hash(&self) -> &Hash {
        self.transaction.message_hash()
    }

    /// Returns true if this transaction is a simple vote
    #[inline(always)]
    pub fn is_simple_vote_transaction(&self) -> bool {
        self.transaction.is_simple_vote_transaction()
    }

    /// Convert this sanitized transaction into a versioned transaction for
    /// recording in the ledger.
    #[inline(always)]
    pub fn to_versioned_transaction(&self) -> VersionedTransaction {
        self.transaction.to_versioned_transaction()
    }

    /// Validate and return the account keys locked by this transaction
    #[inline(always)]
    pub fn get_account_locks(
        &self,
        tx_account_lock_limit: usize,
    ) -> Result<TransactionAccountLocks> {
        self.transaction.get_account_locks(tx_account_lock_limit)
    }

    /// Return the list of accounts that must be locked during processing this transaction.
    #[inline(always)]
    pub fn get_account_locks_unchecked(&self) -> TransactionAccountLocks {
        self.transaction.get_account_locks_unchecked()
    }

    /// Return the list of addresses loaded from on-chain address lookup tables
    #[inline(always)]
    pub fn get_loaded_addresses(&self) -> LoadedAddresses {
        self.transaction.get_loaded_addresses()
    }
}
