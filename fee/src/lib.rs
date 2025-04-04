use {
    agave_feature_set::{enable_secp256r1_precompile, FeatureSet},
    solana_fee_structure::FeeDetails,
    solana_svm_transaction::svm_message::SVMMessage,
};

/// Bools indicating the activation of features relevant
/// to the fee calculation.
// DEVELOPER NOTE:
// This struct may become empty at some point. It is preferable to keep it
// instead of removing, since fees will naturally be changed via feature-gates
// in the future. Keeping this struct will help keep things organized.
#[derive(Copy, Clone)]
pub struct FeeFeatures {
    pub enable_secp256r1_precompile: bool,
}

impl From<&FeatureSet> for FeeFeatures {
    fn from(feature_set: &FeatureSet) -> Self {
        Self {
            enable_secp256r1_precompile: feature_set.is_active(&enable_secp256r1_precompile::ID),
        }
    }
}

/// Calculate fee for `SanitizedMessage`
pub fn calculate_fee(
    message: &impl SVMMessage,
    zero_fees_for_test: bool,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    fee_features: FeeFeatures,
) -> u64 {
    calculate_fee_details(
        message,
        zero_fees_for_test,
        lamports_per_signature,
        prioritization_fee,
        fee_features,
    )
    .total_fee()
}

pub fn calculate_fee_details(
    message: &impl SVMMessage,
    zero_fees_for_test: bool,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    fee_features: FeeFeatures,
) -> FeeDetails {
    if zero_fees_for_test {
        return FeeDetails::default();
    }

    FeeDetails::new(
        calculate_signature_fee(
            SignatureCounts::from(message),
            lamports_per_signature,
            fee_features.enable_secp256r1_precompile,
        ),
        prioritization_fee,
    )
}

/// Calculate fees from signatures.
fn calculate_signature_fee(
    SignatureCounts {
        num_transaction_signatures,
        num_ed25519_signatures,
        num_secp256k1_signatures,
        num_secp256r1_signatures,
    }: SignatureCounts,
    lamports_per_signature: u64,
    enable_secp256r1_precompile: bool,
) -> u64 {
    let signature_count = num_transaction_signatures
        .saturating_add(num_ed25519_signatures)
        .saturating_add(num_secp256k1_signatures)
        .saturating_add(
            u64::from(enable_secp256r1_precompile).wrapping_mul(num_secp256r1_signatures),
        );
    signature_count.saturating_mul(lamports_per_signature)
}

struct SignatureCounts {
    pub num_transaction_signatures: u64,
    pub num_ed25519_signatures: u64,
    pub num_secp256k1_signatures: u64,
    pub num_secp256r1_signatures: u64,
}

impl<Tx: SVMMessage> From<&Tx> for SignatureCounts {
    fn from(message: &Tx) -> Self {
        Self {
            num_transaction_signatures: message.num_transaction_signatures(),
            num_ed25519_signatures: message.num_ed25519_signatures(),
            num_secp256k1_signatures: message.num_secp256k1_signatures(),
            num_secp256r1_signatures: message.num_secp256r1_signatures(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_signature_fee() {
        const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

        // Impossible case - 0 signatures.
        assert_eq!(
            calculate_signature_fee(
                SignatureCounts {
                    num_transaction_signatures: 0,
                    num_ed25519_signatures: 0,
                    num_secp256k1_signatures: 0,
                    num_secp256r1_signatures: 0,
                },
                LAMPORTS_PER_SIGNATURE,
                true,
            ),
            0
        );

        // Simple signature
        assert_eq!(
            calculate_signature_fee(
                SignatureCounts {
                    num_transaction_signatures: 1,
                    num_ed25519_signatures: 0,
                    num_secp256k1_signatures: 0,
                    num_secp256r1_signatures: 0,
                },
                LAMPORTS_PER_SIGNATURE,
                true,
            ),
            LAMPORTS_PER_SIGNATURE
        );

        // Pre-compile signatures.
        assert_eq!(
            calculate_signature_fee(
                SignatureCounts {
                    num_transaction_signatures: 1,
                    num_ed25519_signatures: 2,
                    num_secp256k1_signatures: 3,
                    num_secp256r1_signatures: 4,
                },
                LAMPORTS_PER_SIGNATURE,
                true,
            ),
            10 * LAMPORTS_PER_SIGNATURE
        );

        // Pre-compile signatures (no secp256r1)
        assert_eq!(
            calculate_signature_fee(
                SignatureCounts {
                    num_transaction_signatures: 1,
                    num_ed25519_signatures: 2,
                    num_secp256k1_signatures: 3,
                    num_secp256r1_signatures: 4,
                },
                LAMPORTS_PER_SIGNATURE,
                false,
            ),
            6 * LAMPORTS_PER_SIGNATURE
        );
    }
}
