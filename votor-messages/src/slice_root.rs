//! Module to define [`SliceRoot`].

use {
    serde::{Deserialize, Serialize},
    solana_hash::{Hash, HASH_BYTES},
    std::fmt::Display,
};

/// For every FEC set (AKA slice) of shreds, we have a Merkle tree over the shreds signed by the leader.
/// This is the root of the tree.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "4inPqcaUikLaW1bDBfcHHYCLTBYJAfqvrUwKaSzVTP4B")
)]
#[derive(
    Copy, Clone, Default, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct SliceRoot(pub [u8; HASH_BYTES]);

impl SliceRoot {
    /// Generates a unique [`SliceRoot`] to be used for tests and benchmarks.
    pub fn new_unique() -> Self {
        let hash = Hash::new_unique();
        Self(hash.to_bytes())
    }

    /// Generates a random [`SliceRoot`].
    pub fn new_random() -> Self {
        let hash = Hash::new_from_array(rand::random());
        Self(hash.to_bytes())
    }
}

impl Display for SliceRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = Hash::from(self.0);
        write!(f, "{hash}")
    }
}
