//! Provenance is what *this node* knows about the origin of the Shred.

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
