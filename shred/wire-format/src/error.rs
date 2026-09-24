use {
    crate::shred_variant::{ShredKind, ShredVariant},
    thiserror::Error,
};

/// What can go wrong while parsing raw bytes as a shred.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The byte at offset 64 is not a valid [`ShredVariant`](crate::shred_variant::ShredVariant).
    #[error("invalid shred variant: {0:#04x}")]
    InvalidVariant(u8),

    /// Fewer bytes than the shred requires.
    #[error("shred is {len} bytes, expected at least {expected}")]
    TooShort {
        /// Number of bytes available.
        len: usize,
        /// Number of bytes the shred kind requires.
        expected: usize,
    },
    /// A buffer that must hold exactly one shred is not that long.
    #[error("shred buffer is {len} bytes, expected exactly {expected}")]
    WrongLength {
        /// Number of bytes available.
        len: usize,
        /// Number of bytes the shred kind requires.
        expected: usize,
    },
    /// The shred is followed by unexpected bytes.
    #[error("{0} trailing bytes after the shred")]
    TrailingBytes(usize),
    /// A repair response is not followed by exactly one nonce.
    #[error("repair response does not end in a nonce")]
    InvalidNonce,

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
    /// A data shred's flags set the slot-end bit without the FEC-set-end bit.
    #[error("data shred flags {flags:#010b} are not a defined combination")]
    InvalidShredFlags {
        /// The flag byte the data header carries.
        flags: u8,
    },
    /// A data shred's index is below its FEC set's first index.
    ///
    /// A shred's index minus its FEC set's is its erasure shard index, so an index below the set's
    /// describes no shard at all.
    #[error("data shred index {index} is below its FEC set index {fec_set_index}")]
    IndexBeforeFecSet {
        /// The index the common header claims.
        index: u32,
        /// The FEC set index the common header claims.
        fec_set_index: u32,
    },
    /// A code shred's position is not inside its FEC set's code shards.
    ///
    /// Data shards come first, so a code shred's index minus its position is the first code
    /// index of its FEC set, and the position must be below the number of code shreds.
    #[error(
        "code shred position {position} does not fit index {index} and {num_code_shreds} code \
         shreds"
    )]
    InvalidCodePosition {
        /// The index the common header claims.
        index: u32,
        /// The position the code header claims.
        position: u16,
        /// The number of code shreds the code header claims.
        num_code_shreds: u16,
    },
    /// Some of the headers could not be deserialized.
    #[error(transparent)]
    Read(#[from] wincode::ReadError),
}

/// What can go wrong while writing a shred's headers.
#[derive(Debug, Error)]
pub enum WriteError {
    /// The headers name a different variant than the one the buffer's layout was chosen for.
    ///
    /// Writing them anyway would leave every boundary after the body somewhere a reader of the
    /// variant byte does not look.
    #[error("headers carry variant {found:?}, the buffer is laid out for {expected:?}")]
    VariantMismatch {
        /// The variant the buffer was laid out for.
        expected: ShredVariant,
        /// The variant the headers carry.
        found: ShredVariant,
    },
    /// The headers could not be serialized.
    #[error(transparent)]
    Write(#[from] wincode::WriteError),
}
