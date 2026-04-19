use {
    crate::svm_transaction::SVMTransaction,
    solana_message::SanitizedMessage,
    solana_signature::Signature,
    solana_transaction::{sanitized::SanitizedTransaction, versioned::TransactionVersion},
};

impl SVMTransaction for SanitizedTransaction {
    fn version(&self) -> TransactionVersion {
        match self.message() {
            SanitizedMessage::Legacy(_) => TransactionVersion::LEGACY,
            SanitizedMessage::V0(_) => TransactionVersion::Number(0),
            SanitizedMessage::V1(_) => TransactionVersion::Number(1),
        }
    }

    fn signature(&self) -> &Signature {
        SanitizedTransaction::signature(self)
    }

    fn signatures(&self) -> &[Signature] {
        SanitizedTransaction::signatures(self)
    }
}
