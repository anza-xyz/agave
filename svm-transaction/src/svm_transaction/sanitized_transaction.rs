use {
    crate::svm_transaction::SVMTransaction, solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
};

impl SVMTransaction for SanitizedTransaction {
    fn signature(&self) -> &Signature {
        SanitizedTransaction::signature(self)
    }

    fn signatures(&self) -> &[Signature] {
        SanitizedTransaction::signatures(self)
    }
}
