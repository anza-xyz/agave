//! Provenance is what *this node* knows about the origin of the Shred.
//!
//! Where the bytes came from is a field, [`Provenance`],
//! not a type parameter. Provenance for a shred can not be modified.
//!
//! Four things can put a shred in [`Verified`](crate::state::Verified), one per provenance:
//!
//! ```text
//! check_policy() + verify()  Received(source)   hash and ed25519
//! recover(data, code)        Recovered          the batch's root is checked as it is rebuilt
//! from_blockstore(bytes)     Blockstore         verified before it was ever stored
//! FecSet::build(..)          BlockProduction    signed here
//! ```
//!
//! [`resign`](crate::shred::Shred::resign) accepts only
//! [`Provenance::Received`].

/// Which socket a received shred arrived on.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShredSource {
    Turbine,
    Repair,
}

/// How a shred reached this node.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Provenance {
    /// Arrived from a peer, over the socket named here.
    Received(ShredSource),
    /// Rebuilt by erasure recovery from a batch whose Merkle root was verified.
    Recovered,
    /// Read back from this node's blockstore, which only stores verified shreds.
    Blockstore,
    /// Built here, by this node's own block production.
    BlockProduction,
}

impl Provenance {
    #[inline]
    pub const fn is_received(self) -> bool {
        matches!(self, Self::Received(_))
    }
}
