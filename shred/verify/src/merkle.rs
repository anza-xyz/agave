//! The leaf a shred hashes to, and the root its proof climbs to.
//!
//! The tree itself is [`merkle_tree`](crate::shred::merkle_tree), which is the same file
//! `solana-ledger` builds its trees from rather than a second implementation of it. What is left
//! for this module is the part that is about shreds instead of trees: which bytes of a shred are
//! hashed into its leaf, under which prefix, and where its leaf sits in the tree.

use {
    crate::{
        error::MerkleError,
        shred::merkle_tree::{self, MERKLE_HASH_PREFIX_LEAF, MerkleProofEntry, MerkleTree},
    },
    solana_hash::Hash,
    solana_sha256_hasher::hashv,
    solana_shred_wire_format::{kind::ShredLayout, view::ShredView},
};

/// The leaf hash of a shred, over its
/// [`merkle_leaf`](solana_shred_wire_format::view::ShredView::merkle_leaf) region.
///
/// The leaf's index in the tree is the shred's
/// [`erasure_shard_index`](ShredLayout::erasure_shard_index): data shards first, then code shards.
#[inline]
pub fn leaf(merkle_leaf: &[u8]) -> Hash {
    hashv(&[MERKLE_HASH_PREFIX_LEAF, merkle_leaf])
}

/// The root a `proof` for the leaf at `index` climbs to.
#[inline]
pub fn root<'a, I>(index: usize, leaf: Hash, proof: I) -> Result<Hash, MerkleError>
where
    I: IntoIterator<Item = &'a MerkleProofEntry>,
{
    merkle_tree::get_merkle_root(index, leaf, proof)
}

/// The Merkle root a shred's own proof reconstructs from its own leaf.
///
/// This is the message both the leader's and the retransmitter's signatures are over. A shred
/// carries no root of its own, only the previous batch's, so it has to be recomputed.
pub fn root_of<K: ShredLayout>(view: &ShredView<'_, K>) -> Result<Hash, MerkleError> {
    let index = K::erasure_shard_index(&view.common, &view.header);
    root(index, leaf(view.merkle_leaf), view.merkle_proof)
}

/// The Merkle tree over an erasure batch's `leaves`, in shard order.
///
/// [`MerkleTree::try_new`](crate::MerkleTree) is not public in the shared tree file, so this
/// reproduces its one added check, that a tree needs at least one leaf, over the public
/// constructor.
pub fn tree(leaves: impl ExactSizeIterator<Item = Hash>) -> Result<MerkleTree, MerkleError> {
    let len = leaves.len();
    if len == 0 {
        return Err(MerkleError::EmptyIterator);
    }
    MerkleTree::try_new_with_len(leaves.map(Ok), len)
}
