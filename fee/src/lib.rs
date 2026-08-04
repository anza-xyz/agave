#![cfg(feature = "agave-unstable-api")]
use {solana_fee_structure::FeeDetails, solana_svm_transaction::svm_message::SVMStaticMessage};

/// Flat base inclusion fee paid entirely to the leader (SIMD-0553).
pub const BASE_INCLUSION_FEE: u64 = 2500;

/// Bools indicating the activation of features relevant
/// to the fee calculation.
// DEVELOPER NOTE:
// This struct may become empty at some point. It is preferable to keep it
// instead of removing, since fees will naturally be changed via feature-gates
// in the future. Keeping this struct will help keep things organized.
#[derive(Debug, Copy, Clone)]
pub struct FeeFeatures {
    pub resource_fee_burn_1_10: bool,
    pub resource_fee_burn_1_4: bool,
    pub resource_fee_burn_1_2: bool,
}

impl FeeFeatures {
    /// Returns whether any SIMD-0553 resource-fee gate is active.
    pub fn is_resource_fee_active(&self) -> bool {
        self.resource_fee_denominator().is_some()
    }

    /// Effective resource-fee denominator when a gate is active.
    ///
    /// When multiple gates are active, the highest rate wins (1/2 > 1/4 > 1/10).
    /// When no gate is active (pre-activation), returns None.
    fn resource_fee_denominator(&self) -> Option<u64> {
        if self.resource_fee_burn_1_2 {
            Some(2)
        } else if self.resource_fee_burn_1_4 {
            Some(4)
        } else if self.resource_fee_burn_1_10 {
            Some(10)
        } else {
            None
        }
    }
}

/// Calculate fee for `SanitizedMessage`
pub fn calculate_fee(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    requested_cost_units: u64,
    fee_features: FeeFeatures,
) -> u64 {
    calculate_fee_details(
        message,
        lamports_per_signature,
        prioritization_fee,
        requested_cost_units,
        fee_features,
    )
    .total_fee()
}

pub fn calculate_fee_details(
    message: &impl SVMStaticMessage,
    lamports_per_signature: u64,
    prioritization_fee: u64,
    requested_cost_units: u64,
    fee_features: FeeFeatures,
) -> FeeDetails {
    if let Some(denominator) = fee_features.resource_fee_denominator() {
        let resource_fee = calculate_resource_fee(requested_cost_units, denominator);
        FeeDetails::new_with_resource_fee(BASE_INCLUSION_FEE, prioritization_fee, resource_fee)
    } else {
        FeeDetails::new(
            calculate_signature_fee(SignatureCounts::from(message), lamports_per_signature),
            prioritization_fee,
        )
    }
}

/// `ceil_div(requested_cost_units, denominator)` (SIMD-0553; numerator is always 1).
pub fn calculate_resource_fee(requested_cost_units: u64, denominator: u64) -> u64 {
    debug_assert!(denominator > 0);
    requested_cost_units.div_ceil(denominator)
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
    fn test_calculate_resource_fee_ceil_div() {
        // Exact division
        // 10 /  2 = 5
        // 10 / 10 = 1
        assert_eq!(calculate_resource_fee(10, 2), 5);
        assert_eq!(calculate_resource_fee(10, 10), 1);

        // Ceiling
        // 1  / 2  → 1
        // 3  / 2  → 2
        // 1  / 10 → 1
        // 11 / 10 → 2
        assert_eq!(calculate_resource_fee(1, 2), 1);
        assert_eq!(calculate_resource_fee(3, 2), 2);
        assert_eq!(calculate_resource_fee(1, 10), 1);
        assert_eq!(calculate_resource_fee(11, 10), 2);
    }

    #[test]
    fn test_resource_fee_rate_highest_wins() {
        let mut fee_features = FeeFeatures {
            resource_fee_burn_1_10: false,
            resource_fee_burn_1_4: false,
            resource_fee_burn_1_2: false,
        };
        // no resource fee on by default
        assert_eq!(fee_features.resource_fee_denominator(), None);

        // gate priority is correct
        // note these do NOT turn off the prev gates!
        fee_features.resource_fee_burn_1_10 = true;
        assert_eq!(fee_features.resource_fee_denominator(), Some(10));
        fee_features.resource_fee_burn_1_4 = true;
        assert_eq!(fee_features.resource_fee_denominator(), Some(4));
        fee_features.resource_fee_burn_1_2 = true;
        assert_eq!(fee_features.resource_fee_denominator(), Some(2));
    }

    #[test]
    fn test_resource_fee_total_components() {
        // vote example from SIMD-0553 at rate 1/2: 3765 requested cost units.
        let requested_cost_units = 3765;
        let resource_fee = calculate_resource_fee(requested_cost_units, 2);
        assert_eq!(resource_fee, 1883);
        assert_eq!(BASE_INCLUSION_FEE.saturating_add(resource_fee), 2500 + 1883);
    }
}
