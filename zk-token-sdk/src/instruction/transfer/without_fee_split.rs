#[cfg(not(target_os = "solana"))]
use crate::{
    encryption::{
        auth_encryption::{AeCiphertext, AeKey},
        elgamal::{ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey},
        grouped_elgamal::GroupedElGamal,
        pedersen::Pedersen,
    },
    errors::ProofGenerationError,
    instruction::{
        transfer::{try_combine_lo_hi_ciphertexts, try_split_u64, TransferAmountCiphertext},
        BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
        CiphertextCommitmentEqualityProofData,
    },
};
use crate::{
    errors::ProofVerificationError,
    instruction::transfer::{
        without_fee::{
            TransferPubkeys, RANGE_PROOF_PADDING_BIT_LENGTH, REMAINING_BALANCE_BIT_LENGTH,
            TRANSFER_AMOUNT_HI_BITS, TRANSFER_AMOUNT_LO_BITS,
        },
        TransferProofContext,
    },
    instruction::{
        BatchedGroupedCiphertext3HandlesValidityProofContext, BatchedRangeProofContext,
        CiphertextCommitmentEqualityProofContext,
    },
    zk_token_elgamal::pod,
};

#[cfg(not(target_os = "solana"))]
pub fn transfer_split_proof_data(
    current_available_balance: &ElGamalCiphertext,
    current_decryptable_available_balance: &AeCiphertext,
    transfer_amount: u64,
    source_elgamal_keypair: &ElGamalKeypair,
    aes_key: &AeKey,
    destination_elgamal_pubkey: &ElGamalPubkey,
    auditor_elgamal_pubkey: &Option<&ElGamalPubkey>,
) -> Result<
    (
        CiphertextCommitmentEqualityProofData,
        BatchedGroupedCiphertext3HandlesValidityProofData,
        BatchedRangeProofU128Data,
    ),
    ProofGenerationError,
> {
    let default_auditor_pubkey = ElGamalPubkey::default();
    let auditor_elgamal_pubkey = auditor_elgamal_pubkey.unwrap_or(&default_auditor_pubkey);

    // Split the transfer amount into the low and high bit components
    let (transfer_amount_lo, transfer_amount_hi) =
        try_split_u64(transfer_amount, TRANSFER_AMOUNT_LO_BITS)
            .map_err(|_| ProofGenerationError::IllegalAmountBitLength)?;

    // Encrypt the 'lo' and 'hi' transfer amounts
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
        GroupedElGamal::to_elgamal_ciphertext(&transfer_amount_grouped_ciphertext_hi.0, 0).unwrap();

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

    // generate ciphertext validity data
    let ciphertext_validity_proof_data = BatchedGroupedCiphertext3HandlesValidityProofData::new(
        source_elgamal_keypair.pubkey(),
        destination_elgamal_pubkey,
        auditor_elgamal_pubkey,
        &transfer_amount_grouped_ciphertext_lo.0,
        &transfer_amount_grouped_ciphertext_hi.0,
        transfer_amount_lo,
        transfer_amount_hi,
        &transfer_amount_opening_lo,
        &transfer_amount_opening_hi,
    )?;

    // generate range proof data
    let (padding_commitment, padding_opening) = Pedersen::new(0_u64);
    let range_proof_data = BatchedRangeProofU128Data::new(
        vec![
            &new_available_balance_commitment,
            transfer_amount_grouped_ciphertext_lo.get_commitment(),
            transfer_amount_grouped_ciphertext_hi.get_commitment(),
            &padding_commitment,
        ],
        vec![
            new_decrypted_available_balance,
            transfer_amount_lo,
            transfer_amount_hi,
            0,
        ],
        vec![
            REMAINING_BALANCE_BIT_LENGTH,
            TRANSFER_AMOUNT_LO_BITS,
            TRANSFER_AMOUNT_HI_BITS,
            RANGE_PROOF_PADDING_BIT_LENGTH,
        ],
        vec![
            &new_source_opening,
            &transfer_amount_opening_lo,
            &transfer_amount_opening_hi,
            &padding_opening,
        ],
    )?;

    Ok((
        equality_proof_data,
        ciphertext_validity_proof_data,
        range_proof_data,
    ))
}

impl TransferProofContext {
    /// Create a transfer proof context information from split proof contexts.
    pub fn new_from_split_proofs(
        equality_proof_context: &CiphertextCommitmentEqualityProofContext,
        ciphertext_validity_proof_context: &BatchedGroupedCiphertext3HandlesValidityProofContext,
        range_proof_context: &BatchedRangeProofContext,
    ) -> Result<Self, ProofVerificationError> {
        // The equality proof context consists of the source ElGamal public key, the new
        // source available balance ciphertext, and the new source available
        // commitment. The public key and ciphertext should be returned as parts
        // of `TransferProofContextInfo` and the commitment should be checked
        // with range proof for consistency.
        let CiphertextCommitmentEqualityProofContext {
            pubkey: source_pubkey_from_equality_proof,
            ciphertext: new_source_ciphertext,
            commitment: new_source_commitment,
        } = equality_proof_context;

        // The ciphertext validity proof context consists of the destination ElGamal
        // public key, auditor ElGamal public key, and the transfer amount
        // ciphertexts. All of these fields should be returned as part of
        // `TransferProofContextInfo`. In addition, the commitments pertaining
        // to the transfer amount ciphertexts should be checked with range proof for
        // consistency.
        let BatchedGroupedCiphertext3HandlesValidityProofContext {
            source_pubkey: source_pubkey_from_validity_proof,
            destination_pubkey,
            auditor_pubkey,
            grouped_ciphertext_lo: transfer_amount_ciphertext_lo,
            grouped_ciphertext_hi: transfer_amount_ciphertext_hi,
        } = ciphertext_validity_proof_context;

        // The range proof context consists of the Pedersen commitments and bit-lengths
        // for which the range proof is proved. The commitments must consist of
        // three commitments pertaining to the new source available balance, the
        // low bits of the transfer amount, and high bits of the transfer
        // amount. These commitments must be checked for bit lengths `64`, `16`,
        // and `32`.
        let BatchedRangeProofContext {
            commitments: range_proof_commitments,
            bit_lengths: range_proof_bit_lengths,
        } = range_proof_context;

        // check that the ElGamal pubkeys from equality and validity proofs are consistent
        if source_pubkey_from_equality_proof != source_pubkey_from_validity_proof {
            return Err(ProofVerificationError::ProofContext);
        }

        // check that the range proof was created for the correct set of Pedersen
        // commitments
        let transfer_amount_commitment_lo = transfer_amount_ciphertext_lo.extract_commitment();
        let transfer_amount_commitment_hi = transfer_amount_ciphertext_hi.extract_commitment();

        let expected_commitments = [
            *new_source_commitment,
            transfer_amount_commitment_lo,
            transfer_amount_commitment_hi,
            // the fourth dummy commitment can be any commitment
        ];

        if !range_proof_commitments
            .iter()
            .zip(expected_commitments.iter())
            .all(|(proof_commitment, expected_commitment)| proof_commitment == expected_commitment)
        {
            return Err(ProofVerificationError::ProofContext);
        }

        let expected_bit_lengths = [
            REMAINING_BALANCE_BIT_LENGTH,
            TRANSFER_AMOUNT_LO_BITS,
            TRANSFER_AMOUNT_HI_BITS,
            RANGE_PROOF_PADDING_BIT_LENGTH,
        ]
        .iter();

        if !range_proof_bit_lengths
            .iter()
            .zip(expected_bit_lengths)
            .all(|(proof_len, expected_len)| *proof_len as usize == *expected_len)
        {
            return Err(ProofVerificationError::ProofContext);
        }

        let transfer_pubkeys = TransferPubkeys {
            source: *source_pubkey_from_equality_proof,
            destination: *destination_pubkey,
            auditor: *auditor_pubkey,
        };

        Ok(Self {
            ciphertext_lo: pod::TransferAmountCiphertext(*transfer_amount_ciphertext_lo),
            ciphertext_hi: pod::TransferAmountCiphertext(*transfer_amount_ciphertext_hi),
            transfer_pubkeys,
            new_source_ciphertext: *new_source_ciphertext,
        })
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{encryption::auth_encryption::AeKey, instruction::ZkProofData},
    };

    #[test]
    fn test_transfer_with_split_proof_correctness() {
        let source_keypair = ElGamalKeypair::new_rand();

        let destination_keypair = ElGamalKeypair::new_rand();
        let destination_pubkey = destination_keypair.pubkey();

        let auditor_keypair = ElGamalKeypair::new_rand();
        let auditor_pubkey = auditor_keypair.pubkey();

        let aes_key = AeKey::new_rand();
        let remaining_balance = 100;
        let remaining_balance_elgamal_ciphertext =
            source_keypair.pubkey().encrypt(remaining_balance);
        let remaining_balance_decryptable_ciphertext = aes_key.encrypt(remaining_balance);

        let transfer_amount = 10;

        let (equality_proof_data, validity_proof_data, range_proof_data) =
            transfer_split_proof_data(
                &remaining_balance_elgamal_ciphertext,
                &remaining_balance_decryptable_ciphertext,
                transfer_amount,
                &source_keypair,
                &aes_key,
                destination_pubkey,
                &Some(auditor_pubkey),
            )
            .unwrap();

        equality_proof_data.verify_proof().unwrap();
        validity_proof_data.verify_proof().unwrap();
        range_proof_data.verify_proof().unwrap();

        TransferProofContext::new_from_split_proofs(
            equality_proof_data.context_data(),
            validity_proof_data.context_data(),
            range_proof_data.context_data(),
        )
        .unwrap();
    }
}
