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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_fee_details_internal() {
        const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

        // Impossible case - 0 signatures.
        {
            let signature_counts = SignatureCounts {
                num_transaction_signatures: 0,
                num_ed25519_signatures: 0,
                num_secp256k1_signatures: 0,
            };
            let prioritization_fee = 0;

            assert_eq!(
                calculate_fee_details_internal(
                    signature_counts,
                    LAMPORTS_PER_SIGNATURE,
                    prioritization_fee,
                    false
                ),
                FeeDetails::new(0, 0, false)
            );
        }

        // Simple signature
        {
            let signature_counts = SignatureCounts {
                num_transaction_signatures: 1,
                num_ed25519_signatures: 0,
                num_secp256k1_signatures: 0,
            };
            let prioritization_fee = 0;

            assert_eq!(
                calculate_fee_details_internal(
                    signature_counts,
                    LAMPORTS_PER_SIGNATURE,
                    prioritization_fee,
                    false
                ),
                FeeDetails::new(LAMPORTS_PER_SIGNATURE, 0, false)
            );
        }

        // Simple signature and prioritization.
        {
            let signature_counts = SignatureCounts {
                num_transaction_signatures: 1,
                num_ed25519_signatures: 0,
                num_secp256k1_signatures: 0,
            };
            let prioritization_fee = 1_000;

            assert_eq!(
                calculate_fee_details_internal(
                    signature_counts,
                    LAMPORTS_PER_SIGNATURE,
                    prioritization_fee,
                    false
                ),
                FeeDetails::new(LAMPORTS_PER_SIGNATURE, 1_000, false)
            );
        }

        // Pre-compile signatures.
        {
            let signature_counts = SignatureCounts {
                num_transaction_signatures: 1,
                num_ed25519_signatures: 2,
                num_secp256k1_signatures: 3,
            };
            let prioritization_fee = 4_000;

            assert_eq!(
                calculate_fee_details_internal(
                    signature_counts,
                    LAMPORTS_PER_SIGNATURE,
                    prioritization_fee,
                    false
                ),
                FeeDetails::new(6 * LAMPORTS_PER_SIGNATURE, 4_000, false)
            );
        }
    }
}
