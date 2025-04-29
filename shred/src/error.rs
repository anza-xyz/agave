use {solana_clock::Slot, thiserror::Error};
pub use {
    solana_fec_set_recovery::error::RecoverError, solana_shred_verify::MerkleError,
    solana_shred_wire_format::error::ParseError, solana_shredder::error::BuildError,
};

/// Why a shred is invalid.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RejectReason {
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

impl From<MerkleError> for RejectReason {
    fn from(_error: MerkleError) -> Self {
        Self::InvalidMerkleProof
    }
}
