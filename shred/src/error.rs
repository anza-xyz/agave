//! Errors of the validation cascade, one type per transition.

use {crate::shred_variant::ShredType, solana_clock::Slot, thiserror::Error};

/// A shred's bytes could not be read as a well-formed shred.
#[derive(Debug, Error)]
pub enum ParseError {
    /// Fewer bytes than the shred kind's fixed payload length.
    #[error("shred is {len} bytes, expected at least {expected}")]
    TooShort {
        /// Number of bytes available.
        len: usize,
        /// Number of bytes the shred kind requires.
        expected: usize,
    },
    /// The byte at offset 64 is not a valid [`ShredVariant`](crate::ShredVariant).
    #[error("invalid shred variant: {0:#04x}")]
    InvalidVariant(u8),
    /// The variant's `proof_size` leaves no room for the shred's body.
    #[error("invalid proof size: {0}")]
    InvalidProofSize(u8),
    /// The shred is followed by neither nothing nor a 4-byte repair nonce.
    #[error("{0} trailing bytes after the shred")]
    TrailingBytes(usize),
    /// The shred is of the other kind than the one requested.
    #[error("expected a {expected:?} shred, got {found:?}")]
    UnexpectedKind {
        /// The kind the caller asked for.
        expected: ShredType,
        /// The kind found on the wire.
        found: ShredType,
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
    /// The Merkle proof does not reconstruct a root.
    #[error("the Merkle proof does not reconstruct a root")]
    InvalidMerkleProof,
    /// The signature does not verify against the expected signer.
    #[error("the signature does not verify against the expected signer")]
    InvalidSignature,
    /// A retransmitter signature was requested on a variant that has no room for one.
    #[error("this shred variant reserves no room for a retransmitter signature")]
    NotResignable,
}

/// A data shred's `size` field does not describe a region inside the shred's body.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid data size: {size}")]
pub struct InvalidDataSize {
    /// The size the data header claims.
    pub size: usize,
}
