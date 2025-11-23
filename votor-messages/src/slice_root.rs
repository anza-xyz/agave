//! Module to define [`SliceRoot`].

use {
    rand::Rng,
    serde::{Deserialize, Serialize},
    solana_hash::Hash,
    std::fmt::Display,
};

/// For every FEC set (AKA slice) of shreds, we have a Merkle tree over the shreds signed by the leader.
/// This is the root of the tree.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "7NWdx9UtHWLUeqzPF89smNiRta2uVaEEHZSBPxCtNuUe")
)]
#[derive(
    Copy, Clone, Default, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct SliceRoot(pub Hash);

impl SliceRoot {
    /// Generates a unique [`SliceRoot`].
    pub fn new_unique() -> Self {
        Self(Hash::new_unique())
    }

    /// Generates a random [`SliceRoot`].
    pub fn new_random() -> Self {
        Self(Hash::new_from_array(rand::thread_rng().gen()))
    }
}

impl Display for SliceRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
