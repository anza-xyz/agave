//! The leaf a shred hashes to.
//!
//! The tree itself is [`merkle_tree`](crate::shred::merkle_tree), which is the same file
//! `solana-ledger` builds its trees from rather than a second implementation of it. What is left
//! for this module is the part that is about shreds instead of trees: which bytes of a shred are
//! hashed into its leaf, and under which prefix.

use {
    crate::shred::merkle_tree::MERKLE_HASH_PREFIX_LEAF, solana_hash::Hash,
    solana_sha256_hasher::hashv,
};

/// The leaf hash of a shred, over its [`merkle_leaf`](crate::view::ShredView::merkle_leaf) region.
///
/// The leaf's index in the tree is the shred's
/// [`erasure_shard_index`](crate::shred::Shred::erasure_shard_index): data shards first, then code
/// shards.
#[inline]
pub fn leaf(merkle_leaf: &[u8]) -> Hash {
    hashv(&[MERKLE_HASH_PREFIX_LEAF, merkle_leaf])
}
