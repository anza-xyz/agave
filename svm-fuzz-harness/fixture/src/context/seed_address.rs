//! Seed-derived address.

use crate::proto::SeedAddress as ProtoSeedAddress;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeedAddress {
    /// The seed address base (32 bytes).
    pub base: Vec<u8>,
    /// The seed path  (<= 32 bytes).
    pub seed: Vec<u8>,
    /// The seed address owner (32 bytes).
    pub owner: Vec<u8>,
}

impl From<ProtoSeedAddress> for SeedAddress {
    fn from(value: ProtoSeedAddress) -> Self {
        let ProtoSeedAddress { base, seed, owner } = value;
        Self { base, seed, owner }
    }
}

impl From<SeedAddress> for ProtoSeedAddress {
    fn from(value: SeedAddress) -> Self {
        let SeedAddress { base, seed, owner } = value;
        ProtoSeedAddress { base, seed, owner }
    }
}
