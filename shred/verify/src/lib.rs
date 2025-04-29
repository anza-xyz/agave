#![cfg(feature = "agave-unstable-api")]
//! The crypto half of the shred format: the Merkle tree over an erasure batch, the leaf a shred
//! hashes to, and the two signatures over the root.
//!
//! What a shred's bytes mean is `solana-shred-wire-format`; what has to be true of them before they
//! can be trusted is here. Everything is a free function over a view or a hash, so the typestate
//! that decides *when* to call them stays in `solana-shred`.

pub mod error;
pub mod merkle;
pub mod shred;

pub use {
    error::MerkleError,
    shred::merkle_tree::{
        MerkleProofEntry, MerkleTree, SIZE_OF_MERKLE_PROOF_ENTRY, hash_as_merkle_proof_entry,
    },
};
use {
    solana_hash::Hash, solana_keypair::Keypair, solana_pubkey::Pubkey,
    solana_shred_wire_format::constants, solana_signature::Signature, solana_signer::Signer,
    static_assertions::const_assert_eq,
};

// The wire format states the proof geometry without hashing anything, and the shared tree file
// states it again as a consequence of how it hashes. These pin the two to each other.
const_assert_eq!(
    SIZE_OF_MERKLE_PROOF_ENTRY,
    constants::SIZE_OF_MERKLE_PROOF_ENTRY
);
const_assert_eq!(
    shred::merkle_tree::PROOF_ENTRIES_FOR_32_32_BATCH as usize,
    constants::MERKLE_PROOF_ENTRIES
);

/// Checks `signer`'s signature over `root`.
///
/// Both the leader's signature and a retransmitter's are over the Merkle root of the shred's
/// erasure batch, so this is the same check either way; who the signer should be is the caller's to
/// work out.
#[inline]
pub fn verify(signature: &Signature, signer: &Pubkey, root: &Hash) -> bool {
    signature.verify(signer.as_ref(), root.as_ref())
}

/// Signs `root`, which is what both the leader and a retransmitter put their signature over.
#[inline]
pub fn sign(keypair: &Keypair, root: &Hash) -> Signature {
    keypair.sign_message(root.as_ref())
}
