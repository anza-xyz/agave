use {
    agave_shred_verify::MerkleError, agave_shred_wire_format::error::ParseError,
    solana_clock::Slot, thiserror::Error,
};

/// A shred, or an erasure batch of them, could not be built as specified.
#[derive(Debug, Error)]
pub enum BuildError {
    /// The data does not fit in one erasure batch, so it belongs to more than one.
    #[error("{len} bytes of data exceed an erasure batch's capacity of {capacity}")]
    TooMuchData {
        /// Length of the data offered.
        len: usize,
        /// What one batch can carry.
        capacity: usize,
    },
    /// The batch was asked to start where no FEC set starts.
    #[error("fec set index {fec_set_index} does not start an erasure batch")]
    MisalignedFecSet {
        /// The index the batch was asked to start at.
        fec_set_index: u32,
    },
    /// The parent slot is not a slot this one can chain to within a `u16` offset.
    #[error("slot {slot} cannot chain to parent slot {parent_slot}")]
    BadParentSlot {
        /// The slot being built.
        slot: Slot,
        /// The slot it was asked to chain to.
        parent_slot: Slot,
    },
    /// A shred index ran past the end of its type.
    #[error("shred index overflowed")]
    IndexOverflow,
    /// The erasure coder rejected the batch.
    #[error(transparent)]
    Erasure(#[from] reed_solomon_erasure::Error),
    /// A header could not be serialized.
    #[error(transparent)]
    Write(#[from] wincode::WriteError),
    /// The Merkle tree over the batch could not be built.
    #[error(transparent)]
    Merkle(#[from] MerkleError),
    /// The bytes that were built do not read back as the shred they were meant to be, which is a
    /// bug in this crate rather than anything the caller did.
    #[error(transparent)]
    Layout(#[from] ParseError),
}
