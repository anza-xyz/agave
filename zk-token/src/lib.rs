use {
    crate::errors::ProofDataError,
    curve25519_dalek::scalar::Scalar,
    solana_zk_sdk::encryption::{
        elgamal::ElGamalCiphertext,
        pedersen::{PedersenCommitment, PedersenOpening},
    },
};

pub mod encryption;
pub mod errors;
pub mod transfer;
pub mod transfer_with_fee;

/// Takes in a 64-bit number `amount` and a bit length `bit_length`. It returns:
/// - the `bit_length` low bits of `amount` interpretted as u64
/// - the `(64 - bit_length)` high bits of `amount` interpretted as u64
#[cfg(not(target_os = "solana"))]
pub fn try_split_u64(amount: u64, bit_length: usize) -> Result<(u64, u64), ProofDataError> {
    match bit_length {
        0 => Ok((0, amount)),
        1..=63 => {
            let bit_length_complement = u64::BITS.checked_sub(bit_length as u32).unwrap();
            // shifts are safe as long as `bit_length` and `bit_length_complement` < 64
            let lo = amount
                .checked_shl(bit_length_complement) // clear out the high bits
                .and_then(|amount| amount.checked_shr(bit_length_complement))
                .unwrap(); // shift back
            let hi = amount.checked_shr(bit_length as u32).unwrap();
            Ok((lo, hi))
        }
        64 => Ok((amount, 0)),
        _ => Err(ProofDataError::IllegalAmountBitLength),
    }
}

/// Combine two numbers that are interpretted as the low and high bits of a target number. The
/// `bit_length` parameter specifies the number of bits that `amount_hi` is to be shifted by.
#[cfg(not(target_os = "solana"))]
pub fn try_combine_lo_hi_u64(
    amount_lo: u64,
    amount_hi: u64,
    bit_length: usize,
) -> Result<u64, ProofDataError> {
    match bit_length {
        0 => Ok(amount_hi),
        1..=63 => {
            // shifts are safe as long as `bit_length` < 64
            let amount_hi = amount_hi.checked_shl(bit_length as u32).unwrap();
            let combined = amount_lo
                .checked_add(amount_hi)
                .ok_or(ProofDataError::IllegalAmountBitLength)?;
            Ok(combined)
        }
        64 => Ok(amount_lo),
        _ => Err(ProofDataError::IllegalAmountBitLength),
    }
}
#[cfg(not(target_os = "solana"))]
pub fn try_combine_lo_hi_ciphertexts(
    ciphertext_lo: &ElGamalCiphertext,
    ciphertext_hi: &ElGamalCiphertext,
    bit_length: usize,
) -> Result<ElGamalCiphertext, ProofDataError> {
    let two_power = if bit_length < u64::BITS as usize {
        1_u64.checked_shl(bit_length as u32).unwrap()
    } else {
        return Err(ProofDataError::IllegalAmountBitLength);
    };
    Ok(ciphertext_lo + ciphertext_hi * Scalar::from(two_power))
}

#[cfg(not(target_os = "solana"))]
pub fn try_combine_lo_hi_commitments(
    comm_lo: &PedersenCommitment,
    comm_hi: &PedersenCommitment,
    bit_length: usize,
) -> Result<PedersenCommitment, ProofDataError> {
    let two_power = if bit_length < u64::BITS as usize {
        1_u64.checked_shl(bit_length as u32).unwrap()
    } else {
        return Err(ProofDataError::IllegalAmountBitLength);
    };
    Ok(comm_lo + comm_hi * Scalar::from(two_power))
}

#[cfg(not(target_os = "solana"))]
pub fn try_combine_lo_hi_openings(
    opening_lo: &PedersenOpening,
    opening_hi: &PedersenOpening,
    bit_length: usize,
) -> Result<PedersenOpening, ProofDataError> {
    let two_power = if bit_length < u64::BITS as usize {
        1_u64.checked_shl(bit_length as u32).unwrap()
    } else {
        return Err(ProofDataError::IllegalAmountBitLength);
    };
    Ok(opening_lo + opening_hi * Scalar::from(two_power))
}
