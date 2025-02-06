#![cfg(feature = "full")]
#[deprecated(since = "2.2.0", note = "Use solana_transaction_error crate instead")]
pub use solana_transaction_error::{
    AddressLoaderError, SanitizeMessageError, TransactionError, TransactionResult as Result,
    TransportError, TransportResult,
};
#[deprecated(since = "2.2.0", note = "Use solana_transaction crate instead")]
pub use {
    solana_program::message::{AddressLoader, SimpleAddressLoader},
    solana_transaction::{
        sanitized::{
            MessageHash, SanitizedTransaction, TransactionAccountLocks, MAX_TX_ACCOUNT_LOCKS,
        },
        uses_durable_nonce,
        versioned::{
            sanitized::SanitizedVersionedTransaction, Legacy, TransactionVersion,
            VersionedTransaction,
        },
        Transaction, TransactionVerificationMode,
    },
};
