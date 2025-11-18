//! Module to define [`SliceRoot`].

use {
    serde::{Deserialize, Serialize},
    solana_hash::Hash,
    std::fmt::Display,
};

/// For every FEC set (AKA slice) of shreds, we have a Merkle tree over the shreds signed by the leader.
/// This is the root of the tree.
#[derive(
    Copy, Clone, Default, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct SliceRoot(pub Hash);

impl SliceRoot {
    /// Generates a unique [`SliceRoot`].
    pub fn new_unique() -> Self {
        SliceRoot(Hash::new_unique())
    }
}

impl Display for SliceRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
