//! What can go wrong, split by which stage raises it: [`ParseError`] for bytes that are not a
//! shred, [`Reject`] for a shred that does not pass a stage of the cascade, [`BuildError`] for a
//! batch that could not be built, [`RecoverError`] for one that could not be rebuilt.

use {crate::shred_variant::ShredKind, solana_clock::Slot, thiserror::Error};

/// A shred's bytes could not be interpreted as a well-formed shred.
#[derive(Debug, Error)]
pub enum ParseError {
    /// Fewer bytes than the shred requires.
    #[error("shred is {len} bytes, expected at least {expected}")]
    TooShort {
        /// Number of bytes available.
        len: usize,
        /// Number of bytes the shred kind requires.
        expected: usize,
    },
    /// The byte at offset 64 is not a valid [`ShredVariant`](crate::shred_variant::ShredVariant).
    #[error("invalid shred variant: {0:#04x}")]
    InvalidVariant(u8),
    /// The shred is followed by unexpected bytes.
    #[error("{0} trailing bytes after the shred")]
    TrailingBytes(usize),
    /// A repair response carried no/incomplete nonce.
    #[error("repair response carries no nonce")]
    MissingNonce,
    /// The shred is of the other kind than the one requested.
    #[error("expected a {expected:?} shred, got {found:?}")]
    UnexpectedKind {
        /// The kind the caller asked for.
        expected: ShredKind,
        /// The kind found on the wire.
        found: ShredKind,
    },
    /// A data shred's `size` field does not describe a region inside the shred's body.
    ///
    /// The field covers the headers as well as the data, so it must be at least the length of the
    /// headers and at most that plus the body the layout leaves.
    #[error("data size {size} does not describe a region inside the shred's body")]
    InvalidDataSize {
        /// The size the data header claims.
        size: u16,
    },
    /// The headers could not be deserialized.
    #[error(transparent)]
    Read(#[from] wincode::ReadError),
}

/// Why a shred did not advance to the next state.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum Reject {
    /// The shred belongs to a different cluster.
    #[error("shred version {found} does not match the cluster's {expected}")]
    ShredVersionMismatch {
        /// The version this node accepts.
        expected: u16,
        /// The version the shred carries.
        found: u16,
    },
    /// The slot is at or below the root, or too far ahead of it.
    #[error("slot {slot} is outside the acceptable range")]
    SlotOutOfRange {
        /// The shred's slot.
        slot: Slot,
    },
    /// The index exceeds the per-slot limit for this shred kind.
    #[error("shred index {index} exceeds the per-slot limit")]
    IndexOutOfBounds {
        /// The shred's index.
        index: u32,
    },
    /// The parent offset is zero for a non-genesis slot, or reaches below slot zero.
    #[error("slot {slot} cannot chain to a parent {parent_offset} slots back")]
    BadParentOffset {
        /// The shred's slot.
        slot: Slot,
        /// The offset back to the claimed parent.
        parent_offset: u16,
    },
    /// The index is not consistent with the FEC set it claims to belong to.
    #[error("shred index {index} does not belong to the FEC set at {fec_set_index}")]
    MisalignedFecSet {
        /// The shred's index.
        index: u32,
        /// The claimed first index of the FEC set.
        fec_set_index: u32,
    },
    /// `DATA_COMPLETE_SHRED` is set on a shred that is not the last of its FEC set.
    #[error("DATA_COMPLETE_SHRED is set on a shred that does not end its FEC set")]
    UnexpectedDataCompleteShred,
    /// `LAST_SHRED_IN_SLOT` is set on an index that cannot end a slot.
    #[error("LAST_SHRED_IN_SLOT is set on an index that cannot end a slot")]
    MisalignedLastDataIndex,
    /// The FEC set is not the fixed 32:32 configuration.
    #[error("erasure config {num_data_shreds}:{num_code_shreds} is not the fixed configuration")]
    MisalignedErasureConfig {
        /// Number of data shreds claimed.
        num_data_shreds: u16,
        /// Number of code shreds claimed.
        num_code_shreds: u16,
    },
    /// A code shred's position does not agree with its index within its FEC set.
    #[error(
        "code shred at index {index} claims position {position} in the FEC set at {fec_set_index}"
    )]
    MisalignedCodePosition {
        /// The shred's index.
        index: u32,
        /// The claimed first index of the FEC set.
        fec_set_index: u32,
        /// The position the code header claims among the FEC set's code shreds.
        position: u16,
    },
    /// The Merkle proof does not reconstruct a root.
    #[error("the Merkle proof does not reconstruct a root")]
    InvalidMerkleProof,
    /// The signature does not verify against the expected signer.
    #[error("the signature does not verify against the expected signer")]
    InvalidSignature,
    /// A retransmitter signature was asked for on a shred whose variant reserves no room for one.
    #[error("this shred's variant carries no retransmitter signature")]
    MissingRetransmitterSignature,
    /// The retransmitter signature does not verify against the expected retransmitter.
    #[error("the retransmitter signature does not verify against the expected retransmitter")]
    InvalidRetransmitterSignature,
    /// A retransmitter signature was requested for a shred no peer sent this node.
    #[error("only a shred received from a peer can be retransmitter-signed")]
    NotReceived,
}

/// Why a Merkle tree could not be built, or a proof could not be checked.
///
/// This is the error type the shared `merkle_tree` file raises, which is why its variants are named
/// after that file's needs rather than this crate's.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MerkleError {
    /// A tree was asked for over no leaves at all.
    #[error("a Merkle tree needs at least one leaf")]
    EmptyIterator,
    /// The proof does not have the shape a proof for this leaf must have.
    #[error("the Merkle proof is not a proof of this leaf")]
    InvalidMerkleProof,
}

impl From<MerkleError> for Reject {
    fn from(_error: MerkleError) -> Self {
        Self::InvalidMerkleProof
    }
}

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
    /// A surviving shred's own Merkle proof does not reconstruct a root, so there is nothing to
    /// check the rebuilt batch against.
    #[error(transparent)]
    Root(#[from] Reject),
    /// A rebuilt header could not be serialized.
    #[error(transparent)]
    Write(#[from] wincode::WriteError),
    /// The rebuilt bytes do not read back as the shred they were meant to be, which is a bug in
    /// this crate rather than anything the caller did.
    #[error(transparent)]
    Layout(#[from] ParseError),
}

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
