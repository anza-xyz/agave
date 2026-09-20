use {
    super::versioned_message::versioned_message_from_proto,
    protosol::protos::SanitizedTransaction as ProtoSanitizedTransaction,
    solana_signature::Signature, solana_transaction::versioned::VersionedTransaction,
};

pub fn versioned_transaction_from_proto(value: &ProtoSanitizedTransaction) -> VersionedTransaction {
    let message = versioned_message_from_proto(value.message.as_ref().unwrap());
    let signatures = value
        .signatures
        .iter()
        .map(|signature| Signature::try_from(signature.as_slice()).unwrap())
        .collect();

    VersionedTransaction {
        signatures,
        message,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::versioned_transaction_from_proto,
        protosol::protos::{
            SanitizedTransaction as ProtoSanitizedTransaction,
            TransactionMessage as ProtoTransactionMessage,
        },
    };

    #[test]
    fn versioned_transaction_from_proto_preserves_empty_signatures() {
        let transaction = ProtoSanitizedTransaction {
            message: Some(ProtoTransactionMessage {
                is_legacy: true,
                ..ProtoTransactionMessage::default()
            }),
            ..ProtoSanitizedTransaction::default()
        };

        let transaction = versioned_transaction_from_proto(&transaction);

        assert!(transaction.signatures.is_empty());
    }
}
