//! What can go wrong while hashing or checking a shred's Merkle proof.

use thiserror::Error;

/// Why a Merkle tree could not be built, or a proof could not be checked.
///
/// This is the error type the shared `merkle_tree` file raises, which is why its variants are named
/// after that file's needs rather than this crate's.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MerkleError {
    /// A tree was asked for over no leaves at all.
    #[error("a Merkle tree needs at least one leaf")]
    EmptyIterator,
    /// The proof does not have the shape a proof for this leaf must have.
    #[error("the Merkle proof is not a proof of this leaf")]
    InvalidMerkleProof,
}
