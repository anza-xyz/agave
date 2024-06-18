use {
    crate::{
        encryption::{
            FeeCiphertext, PodFeeCiphertext, PodTransferAmountCiphertext, TransferAmountCiphertext,
        },
        errors::ProofDataError,
        try_combine_lo_hi_ciphertexts, try_combine_lo_hi_commitments, try_combine_lo_hi_openings,
        try_split_u64,
    },
    curve25519_dalek::scalar::Scalar,
    solana_zk_sdk::{
        elgamal_program::proof_data::{
            BatchedGroupedCiphertext2HandlesValidityProofContext,
            BatchedGroupedCiphertext2HandlesValidityProofData,
            BatchedGroupedCiphertext3HandlesValidityProofContext,
            BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofContext,
            BatchedRangeProofU256Data, CiphertextCommitmentEqualityProofContext,
            CiphertextCommitmentEqualityProofData, PercentageWithCapProofContext,
            PercentageWithCapProofData,
        },
        encryption::{
            auth_encryption::{AeCiphertext, AeKey},
            elgamal::{ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey},
            grouped_elgamal::GroupedElGamal,
            pedersen::{Pedersen, PedersenCommitment, PedersenOpening},
            pod::{
                elgamal::{PodElGamalCiphertext, PodElGamalPubkey},
                pedersen::PodPedersenCommitment,
            },
        },
    },
    solana_zk_token_sdk::curve25519::{
        ristretto::{self, PodRistrettoPoint},
        scalar::PodScalar,
    },
};

/// The transfer public keys associated with a transfer with fee.
pub struct TransferWithFeePubkeys {
    /// Source ElGamal public key
    pub source: PodElGamalPubkey,
    /// Destination ElGamal public key
    pub destination: PodElGamalPubkey,
    /// Auditor ElGamal public key
    pub auditor: PodElGamalPubkey,
    /// Withdraw withheld authority public key
    pub withdraw_withheld_authority: PodElGamalPubkey,
}

/// The proof context information needed to process a [Transfer] instruction
/// with fee.
pub struct TransferWithFeeProofContext {
    /// Group encryption of the low 16 bits of the transfer amount
    pub ciphertext_lo: PodTransferAmountCiphertext,
    /// Group encryption of the high 48 bits of the transfer amount
    pub ciphertext_hi: PodTransferAmountCiphertext,
    /// The public encryption keys associated with the transfer: source, dest,
    /// auditor, and withdraw withheld authority
    pub transfer_with_fee_pubkeys: TransferWithFeePubkeys,
    /// The final spendable ciphertext after the transfer,
    pub new_source_ciphertext: PodElGamalCiphertext,
    /// The transfer fee encryption of the low 16 bits of the transfer fee
    /// amount
    pub fee_ciphertext_lo: PodFeeCiphertext,
    /// The transfer fee encryption of the hi 32 bits of the transfer fee amount
    pub fee_ciphertext_hi: PodFeeCiphertext,
}

const MAX_FEE_BASIS_POINTS: u64 = 10_000;
const ONE_IN_BASIS_POINTS: u128 = MAX_FEE_BASIS_POINTS as u128;

const TRANSFER_AMOUNT_LO_BITS: usize = 16;
const TRANSFER_AMOUNT_HI_BITS: usize = 32;
const REMAINING_BALANCE_BIT_LENGTH: usize = 64;
const RANGE_PROOF_PADDING_BIT_LENGTH: usize = 16;
const TRANSFER_DELTA_BITS: usize = 16;
const FEE_AMOUNT_LO_BITS: usize = 16;
const FEE_AMOUNT_HI_BITS: usize = 32;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct FeeParameters {
    /// Fee rate expressed as basis points of the transfer amount, i.e. increments of 0.01%
    pub fee_rate_basis_points: u16,
    /// Maximum fee assessed on transfers, expressed as an amount of tokens
    pub maximum_fee: u64,
}

pub fn transfer_with_fee_split_proof_data(
    current_available_balance: &ElGamalCiphertext,
    current_decryptable_available_balance: &AeCiphertext,
    transfer_amount: u64,
    source_elgamal_keypair: &ElGamalKeypair,
    aes_key: &AeKey,
    destination_elgamal_pubkey: &ElGamalPubkey,
    auditor_elgamal_pubkey: Option<&ElGamalPubkey>,
    withdraw_withheld_authority_elgamal_pubkey: &ElGamalPubkey,
    transfer_fee_parameters: &FeeParameters,
) -> Result<
    (
        CiphertextCommitmentEqualityProofData,
        BatchedGroupedCiphertext3HandlesValidityProofData,
        PercentageWithCapProofData,
        BatchedGroupedCiphertext2HandlesValidityProofData,
        BatchedRangeProofU256Data,
    ),
    ProofDataError,
> {
    let default_auditor_pubkey = ElGamalPubkey::default();
    let auditor_elgamal_pubkey = auditor_elgamal_pubkey.unwrap_or(&default_auditor_pubkey);

    // Split the transfer amount into the low and high bit components
    let (transfer_amount_lo, transfer_amount_hi) =
        try_split_u64(transfer_amount, TRANSFER_AMOUNT_LO_BITS)
            .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    // Encrypt the `lo` and `hi` transfer amounts
    let (transfer_amount_grouped_ciphertext_lo, transfer_amount_opening_lo) =
        TransferAmountCiphertext::new(
            transfer_amount_lo,
            source_elgamal_keypair.pubkey(),
            destination_elgamal_pubkey,
            auditor_elgamal_pubkey,
        );

    let (transfer_amount_grouped_ciphertext_hi, transfer_amount_opening_hi) =
        TransferAmountCiphertext::new(
            transfer_amount_hi,
            source_elgamal_keypair.pubkey(),
            destination_elgamal_pubkey,
            auditor_elgamal_pubkey,
        );

    // Decrypt the current available balance at the source
    let current_decrypted_available_balance = current_decryptable_available_balance
        .decrypt(aes_key)
        .ok_or(ProofDataError::Decryption)?;

    // Compute the remaining balance at the source
    let new_decrypted_available_balance = current_decrypted_available_balance
        .checked_sub(transfer_amount)
        .ok_or(ProofDataError::NotEnoughFunds)?;

    // Create a new Pedersen commitment for the remaining balance at the source
    let (new_available_balance_commitment, new_source_opening) =
        Pedersen::new(new_decrypted_available_balance);

    // Compute the remaining balance at the source as ElGamal ciphertexts
    let transfer_amount_source_ciphertext_lo =
        GroupedElGamal::to_elgamal_ciphertext(&transfer_amount_grouped_ciphertext_lo.0, 0).unwrap();
    let transfer_amount_source_ciphertext_hi =
        GroupedElGamal::to_elgamal_ciphertext(&transfer_amount_grouped_ciphertext_hi.0, 0).unwrap();

    let new_available_balance_ciphertext = current_available_balance
        - try_combine_lo_hi_ciphertexts(
            &transfer_amount_source_ciphertext_lo,
            &transfer_amount_source_ciphertext_hi,
            TRANSFER_AMOUNT_LO_BITS,
        )
        .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    // generate equality proof data
    let equality_proof_data = CiphertextCommitmentEqualityProofData::new(
        source_elgamal_keypair,
        &new_available_balance_ciphertext,
        &new_available_balance_commitment,
        &new_source_opening,
        new_decrypted_available_balance,
    )
    .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    // generate ciphertext validity data
    let transfer_amount_ciphertext_validity_proof_data =
        BatchedGroupedCiphertext3HandlesValidityProofData::new(
            source_elgamal_keypair.pubkey(),
            destination_elgamal_pubkey,
            auditor_elgamal_pubkey,
            &transfer_amount_grouped_ciphertext_lo.0,
            &transfer_amount_grouped_ciphertext_hi.0,
            transfer_amount_lo,
            transfer_amount_hi,
            &transfer_amount_opening_lo,
            &transfer_amount_opening_hi,
        )
        .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    // calculate fee
    let transfer_fee_basis_points = transfer_fee_parameters.fee_rate_basis_points;
    let transfer_fee_maximum_fee = transfer_fee_parameters.maximum_fee;
    let (raw_fee_amount, delta_fee) = calculate_fee(transfer_amount, transfer_fee_basis_points)
        .ok_or(ProofDataError::FeeCalculation)?;

    // if raw fee is greater than the maximum fee, then use the maximum fee for the fee amount
    let fee_amount = std::cmp::min(transfer_fee_maximum_fee, raw_fee_amount);

    // split and encrypt fee
    let (fee_amount_lo, fee_amount_hi) = try_split_u64(fee_amount, FEE_AMOUNT_LO_BITS)
        .map_err(|_| ProofDataError::IllegalAmountBitLength)?;
    let (fee_ciphertext_lo, fee_opening_lo) = FeeCiphertext::new(
        fee_amount_lo,
        destination_elgamal_pubkey,
        withdraw_withheld_authority_elgamal_pubkey,
    );
    let (fee_ciphertext_hi, fee_opening_hi) = FeeCiphertext::new(
        fee_amount_hi,
        destination_elgamal_pubkey,
        withdraw_withheld_authority_elgamal_pubkey,
    );

    // create combined commitments and openings to be used to generate proofs
    let combined_transfer_amount_commitment = try_combine_lo_hi_commitments(
        transfer_amount_grouped_ciphertext_lo.get_commitment(),
        transfer_amount_grouped_ciphertext_hi.get_commitment(),
        TRANSFER_AMOUNT_LO_BITS,
    )
    .map_err(|_| ProofDataError::IllegalAmountBitLength)?;
    let combined_transfer_amount_opening = try_combine_lo_hi_openings(
        &transfer_amount_opening_lo,
        &transfer_amount_opening_hi,
        TRANSFER_AMOUNT_LO_BITS,
    )
    .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    let combined_fee_commitment = try_combine_lo_hi_commitments(
        fee_ciphertext_lo.get_commitment(),
        fee_ciphertext_hi.get_commitment(),
        FEE_AMOUNT_LO_BITS,
    )
    .map_err(|_| ProofDataError::IllegalAmountBitLength)?;
    let combined_fee_opening =
        try_combine_lo_hi_openings(&fee_opening_lo, &fee_opening_hi, FEE_AMOUNT_LO_BITS)
            .map_err(|_| ProofDataError::IllegalAmountBitLength)?;

    // compute claimed and real delta commitment
    let (claimed_commitment, claimed_opening) = Pedersen::new(delta_fee);
    let (delta_commitment, delta_opening) = compute_delta_commitment_and_opening(
        (
            &combined_transfer_amount_commitment,
            &combined_transfer_amount_opening,
        ),
        (&combined_fee_commitment, &combined_fee_opening),
        transfer_fee_basis_points,
    );

    // generate fee sigma proof
    let percentage_with_cap_proof_data = PercentageWithCapProofData::new(
        &combined_fee_commitment,
        &combined_fee_opening,
        fee_amount,
        &delta_commitment,
        &delta_opening,
        delta_fee,
        &claimed_commitment,
        &claimed_opening,
        transfer_fee_maximum_fee,
    )
    .map_err(|_| ProofDataError::ProofGeneration)?;

    // encrypt the fee amount under the destination and withdraw withheld authority
    // ElGamal public key
    let fee_destination_withdraw_withheld_authority_ciphertext_lo = GroupedElGamal::encrypt_with(
        [
            destination_elgamal_pubkey,
            withdraw_withheld_authority_elgamal_pubkey,
        ],
        fee_amount_lo,
        &fee_opening_lo,
    );
    let fee_destination_withdraw_withheld_authority_ciphertext_hi = GroupedElGamal::encrypt_with(
        [
            destination_elgamal_pubkey,
            withdraw_withheld_authority_elgamal_pubkey,
        ],
        fee_amount_hi,
        &fee_opening_hi,
    );

    // generate fee ciphertext validity data
    let fee_ciphertext_validity_proof_data =
        BatchedGroupedCiphertext2HandlesValidityProofData::new(
            destination_elgamal_pubkey,
            withdraw_withheld_authority_elgamal_pubkey,
            &fee_destination_withdraw_withheld_authority_ciphertext_lo,
            &fee_destination_withdraw_withheld_authority_ciphertext_hi,
            fee_amount_lo,
            fee_amount_hi,
            &fee_opening_lo,
            &fee_opening_hi,
        )
        .map_err(|_| ProofDataError::ProofGeneration)?;

    // generate range proof data
    const REMAINING_BALANCE_BIT_LENGTH: usize = 64;
    const DELTA_BIT_LENGTH: usize = 48;
    const MAX_FEE_BASIS_POINTS: u64 = 10_000;

    let delta_fee_complement = MAX_FEE_BASIS_POINTS - delta_fee;

    let max_fee_basis_points_commitment =
        Pedersen::with(MAX_FEE_BASIS_POINTS, &PedersenOpening::default());
    let claimed_complement_commitment = max_fee_basis_points_commitment - claimed_commitment;
    let claimed_complement_opening = PedersenOpening::default() - &claimed_opening;

    let range_proof_data = BatchedRangeProofU256Data::new(
        vec![
            &new_available_balance_commitment,
            transfer_amount_grouped_ciphertext_lo.get_commitment(),
            transfer_amount_grouped_ciphertext_hi.get_commitment(),
            &claimed_commitment,
            &claimed_complement_commitment,
            fee_ciphertext_lo.get_commitment(),
            fee_ciphertext_hi.get_commitment(),
        ],
        vec![
            new_decrypted_available_balance,
            transfer_amount_lo,
            transfer_amount_hi,
            delta_fee,
            delta_fee_complement,
            fee_amount_lo,
            fee_amount_hi,
        ],
        vec![
            REMAINING_BALANCE_BIT_LENGTH,
            TRANSFER_AMOUNT_LO_BITS,
            TRANSFER_AMOUNT_HI_BITS,
            DELTA_BIT_LENGTH,
            DELTA_BIT_LENGTH,
            FEE_AMOUNT_LO_BITS,
            FEE_AMOUNT_HI_BITS,
        ],
        vec![
            &new_source_opening,
            &transfer_amount_opening_lo,
            &transfer_amount_opening_hi,
            &claimed_opening,
            &claimed_complement_opening,
            &fee_opening_lo,
            &fee_opening_hi,
        ],
    )
    .map_err(|_| ProofDataError::ProofGeneration)?;

    Ok((
        equality_proof_data,
        transfer_amount_ciphertext_validity_proof_data,
        percentage_with_cap_proof_data,
        fee_ciphertext_validity_proof_data,
        range_proof_data,
    ))
}

impl TransferWithFeeProofContext {
    pub fn verify_and_extract(
        equality_proof_context: &CiphertextCommitmentEqualityProofContext,
        transfer_amount_ciphertext_validity_proof_context: &BatchedGroupedCiphertext3HandlesValidityProofContext,
        fee_sigma_proof_context: &PercentageWithCapProofContext,
        fee_ciphertext_validity_proof_context: &BatchedGroupedCiphertext2HandlesValidityProofContext,
        range_proof_context: &BatchedRangeProofContext,
        fee_parameters: &FeeParameters,
    ) -> Result<Self, ProofDataError> {
        // The equality proof context consists of the source ElGamal public key, the new
        // source available balance ciphertext, and the new source available
        // commitment. The public key and ciphertext should be returned as part
        // of `TransferWithFeeProofContextInfo` and the commitment should be
        // checked with range proof for consistency.
        let CiphertextCommitmentEqualityProofContext {
            pubkey: source_pubkey_from_equality_proof,
            ciphertext: new_source_ciphertext,
            commitment: new_source_commitment,
        } = equality_proof_context;

        // The transfer amount ciphertext validity proof context consists of the
        // destination ElGamal public key, auditor ElGamal public key, and the
        // transfer amount ciphertexts. All of these fields should be returned
        // as part of `TransferWithFeeProofContextInfo`. In addition, the
        // commitments pertaining to the transfer amount ciphertexts should be
        // checked with range proof for consistency.
        let BatchedGroupedCiphertext3HandlesValidityProofContext {
            first_pubkey: source_pubkey_from_validity_proof,
            second_pubkey: destination_pubkey,
            third_pubkey: auditor_pubkey,
            grouped_ciphertext_lo: transfer_amount_ciphertext_lo,
            grouped_ciphertext_hi: transfer_amount_ciphertext_hi,
        } = transfer_amount_ciphertext_validity_proof_context;

        // The fee sigma proof context consists of the fee commitment, delta commitment,
        // claimed commitment, and max fee. The fee and claimed commitment
        // should be checked with range proof for consistency. The delta
        // commitment should be checked whether it is properly generated with
        // respect to the fee parameters. The max fee should be checked for
        // consistency with the fee parameters.
        let PercentageWithCapProofContext {
            percentage_commitment: fee_commitment,
            delta_commitment,
            claimed_commitment,
            max_value: max_fee,
        } = fee_sigma_proof_context;

        let expected_maximum_fee: u64 = fee_parameters.maximum_fee.into();
        let proof_maximum_fee: u64 = (*max_fee).into();
        if expected_maximum_fee != proof_maximum_fee {
            return Err(ProofDataError::ProofVerification);
        }

        // The transfer fee ciphertext validity proof context consists of the
        // destination ElGamal public key, withdraw withheld authority ElGamal
        // public key, and the transfer fee ciphertexts. The rest of the fields
        // should be return as part of `TransferWithFeeProofContextInfo`. In
        // addition, the destination public key should be checked for
        // consistency with the destination public key contained in the transfer amount
        // ciphertext validity proof, and the commitments pertaining to the transfer fee
        // amount ciphertexts should be checked with range proof for
        // consistency.
        let BatchedGroupedCiphertext2HandlesValidityProofContext {
            first_pubkey: destination_pubkey_from_transfer_fee_validity_proof,
            second_pubkey: withdraw_withheld_authority_pubkey,
            grouped_ciphertext_lo: fee_ciphertext_lo,
            grouped_ciphertext_hi: fee_ciphertext_hi,
        } = fee_ciphertext_validity_proof_context;

        if destination_pubkey != destination_pubkey_from_transfer_fee_validity_proof {
            return Err(ProofDataError::ProofVerification);
        }

        // The range proof context consists of the Pedersen commitments and bit-lengths
        // for which the range proof is proved. The commitments must consist of
        // seven commitments pertaining to
        // - the new source available balance (64 bits)
        // - the low bits of the transfer amount (16 bits)
        // - the high bits of the transfer amount (32 bits)
        // - the delta amount for the fee (48 bits)
        // - the complement of the delta amount for the fee (48 bits)
        // - the low bits of the fee amount (16 bits)
        // - the high bits of the fee amount (32 bits)
        let BatchedRangeProofContext {
            commitments: range_proof_commitments,
            bit_lengths: range_proof_bit_lengths,
        } = range_proof_context;

        // check that the range proof was created for the correct set of Pedersen
        // commitments
        let transfer_amount_commitment_lo = transfer_amount_ciphertext_lo.extract_commitment();
        let transfer_amount_commitment_hi = transfer_amount_ciphertext_hi.extract_commitment();

        let fee_commitment_lo = fee_ciphertext_lo.extract_commitment();
        let fee_commitment_hi = fee_ciphertext_hi.extract_commitment();

        const MAX_FEE_BASIS_POINTS: u64 = 10_000;
        let max_fee_basis_points_scalar = u64_to_scalar(MAX_FEE_BASIS_POINTS);
        let max_fee_basis_points_commitment =
            ristretto::multiply_ristretto(&max_fee_basis_points_scalar, &G)
                .ok_or(ProofDataError::ProofVerification)?;
        let claimed_complement_commitment = ristretto::subtract_ristretto(
            &max_fee_basis_points_commitment,
            &(*claimed_commitment).into(),
        )
        .ok_or(ProofDataError::ProofVerification)?;

        let expected_commitments = [
            *new_source_commitment,
            transfer_amount_commitment_lo,
            transfer_amount_commitment_hi,
            *claimed_commitment,
            claimed_complement_commitment.into(),
            fee_commitment_lo,
            fee_commitment_hi,
        ];

        if !range_proof_commitments
            .iter()
            .zip(expected_commitments.iter())
            .all(|(proof_commitment, expected_commitment)| proof_commitment == expected_commitment)
        {
            return Err(ProofDataError::ProofVerification);
        }

        // check that the range proof was created for the correct number of bits
        const REMAINING_BALANCE_BIT_LENGTH: u8 = 64;
        const TRANSFER_AMOUNT_LO_BIT_LENGTH: u8 = 16;
        const TRANSFER_AMOUNT_HI_BIT_LENGTH: u8 = 32;
        const DELTA_BIT_LENGTH: u8 = 48;
        const FEE_AMOUNT_LO_BIT_LENGTH: u8 = 16;
        const FEE_AMOUNT_HI_BIT_LENGTH: u8 = 32;

        let expected_bit_lengths = [
            REMAINING_BALANCE_BIT_LENGTH,
            TRANSFER_AMOUNT_LO_BIT_LENGTH,
            TRANSFER_AMOUNT_HI_BIT_LENGTH,
            DELTA_BIT_LENGTH,
            DELTA_BIT_LENGTH,
            FEE_AMOUNT_LO_BIT_LENGTH,
            FEE_AMOUNT_HI_BIT_LENGTH,
        ]
        .iter();

        if !range_proof_bit_lengths
            .iter()
            .zip(expected_bit_lengths)
            .all(|(proof_len, expected_len)| proof_len == expected_len)
        {
            return Err(ProofDataError::ProofVerification);
        }

        // check consistency between fee sigma and fee ciphertext validity proofs

        let sigma_proof_fee_commitment_point: PodRistrettoPoint = (*fee_commitment).into();
        let validity_proof_fee_point =
            combine_lo_hi_pedersen_points(&fee_commitment_lo.into(), &fee_commitment_hi.into())
                .ok_or(ProofDataError::ProofVerification)?;
        if validity_proof_fee_point != sigma_proof_fee_commitment_point {
            return Err(ProofDataError::ProofVerification);
        }

        verify_delta_commitment(
            &transfer_amount_commitment_lo,
            &transfer_amount_commitment_hi,
            fee_commitment,
            delta_commitment,
            fee_parameters.fee_rate_basis_points,
        )?;

        // create transfer with fee proof context info and return
        let transfer_with_fee_pubkeys = TransferWithFeePubkeys {
            source: *source_pubkey_from_equality_proof,
            destination: *destination_pubkey,
            auditor: *auditor_pubkey,
            withdraw_withheld_authority: *withdraw_withheld_authority_pubkey,
        };

        Ok(Self {
            ciphertext_lo: PodTransferAmountCiphertext(*transfer_amount_ciphertext_lo),
            ciphertext_hi: PodTransferAmountCiphertext(*transfer_amount_ciphertext_hi),
            transfer_with_fee_pubkeys,
            new_source_ciphertext: *new_source_ciphertext,
            fee_ciphertext_lo: PodFeeCiphertext(*fee_ciphertext_lo),
            fee_ciphertext_hi: PodFeeCiphertext(*fee_ciphertext_hi),
        })
    }
}

/// Ristretto generator point for curve25519
const G: ristretto::PodRistrettoPoint = ristretto::PodRistrettoPoint([
    226, 242, 174, 10, 106, 188, 78, 113, 168, 132, 169, 97, 197, 0, 81, 95, 88, 227, 11, 106, 165,
    130, 221, 141, 182, 166, 89, 69, 224, 141, 45, 118,
]);

/// Convert a `u64` amount into a curve25519 scalar
fn u64_to_scalar(amount: u64) -> PodScalar {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&amount.to_le_bytes());
    PodScalar(bytes)
}

/// Convert a `u16` amount into a curve25519 scalar
fn u16_to_scalar(amount: u16) -> PodScalar {
    let mut bytes = [0u8; 32];
    bytes[..2].copy_from_slice(&amount.to_le_bytes());
    PodScalar(bytes)
}

#[cfg(not(target_os = "solana"))]
fn calculate_fee(transfer_amount: u64, fee_rate_basis_points: u16) -> Option<(u64, u64)> {
    let numerator = (transfer_amount as u128).checked_mul(fee_rate_basis_points as u128)?;

    // Warning: Division may involve CPU opcodes that have variable execution times. This
    // non-constant-time execution of the fee calculation can theoretically reveal information
    // about the transfer amount. For transfers that invole extremely sensitive data, additional
    // care should be put into how the fees are calculated.
    let fee = numerator
        .checked_add(ONE_IN_BASIS_POINTS)?
        .checked_sub(1)?
        .checked_div(ONE_IN_BASIS_POINTS)?;

    let delta_fee = fee
        .checked_mul(ONE_IN_BASIS_POINTS)?
        .checked_sub(numerator)?;

    Some((fee as u64, delta_fee as u64))
}

#[cfg(not(target_os = "solana"))]
fn compute_delta_commitment_and_opening(
    (combined_commitment, combined_opening): (&PedersenCommitment, &PedersenOpening),
    (combined_fee_commitment, combined_fee_opening): (&PedersenCommitment, &PedersenOpening),
    fee_rate_basis_points: u16,
) -> (PedersenCommitment, PedersenOpening) {
    let fee_rate_scalar = Scalar::from(fee_rate_basis_points);
    let delta_commitment = combined_fee_commitment * Scalar::from(MAX_FEE_BASIS_POINTS)
        - combined_commitment * fee_rate_scalar;
    let delta_opening = combined_fee_opening * Scalar::from(MAX_FEE_BASIS_POINTS)
        - combined_opening * fee_rate_scalar;

    (delta_commitment, delta_opening)
}

#[cfg(not(target_os = "solana"))]
fn compute_delta_commitment(
    combined_commitment: &PedersenCommitment,
    combined_fee_commitment: &PedersenCommitment,
    fee_rate_basis_points: u16,
) -> PedersenCommitment {
    let fee_rate_scalar = Scalar::from(fee_rate_basis_points);
    combined_fee_commitment * Scalar::from(MAX_FEE_BASIS_POINTS)
        - combined_commitment * fee_rate_scalar
}

/// Combine lo and hi Pedersen commitment points
fn combine_lo_hi_pedersen_points(
    point_lo: &PodRistrettoPoint,
    point_hi: &PodRistrettoPoint,
) -> Option<PodRistrettoPoint> {
    const SCALING_CONSTANT: u64 = 65536;
    let scaling_constant_scalar = u64_to_scalar(SCALING_CONSTANT);
    let scaled_point_hi = ristretto::multiply_ristretto(&scaling_constant_scalar, point_hi)?;
    ristretto::add_ristretto(point_lo, &scaled_point_hi)
}

/// Compute fee delta commitment
fn verify_delta_commitment(
    transfer_amount_commitment_lo: &PodPedersenCommitment,
    transfer_amount_commitment_hi: &PodPedersenCommitment,
    fee_commitment: &PodPedersenCommitment,
    proof_delta_commitment: &PodPedersenCommitment,
    transfer_fee_basis_points: u16,
) -> Result<(), ProofDataError> {
    let transfer_amount_point = combine_lo_hi_pedersen_points(
        &(*transfer_amount_commitment_lo).into(),
        &(*transfer_amount_commitment_hi).into(),
    )
    .ok_or(ProofDataError::ProofVerification)?;
    let transfer_fee_basis_points_scalar = u16_to_scalar(transfer_fee_basis_points);
    let scaled_transfer_amount_point =
        ristretto::multiply_ristretto(&transfer_fee_basis_points_scalar, &transfer_amount_point)
            .ok_or(ProofDataError::ProofVerification)?;

    const MAX_FEE_BASIS_POINTS: u64 = 10_000;
    let max_fee_basis_points_scalar = u64_to_scalar(MAX_FEE_BASIS_POINTS);
    let fee_point: PodRistrettoPoint = (*fee_commitment).into();
    let scaled_fee_point = ristretto::multiply_ristretto(&max_fee_basis_points_scalar, &fee_point)
        .ok_or(ProofDataError::ProofVerification)?;

    let expected_delta_commitment_point =
        ristretto::subtract_ristretto(&scaled_fee_point, &scaled_transfer_amount_point)
            .ok_or(ProofDataError::ProofVerification)?;

    let proof_delta_commitment_point = (*proof_delta_commitment).into();
    if expected_delta_commitment_point != proof_delta_commitment_point {
        return Err(ProofDataError::ProofVerification);
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use solana_zk_sdk::elgamal_program::proof_data::ZkProofData;

    #[test]
    fn test_transfer_with_fee_correctness() {
        let source_keypair = ElGamalKeypair::new_rand();

        let aes_key = AeKey::new_rand();

        let destination_keypair = ElGamalKeypair::new_rand();
        let destination_pubkey = destination_keypair.pubkey();

        let auditor_keypair = ElGamalKeypair::new_rand();
        let auditor_pubkey = auditor_keypair.pubkey();

        let withdraw_withheld_authority_keypair = ElGamalKeypair::new_rand();
        let withdraw_withheld_authority_pubkey = withdraw_withheld_authority_keypair.pubkey();

        // Case 1: transfer 0 amount
        let spendable_balance: u64 = 120;
        let spendable_ciphertext = source_keypair.pubkey().encrypt(spendable_balance);

        let decryptable_balance = aes_key.encrypt(spendable_balance);

        let transfer_amount: u64 = 0;

        let fee_parameters = FeeParameters {
            fee_rate_basis_points: 400,
            maximum_fee: 3,
        };

        let (
            equality_proof_data,
            transfer_amount_ciphertext_validity_proof_data,
            percentage_with_cap_proof_data,
            fee_ciphertext_validity_proof_data,
            range_proof_data,
        ) = transfer_with_fee_split_proof_data(
            &spendable_ciphertext,
            &decryptable_balance,
            transfer_amount,
            &source_keypair,
            &aes_key,
            destination_pubkey,
            Some(auditor_pubkey),
            withdraw_withheld_authority_pubkey,
            &fee_parameters,
        )
        .unwrap();

        equality_proof_data.verify_proof().unwrap();
        transfer_amount_ciphertext_validity_proof_data
            .verify_proof()
            .unwrap();
        percentage_with_cap_proof_data.verify_proof().unwrap();
        fee_ciphertext_validity_proof_data.verify_proof().unwrap();
        range_proof_data.verify_proof().unwrap();

        TransferWithFeeProofContext::verify_and_extract(
            equality_proof_data.context_data(),
            transfer_amount_ciphertext_validity_proof_data.context_data(),
            percentage_with_cap_proof_data.context_data(),
            fee_ciphertext_validity_proof_data.context_data(),
            range_proof_data.context_data(),
            &fee_parameters,
        )
        .unwrap();

        // Case 2: transfer max amount
        let spendable_balance: u64 = u64::MAX;
        let spendable_ciphertext = source_keypair.pubkey().encrypt(spendable_balance);

        let decryptable_balance = aes_key.encrypt(spendable_balance);

        let transfer_amount: u64 =
            (1u64 << (TRANSFER_AMOUNT_LO_BITS + TRANSFER_AMOUNT_HI_BITS)) - 1;

        let fee_parameters = FeeParameters {
            fee_rate_basis_points: 400,
            maximum_fee: 3,
        };

        let (
            equality_proof_data,
            transfer_amount_ciphertext_validity_proof_data,
            percentage_with_cap_proof_data,
            fee_ciphertext_validity_proof_data,
            range_proof_data,
        ) = transfer_with_fee_split_proof_data(
            &spendable_ciphertext,
            &decryptable_balance,
            transfer_amount,
            &source_keypair,
            &aes_key,
            destination_pubkey,
            Some(auditor_pubkey),
            withdraw_withheld_authority_pubkey,
            &fee_parameters,
        )
        .unwrap();

        equality_proof_data.verify_proof().unwrap();
        transfer_amount_ciphertext_validity_proof_data
            .verify_proof()
            .unwrap();
        percentage_with_cap_proof_data.verify_proof().unwrap();
        fee_ciphertext_validity_proof_data.verify_proof().unwrap();
        range_proof_data.verify_proof().unwrap();

        TransferWithFeeProofContext::verify_and_extract(
            equality_proof_data.context_data(),
            transfer_amount_ciphertext_validity_proof_data.context_data(),
            percentage_with_cap_proof_data.context_data(),
            fee_ciphertext_validity_proof_data.context_data(),
            range_proof_data.context_data(),
            &fee_parameters,
        )
        .unwrap();

        // Case 3: general success case
        let spendable_balance: u64 = 120;
        let spendable_ciphertext = source_keypair.pubkey().encrypt(spendable_balance);

        let decryptable_balance = aes_key.encrypt(spendable_balance);

        let transfer_amount: u64 = 100;

        let fee_parameters = FeeParameters {
            fee_rate_basis_points: 400,
            maximum_fee: 3,
        };

        let (
            equality_proof_data,
            transfer_amount_ciphertext_validity_proof_data,
            percentage_with_cap_proof_data,
            fee_ciphertext_validity_proof_data,
            range_proof_data,
        ) = transfer_with_fee_split_proof_data(
            &spendable_ciphertext,
            &decryptable_balance,
            transfer_amount,
            &source_keypair,
            &aes_key,
            destination_pubkey,
            Some(auditor_pubkey),
            withdraw_withheld_authority_pubkey,
            &fee_parameters,
        )
        .unwrap();

        equality_proof_data.verify_proof().unwrap();
        transfer_amount_ciphertext_validity_proof_data
            .verify_proof()
            .unwrap();
        percentage_with_cap_proof_data.verify_proof().unwrap();
        fee_ciphertext_validity_proof_data.verify_proof().unwrap();
        range_proof_data.verify_proof().unwrap();

        TransferWithFeeProofContext::verify_and_extract(
            equality_proof_data.context_data(),
            transfer_amount_ciphertext_validity_proof_data.context_data(),
            percentage_with_cap_proof_data.context_data(),
            fee_ciphertext_validity_proof_data.context_data(),
            range_proof_data.context_data(),
            &fee_parameters,
        )
        .unwrap();
    }
}
