use {
    crate::transaction_meta::TransactionMeta,
    solana_pubkey::Pubkey,
    solana_svm_transaction::{
        svm_message::{SVMMessage, SVMStaticMessage},
        svm_transaction::{SVMStaticTransaction, SVMTransaction},
    },
    solana_transaction::{sanitized::SanitizedTransaction, versioned::VersionedTransaction},
    std::borrow::Cow,
};

pub trait StaticMessageWithMeta: TransactionMeta + SVMStaticMessage {}
impl<T: TransactionMeta + SVMStaticMessage> StaticMessageWithMeta for T {}

pub fn writable_accounts(transaction: &impl SVMMessage) -> impl Iterator<Item = &Pubkey> + Clone {
    transaction
        .account_keys()
        .iter()
        .enumerate()
        .filter_map(|(index, key)| transaction.is_writable(index).then_some(key))
}

pub trait StaticTransactionWithMeta: TransactionMeta + SVMStaticTransaction {
    /// Required to interact with several legacy interfaces that require
    /// `VersionedTransaction`. This should not be used unless necessary, as it
    /// performs numerous allocations that negatively impact performance.
    fn to_versioned_transaction(&self) -> VersionedTransaction;

    /// Returns the serialized transaction size in bytes.
    /// Runtime metadata is not included.
    fn serialized_size(&self) -> usize;
}

pub trait TransactionWithMeta: StaticTransactionWithMeta + SVMTransaction {
    /// Required to interact with geyser plugins.
    /// This function should not be used except for interacting with geyser.
    /// It may do numerous allocations that negatively impact performance.
    fn as_sanitized_transaction(&self) -> Cow<'_, SanitizedTransaction>;
}
