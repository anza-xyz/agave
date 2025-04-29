use {
    solana_shred_verify::MerkleError, solana_shred_wire_format::error::ParseError, thiserror::Error,
};

/// Why the missing shreds of an erasure batch could not be rebuilt.
#[derive(Debug, Error)]
pub enum RecoverError {
    /// Recovery was asked for over no shreds at all.
    #[error("erasure recovery needs at least one shred to work from")]
    NoShreds,
    /// Fewer than a batch's worth of data shards survive, so the batch is unrecoverable.
    #[error("{have} shards cannot rebuild a batch that needs {need}")]
    NotEnoughShards {
        /// Number of distinct shards offered.
        have: usize,
        /// Number of shards Reed-Solomon needs.
        need: usize,
    },
    /// The shreds offered do not all belong to one FEC set.
    #[error("the shreds do not all belong to the same FEC set")]
    MixedFecSets,
    /// A shred's shard index falls outside its own batch.
    #[error("shard index {index} is outside a batch of {shards} shards")]
    ShardIndexOutOfRange {
        /// The index the shred claims.
        index: usize,
        /// Number of shards in a batch.
        shards: usize,
    },
    /// Two shreds claim the same shard of the batch.
    #[error("shard index {index} was offered twice")]
    DuplicateShard {
        /// The index claimed twice.
        index: usize,
    },
    /// The rebuilt batch hashes to a different root than the surviving shreds prove, which means
    /// the shards it was rebuilt from did not all come from one batch.
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
    /// The rebuilt bytes do not read back as the shred they were meant to be, which is a bug in
    /// this crate rather than anything the caller did.
    #[error(transparent)]
    Layout(#[from] ParseError),
}
