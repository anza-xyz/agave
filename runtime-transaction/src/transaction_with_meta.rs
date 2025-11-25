use {
    crate::transaction_meta::StaticMeta,
    solana_svm_transaction::svm_transaction::SVMTransaction,
    solana_transaction::{sanitized::SanitizedTransaction, versioned::VersionedTransaction},
    std::borrow::Cow,
};

pub trait TransactionWithMeta: StaticMeta + SVMTransaction {
    /// Required to interact with geyser plugins.
    /// This function should not be used except for interacting with geyser.
    /// It may do numerous allocations that negatively impact performance.
    fn as_sanitized_transaction(&self) -> Cow<'_, SanitizedTransaction>;
    /// Required to interact with several legacy interfaces that require
    /// `VersionedTransaction`. This should not be used unless necessary, as it
    /// performs numerous allocations that negatively impact performance.
    fn to_versioned_transaction(&self) -> VersionedTransaction;
}
