use {
    agave_shred_verify::MerkleError, agave_shred_wire_format::error::ParseError, thiserror::Error,
};

/// Why an erasure batch could not be rebuilt.
#[derive(Debug, Error)]
pub enum RecoverError {
    /// Fewer than a batch's worth of data shards survive, so the batch is unrecoverable.
    #[error("{have} shards cannot rebuild a batch that needs {need}")]
    NotEnoughShards {
        /// Number of distinct shards offered.
        have: usize,
        /// Number of shards Reed-Solomon needs.
        need: usize,
    },
    /// The shreds offered do not all belong to the same FEC set.
    #[error("the shreds do not all belong to the same FEC set")]
    MixedFecSets,
    /// A shred's headers do not place it where its FEC set has room for it: a data shred past the
    /// data shards, a code shred whose position disagrees with its index, or an erasure
    /// configuration other than the fixed one.
    #[error("shred at index {index} has no place in the FEC set at {fec_set_index}")]
    MisplacedShred {
        /// The shred's index.
        index: u32,
        /// The first index of the FEC set it claims.
        fec_set_index: u32,
    },
    /// Two shreds claim the same shard of the batch.
    #[error("shard index {index} was offered twice")]
    DuplicateShard {
        /// The index claimed twice.
        index: usize,
    },
    /// A data shred came out of the coder with headers that do not describe the shard it was
    /// rebuilt at.
    #[error("the shred rebuilt at shard index {index} does not belong there")]
    InvalidRebuiltShred {
        /// The shard index it was rebuilt at.
        index: usize,
    },
    /// The rebuilt batch hashes to a different root than the surviving shreds prove, which means
    /// either the shards it was rebuilt from did not all come from one batch, or the leader's batch
    /// is not a valid erasure codeword.
    #[error("the rebuilt batch does not hash to the root the surviving shreds prove")]
    RootMismatch,
    /// The erasure coder could not reconstruct the batch.
    #[error(transparent)]
    Erasure(#[from] reed_solomon_erasure::Error),
    /// The Merkle tree over the rebuilt batch could not be built.
    #[error(transparent)]
    Merkle(#[from] MerkleError),
    /// A rebuilt header could not be serialized.
    #[error(transparent)]
    Write(#[from] wincode::WriteError),
    /// A survivor does not read as a shred, or rebuilt bytes do not read back as the shred they
    /// were meant to be.
    #[error(transparent)]
    Layout(#[from] ParseError),
}
