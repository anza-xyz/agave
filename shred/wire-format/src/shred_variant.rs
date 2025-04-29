//! The variant byte at offset 64, which selects the shred's kind and layout.
//!
//! The byte packs the kind, the resigned flag and the proof length into one field with a sparse,
//! historically-chosen encoding. With the proof length fixed
//! ([`MERKLE_PROOF_ENTRIES`](crate::constants::MERKLE_PROOF_ENTRIES)) only four bytes are valid,
//! which makes it a plain tagged enum: one table gives the encode and decode directions the same
//! notion of which bit patterns exist, so they cannot disagree, and no hand-written serialization is
//! needed.

use {
    crate::error::ParseError,
    wincode::{SchemaRead, SchemaWrite},
};

/// Which of the two kinds of shred this is.
///
/// The discriminants are the legacy standalone encoding of the shred type, kept distinct from every
/// valid [`ShredVariant`] byte so that the two can never be confused on the wire. They are also the
/// wincode tags, so reading a shred type off the wire and matching a Rust variant cannot drift
/// apart.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SchemaRead, SchemaWrite)]
#[wincode(tag_encoding = "u8")]
#[repr(u8)]
pub enum ShredKind {
    /// Carries ledger entries.
    #[wincode(tag = 0b1010_0101)]
    Data = 0b1010_0101,
    /// Carries Reed-Solomon erasure codes for the data shreds of its FEC set.
    #[wincode(tag = 0b0101_1010)]
    Code = 0b0101_1010,
}

/// The kind of a shred plus the one layout bit that accompanies it.
///
/// The high nibble identifies the kind and whether a retransmitter signature trails the proof; the
/// low nibble is the number of Merkle proof entries, which is
/// [`MERKLE_PROOF_ENTRIES`](crate::constants::MERKLE_PROOF_ENTRIES) in every shred a leader is
/// allowed to produce:
///
/// ```text
/// 0b0110_0110  0x66  Code
/// 0b0111_0110  0x76  Code, resigned
/// 0b1001_0110  0x96  Data
/// 0b1011_0110  0xb6  Data, resigned
/// ```
///
/// Every other byte is invalid.
///
/// The discriminants are the wincode tags, which is what makes a byte off the wire and a Rust
/// variant the same thing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SchemaRead, SchemaWrite)]
#[wincode(tag_encoding = "u8")]
#[repr(u8)]
pub enum ShredVariant {
    /// A code shred.
    #[wincode(tag = 0x66)]
    MerkleCode = 0x66,
    /// A code shred whose Merkle proof is followed by a retransmitter signature.
    #[wincode(tag = 0x76)]
    MerkleCodeResigned = 0x76,
    /// A data shred.
    #[wincode(tag = 0x96)]
    MerkleData = 0x96,
    /// A data shred whose Merkle proof is followed by a retransmitter signature.
    #[wincode(tag = 0xb6)]
    MerkleDataResigned = 0xb6,
}

impl ShredVariant {
    /// The data-shred variant of the layout `resigned` selects.
    #[inline]
    pub const fn data(resigned: bool) -> Self {
        match resigned {
            true => Self::MerkleDataResigned,
            false => Self::MerkleData,
        }
    }

    /// The code-shred variant of the layout `resigned` selects.
    #[inline]
    pub const fn code(resigned: bool) -> Self {
        match resigned {
            true => Self::MerkleCodeResigned,
            false => Self::MerkleCode,
        }
    }

    /// Whether a retransmitter signature trails the Merkle proof.
    #[inline]
    pub const fn resigned(self) -> bool {
        matches!(self, Self::MerkleCodeResigned | Self::MerkleDataResigned)
    }

    /// Whether this variant carries ledger data or erasure codes.
    #[inline]
    pub const fn shred_kind(self) -> ShredKind {
        match self {
            Self::MerkleCode | Self::MerkleCodeResigned => ShredKind::Code,
            Self::MerkleData | Self::MerkleDataResigned => ShredKind::Data,
        }
    }
}

impl From<ShredVariant> for u8 {
    #[inline]
    fn from(variant: ShredVariant) -> u8 {
        variant as u8
    }
}

impl TryFrom<u8> for ShredVariant {
    type Error = ParseError;

    #[inline]
    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        wincode::deserialize(&[byte]).map_err(|_| ParseError::InvalidVariant(byte))
    }
}
