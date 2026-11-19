use {
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status_client_types::{
        EncodedTransaction, UiMessage, UiRawMessage, UiTransaction,
    },
};

pub(crate) fn json_encode_transaction(transaction: &VersionedTransaction) -> EncodedTransaction {
    EncodedTransaction::Json(UiTransaction {
        signatures: transaction
            .signatures
            .iter()
            .map(ToString::to_string)
            .collect(),
        message: UiMessage::Raw(UiRawMessage::from(&transaction.message)),
    })
}
