//! Shape checks on a shred's Merkle proof.
//!
//! Tree construction, proof generation and root recomputation are out of scope for this draft and
//! still live in `solana-ledger`. This module only checks that the proof region has a shape a
//! verifier could work with: a whole number of proof entries, deep enough to cover the leaf's index.

use crate::{error::Reject, layout::SIZE_OF_MERKLE_PROOF_ENTRY};

/// Checks that `proof` could be a Merkle proof for a leaf at `index`.
pub fn check_proof_shape(index: usize, proof: &[u8]) -> Result<(), Reject> {
    let (entries, remainder) = proof.as_chunks::<SIZE_OF_MERKLE_PROOF_ENTRY>();
    if !remainder.is_empty() {
        // A proof is a flat array of fixed-size entries, so a partial entry cannot be a proof.
        return Err(Reject::InvalidMerkleProof);
    }
    // `entries.len()` proof entries can only witness a leaf in a tree of `2^entries.len()` leaves.
    let depth = u32::try_from(entries.len()).map_err(|_| Reject::InvalidMerkleProof)?;
    if 1usize
        .checked_shl(depth)
        .is_none_or(|leaves| index >= leaves)
    {
        return Err(Reject::InvalidMerkleProof);
    }
    Ok(())
}
