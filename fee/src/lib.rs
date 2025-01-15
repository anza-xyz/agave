use {solana_fee_structure::FeeDetails, solana_svm_transaction::svm_message::SVMMessage};

/// Calculate fee for `SanitizedMessage`
pub fn calculate_fee(
    message: &impl SVMMessage,
    zero_fees_for_test: bool,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    remove_rounding_in_fee_calculation: bool,
) -> u64 {
    calculate_fee_details(
        message,
        zero_fees_for_test,
        lamports_per_signature,
        prioritization_fee,
        remove_rounding_in_fee_calculation,
    )
    .total_fee()
}

pub fn calculate_fee_details(
    message: &impl SVMMessage,
    zero_fees_for_test: bool,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    remove_rounding_in_fee_calculation: bool,
) -> FeeDetails {
    if zero_fees_for_test {
        return FeeDetails::default();
    }

    calculate_fee_details_internal(
        SignatureCounts::from(message),
        lamports_per_signature,
        prioritization_fee,
        remove_rounding_in_fee_calculation,
    )
}

/// This function has the actual fee calculation.
fn calculate_fee_details_internal(
    SignatureCounts {
        num_transaction_signatures,
        num_ed25519_signatures,
        num_secp256k1_signatures,
    }: SignatureCounts,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    remove_rounding_in_fee_calculation: bool,
) -> FeeDetails {
    let signature_fee = (num_transaction_signatures
        .saturating_add(num_ed25519_signatures)
        .saturating_add(num_secp256k1_signatures))
    .saturating_mul(lamports_per_signature);

    FeeDetails::new(
        signature_fee,
        prioritization_fee,
        remove_rounding_in_fee_calculation,
    )
}

struct SignatureCounts {
    pub num_transaction_signatures: u64,
    pub num_ed25519_signatures: u64,
    pub num_secp256k1_signatures: u64,
}

impl<Tx: SVMMessage> From<&Tx> for SignatureCounts {
    fn from(message: &Tx) -> Self {
        Self {
            num_transaction_signatures: message.num_transaction_signatures(),
            num_ed25519_signatures: message.num_ed25519_signatures(),
            num_secp256k1_signatures: message.num_secp256k1_signatures(),
        }
    }
}
