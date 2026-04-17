#![cfg(feature = "agave-unstable-api")]
use {
    agave_feature_set::FeatureSet, solana_fee_structure::FeeDetails,
    solana_svm_transaction::svm_message::SVMStaticMessage,
};

/// Bools indicating the activation of features relevant
/// to the fee calculation.
// DEVELOPER NOTE:
// This struct may become empty at some point. It is preferable to keep it
// instead of removing, since fees will naturally be changed via feature-gates
// in the future. Keeping this struct will help keep things organized.
#[derive(Copy, Clone, Default)]
pub struct FeeFeatures {
    pub halve_simple_vote_lamports_per_signature: bool,
}

impl From<&FeatureSet> for FeeFeatures {
    fn from(feature_set: &FeatureSet) -> Self {
        Self {
            halve_simple_vote_lamports_per_signature: feature_set
                .is_active(&agave_feature_set::halve_slot_times::id()),
        }
    }
}

/// Calculate fee for `SanitizedMessage`
pub fn calculate_fee(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    fee_features: FeeFeatures,
) -> u64 {
    calculate_fee_details(
        message,
        lamports_per_signature,
        prioritization_fee,
        fee_features,
    )
    .total_fee()
}

pub fn calculate_fee_details(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    fee_features: FeeFeatures,
) -> FeeDetails {
    calculate_fee_details_with_flags(
        message,
        lamports_per_signature,
        prioritization_fee,
        false,
        fee_features,
    )
}

pub fn calculate_fee_with_flags(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    is_simple_vote_transaction: bool,
    fee_features: FeeFeatures,
) -> u64 {
    calculate_fee_details_with_flags(
        message,
        lamports_per_signature,
        prioritization_fee,
        is_simple_vote_transaction,
        fee_features,
    )
    .total_fee()
}

pub fn calculate_fee_details_with_flags(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    is_simple_vote_transaction: bool,
    fee_features: FeeFeatures,
) -> FeeDetails {
    FeeDetails::new(
        calculate_signature_fee(
            SignatureCounts::from(message),
            effective_lamports_per_signature(
                lamports_per_signature,
                is_simple_vote_transaction,
                fee_features,
            ),
        ),
        prioritization_fee,
    )
}

fn effective_lamports_per_signature(
    lamports_per_signature: u64,
    is_simple_vote_transaction: bool,
    fee_features: FeeFeatures,
) -> u64 {
    if fee_features.halve_simple_vote_lamports_per_signature && is_simple_vote_transaction {
        lamports_per_signature / 2
    } else {
        lamports_per_signature
    }
}

/// Calculate fees from signatures.
pub fn calculate_signature_fee(
    SignatureCounts {
        num_transaction_signatures,
        num_ed25519_signatures,
        num_secp256k1_signatures,
        num_secp256r1_signatures,
    }: SignatureCounts,
    lamports_per_signature: u64,
) -> u64 {
    let signature_count = num_transaction_signatures
        .saturating_add(num_ed25519_signatures)
        .saturating_add(num_secp256k1_signatures)
        .saturating_add(num_secp256r1_signatures);
    signature_count.saturating_mul(lamports_per_signature)
}

pub struct SignatureCounts {
    pub num_transaction_signatures: u64,
    pub num_ed25519_signatures: u64,
    pub num_secp256k1_signatures: u64,
    pub num_secp256r1_signatures: u64,
}

impl<Tx: SVMStaticMessage> From<&Tx> for SignatureCounts {
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
            ),
            10 * LAMPORTS_PER_SIGNATURE
        );
    }

    #[test]
    fn test_effective_lamports_per_signature_halves_simple_vote_fee() {
        let fee_features = FeeFeatures {
            halve_simple_vote_lamports_per_signature: true,
        };
        assert_eq!(
            effective_lamports_per_signature(5_000, true, fee_features),
            2_500
        );
        assert_eq!(
            effective_lamports_per_signature(5_000, false, fee_features),
            5_000
        );
        assert_eq!(
            effective_lamports_per_signature(5_000, true, FeeFeatures::default()),
            5_000
        );
    }
}
