//! Module to define FecSetRoot which represents the merkle root of a single FEC set (Slice).

use {
    serde::{Deserialize, Serialize},
    solana_hash::{Hash, HASH_BYTES},
    std::borrow::Borrow,
};

/// A type to represent the merkle root of a single FEC set (Slice).
#[derive(
    Copy, Clone, Default, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct FecSetRoot([u8; HASH_BYTES]);

impl FecSetRoot {
    /// Constructs a new [`FecSetRoot`] from a [`Hash`].
    ///
    /// Since the [`merkle_tree`] module is responsible for constructing merkle trees and knows
    /// which node in the tree is the root, the visibility is restricted to just this module.
    pub(super) fn from_hash(hash: Hash) -> Self {
        Self(hash.to_bytes())
    }

    /// Constructs a new unique [`FecSetRoot`] for test and benchmarking purposes.
    #[cfg(test)]
    pub fn new_unique() -> Self {
        Self::from_hash(Hash::new_unique())
    }
}

impl From<FecSetRoot> for Hash {
    fn from(value: FecSetRoot) -> Self {
        value.0.into()
    }
}

impl AsRef<[u8; HASH_BYTES]> for FecSetRoot {
    fn as_ref(&self) -> &[u8; HASH_BYTES] {
        &self.0
    }
}

impl Borrow<[u8]> for FecSetRoot {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}
