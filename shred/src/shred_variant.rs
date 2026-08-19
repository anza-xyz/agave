//! The variant byte at offset 64, which selects the shred's kind and layout.

use {
    crate::error::ParseError,
    std::mem::MaybeUninit,
    wincode::{
        SchemaRead, SchemaWrite, TypeMeta,
        config::ConfigCore,
        io::{Reader, Writer},
    },
};

/// Which of the two kinds of shred this is.
///
/// The discriminants are the legacy standalone encoding of the shred type, kept distinct from every
/// valid [`ShredVariant`] byte so that the two can never be confused on the wire.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum ShredType {
    /// Carries ledger entries.
    Data = 0b1010_0101,
    /// Carries Reed-Solomon erasure codes for the data shreds of its FEC set.
    Code = 0b0101_1010,
}

/// The kind of a shred plus the two layout bits that accompany it.
///
/// The high nibble identifies the variant, the low nibble carries `proof_size`:
///
/// ```text
/// 0b0110_pppp  Code
/// 0b0111_pppp  Code, resigned
/// 0b1001_pppp  Data
/// 0b1011_pppp  Data, resigned
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShredVariant {
    /// A code shred with `proof_size` Merkle proof entries.
    MerkleCode {
        /// Number of 20-byte Merkle proof entries in the trailer.
        proof_size: u8,
        /// Whether a retransmitter signature trails the proof.
        resigned: bool,
    },
    /// A data shred with `proof_size` Merkle proof entries.
    MerkleData {
        /// Number of 20-byte Merkle proof entries in the trailer.
        proof_size: u8,
        /// Whether a retransmitter signature trails the proof.
        resigned: bool,
    },
}

impl ShredVariant {
    /// Number of Merkle proof entries in the trailer.
    #[inline]
    pub const fn proof_size(self) -> u8 {
        match self {
            Self::MerkleCode { proof_size, .. } | Self::MerkleData { proof_size, .. } => proof_size,
        }
    }

    /// Whether a retransmitter signature trails the Merkle proof.
    #[inline]
    pub const fn resigned(self) -> bool {
        match self {
            Self::MerkleCode { resigned, .. } | Self::MerkleData { resigned, .. } => resigned,
        }
    }

    /// Whether this variant carries ledger data or erasure codes.
    #[inline]
    pub const fn shred_type(self) -> ShredType {
        match self {
            Self::MerkleCode { .. } => ShredType::Code,
            Self::MerkleData { .. } => ShredType::Data,
        }
    }
}

impl From<ShredVariant> for u8 {
    #[inline]
    fn from(variant: ShredVariant) -> u8 {
        match variant {
            ShredVariant::MerkleCode {
                proof_size,
                resigned: false,
            } => proof_size | 0x60,
            ShredVariant::MerkleCode {
                proof_size,
                resigned: true,
            } => proof_size | 0x70,
            ShredVariant::MerkleData {
                proof_size,
                resigned: false,
            } => proof_size | 0x90,
            ShredVariant::MerkleData {
                proof_size,
                resigned: true,
            } => proof_size | 0xb0,
        }
    }
}

impl TryFrom<u8> for ShredVariant {
    type Error = ParseError;

    #[inline]
    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        // The two legacy ShredType encodings must never be read as a variant.
        if byte == ShredType::Code as u8 || byte == ShredType::Data as u8 {
            return Err(ParseError::InvalidVariant(byte));
        }
        let proof_size = byte & 0x0f;
        match byte & 0xf0 {
            0x60 => Ok(Self::MerkleCode {
                proof_size,
                resigned: false,
            }),
            0x70 => Ok(Self::MerkleCode {
                proof_size,
                resigned: true,
            }),
            0x90 => Ok(Self::MerkleData {
                proof_size,
                resigned: false,
            }),
            0xb0 => Ok(Self::MerkleData {
                proof_size,
                resigned: true,
            }),
            _ => Err(ParseError::InvalidVariant(byte)),
        }
    }
}

// SAFETY: `TYPE_META` declares the single byte that `write` writes and `read` reads, and
// `zero_copy` is false because the nibble encoding has invalid bit patterns. `read` writes `dst`
// only on success.
unsafe impl<C: ConfigCore> SchemaWrite<C> for ShredVariant {
    type Src = Self;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 1,
        zero_copy: false,
    };

    fn size_of(_src: &Self::Src) -> wincode::WriteResult<usize> {
        Ok(1)
    }

    fn write(writer: impl Writer, src: &Self::Src) -> wincode::WriteResult<()> {
        <u8 as SchemaWrite<C>>::write(writer, &u8::from(*src))
    }
}

// SAFETY: see the `SchemaWrite` impl above.
unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for ShredVariant {
    type Dst = Self;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 1,
        zero_copy: false,
    };

    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> wincode::ReadResult<()> {
        let byte = <u8 as SchemaRead<C>>::get(reader)?;
        let variant = Self::try_from(byte)
            .map_err(|_| wincode::ReadError::InvalidTagEncoding(usize::from(byte)))?;
        dst.write(variant);
        Ok(())
    }
}
