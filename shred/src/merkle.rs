//! Shape checks on a shred's Merkle proof.
//!
//! Tree construction, proof generation and root recomputation are out of scope for this draft and
//! still live in `solana-ledger`. This module only checks that the proof region has a shape a
//! verifier could work with: a whole number of proof entries, deep enough to cover the leaf's index.

use crate::{error::Reject, layout::ProofEntry};

/// Checks that `proof` could be a Merkle proof for a leaf at `index`.
pub fn check_proof_shape(index: usize, proof: &[ProofEntry]) -> Result<(), Reject> {
    // `proof.len()` proof entries can only witness a leaf in a tree of `2^proof.len()` leaves.
    let depth = u32::try_from(proof.len()).map_err(|_| Reject::InvalidMerkleProof)?;
    if 1usize
        .checked_shl(depth)
        .is_none_or(|leaves| index >= leaves)
    {
        return Err(Reject::InvalidMerkleProof);
    }
    Ok(())
}
