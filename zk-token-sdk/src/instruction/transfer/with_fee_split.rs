use crate::{
    encryption::{
        auth_encryption::{AeCiphertext, AeKey},
        elgamal::{DecryptHandle, ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey},
        grouped_elgamal::GroupedElGamal,
        pedersen::{Pedersen, PedersenOpening},
    },
    errors::ProofGenerationError,
    instruction::{
        transfer::{
            try_combine_lo_hi_ciphertexts, try_combine_lo_hi_commitments,
            try_combine_lo_hi_openings, try_split_u64,
            with_fee::{
                calculate_fee, compute_delta_commitment_and_opening, FEE_AMOUNT_HI_BITS,
                FEE_AMOUNT_LO_BITS,
            },
            without_fee::{TRANSFER_AMOUNT_HI_BITS, TRANSFER_AMOUNT_LO_BITS},
            FeeEncryption, FeeParameters, TransferAmountCiphertext,
        },
        BatchedGroupedCiphertext2HandlesValidityProofData, BatchedRangeProofU256Data,
        CiphertextCommitmentEqualityProofData, FeeSigmaProofData,
    },
    zk_token_elgamal::pod::DecryptHandle as PodDecryptHandle,
};

pub struct SourceDecryptHandles {
    pub lo: PodDecryptHandle,
    pub hi: PodDecryptHandle,
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
        BatchedGroupedCiphertext2HandlesValidityProofData,
        FeeSigmaProofData,
        BatchedGroupedCiphertext2HandlesValidityProofData,
        BatchedRangeProofU256Data,
        SourceDecryptHandles,
    ),
    ProofGenerationError,
> {
    let default_auditor_pubkey = ElGamalPubkey::default();
    let auditor_elgamal_pubkey = auditor_elgamal_pubkey.unwrap_or(&default_auditor_pubkey);

    // Split the transfer amount into the low and high bit components.
    let (transfer_amount_lo, transfer_amount_hi) =
        try_split_u64(transfer_amount, TRANSFER_AMOUNT_LO_BITS)
            .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;

    // Encrypt the 'lo' and 'hi' transfer amounts.
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
        .ok_or(ProofGenerationError::Decryption)?;

    // Compute the remaining balance at the source
    let new_decrypted_available_balance = current_decrypted_available_balance
        .checked_sub(transfer_amount)
        .ok_or(ProofGenerationError::NotEnoughFunds)?;

    // Create a new Pedersen commitment for the remaining balance at the source
    let (new_available_balance_commitment, new_source_opening) =
        Pedersen::new(new_decrypted_available_balance);

    // Compute the remaining balance at the source as ElGamal ciphertexts
    let transfer_amount_source_ciphertext_lo =
        GroupedElGamal::to_elgamal_ciphertext(&transfer_amount_grouped_ciphertext_lo.0, 0).unwrap();
    let transfer_amount_source_ciphertext_hi =
        GroupedElGamal::to_elgamal_ciphertext(&transfer_amount_grouped_ciphertext_hi.0, 1).unwrap();

    let new_available_balance_ciphertext = current_available_balance
        - try_combine_lo_hi_ciphertexts(
            &transfer_amount_source_ciphertext_lo,
            &transfer_amount_source_ciphertext_hi,
            TRANSFER_AMOUNT_LO_BITS,
        )
        .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;

    // generate equality proof data
    let equality_proof_data = CiphertextCommitmentEqualityProofData::new(
        source_elgamal_keypair,
        &new_available_balance_ciphertext,
        &new_available_balance_commitment,
        &new_source_opening,
        new_decrypted_available_balance,
    )?;

    // create source decrypt handle
    let source_decrypt_handle_lo =
        DecryptHandle::new(source_elgamal_keypair.pubkey(), &transfer_amount_opening_lo);
    let source_decrypt_handle_hi =
        DecryptHandle::new(source_elgamal_keypair.pubkey(), &transfer_amount_opening_hi);

    let source_decrypt_handles = SourceDecryptHandles {
        lo: source_decrypt_handle_lo.into(),
        hi: source_decrypt_handle_hi.into(),
    };

    // encrypt the transfer amount under the destination and auditor ElGamal public key
    let transfer_amount_destination_auditor_ciphertext_lo = GroupedElGamal::encrypt_with(
        [destination_elgamal_pubkey, auditor_elgamal_pubkey],
        transfer_amount_lo,
        &transfer_amount_opening_lo,
    );
    let transfer_amount_destination_auditor_ciphertext_hi = GroupedElGamal::encrypt_with(
        [destination_elgamal_pubkey, auditor_elgamal_pubkey],
        transfer_amount_hi,
        &transfer_amount_opening_hi,
    );

    // generate transfer amount ciphertext validity data
    let transfer_amount_ciphertext_validity_proof_data =
        BatchedGroupedCiphertext2HandlesValidityProofData::new(
            destination_elgamal_pubkey,
            auditor_elgamal_pubkey,
            &transfer_amount_destination_auditor_ciphertext_lo,
            &transfer_amount_destination_auditor_ciphertext_hi,
            transfer_amount_lo,
            transfer_amount_hi,
            &transfer_amount_opening_lo,
            &transfer_amount_opening_hi,
        )?;

    // calculate fee
    let transfer_fee_basis_points = transfer_fee_parameters.fee_rate_basis_points;
    let transfer_fee_maximum_fee = transfer_fee_parameters.maximum_fee;
    let (raw_fee_amount, delta_fee) = calculate_fee(transfer_amount, transfer_fee_basis_points)
        .ok_or(ProofGenerationError::FeeCalculation)?;

    // if raw fee is greater than the maximum fee, then use the maximum fee for the fee amount
    let fee_amount = std::cmp::min(transfer_fee_maximum_fee, raw_fee_amount);

    // split and encrypt fee
    let (fee_amount_lo, fee_amount_hi) = try_split_u64(fee_amount, FEE_AMOUNT_LO_BITS)
        .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;
    let (fee_ciphertext_lo, fee_opening_lo) = FeeEncryption::new(
        fee_amount_lo,
        destination_elgamal_pubkey,
        withdraw_withheld_authority_elgamal_pubkey,
    );
    let (fee_ciphertext_hi, fee_opening_hi) = FeeEncryption::new(
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
    .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;
    let combined_transfer_amount_opening = try_combine_lo_hi_openings(
        &transfer_amount_opening_lo,
        &transfer_amount_opening_hi,
        TRANSFER_AMOUNT_LO_BITS,
    )
    .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;

    let combined_fee_commitment = try_combine_lo_hi_commitments(
        fee_ciphertext_lo.get_commitment(),
        fee_ciphertext_hi.get_commitment(),
        FEE_AMOUNT_LO_BITS,
    )
    .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;
    let combined_fee_opening =
        try_combine_lo_hi_openings(&fee_opening_lo, &fee_opening_hi, FEE_AMOUNT_LO_BITS)
            .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;

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
    let fee_sigma_proof_data = FeeSigmaProofData::new(
        &combined_fee_commitment,
        &delta_commitment,
        &claimed_commitment,
        &combined_fee_opening,
        &delta_opening,
        &claimed_opening,
        fee_amount,
        delta_fee,
        transfer_fee_maximum_fee,
    )?;

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
        )?;

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
    )?;

    Ok((
        equality_proof_data,
        transfer_amount_ciphertext_validity_proof_data,
        fee_sigma_proof_data,
        fee_ciphertext_validity_proof_data,
        range_proof_data,
        source_decrypt_handles,
    ))
}
