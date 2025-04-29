use {crate::shred_variant::ShredKind, thiserror::Error};

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
    /// Some of the headers could not be deserialized.
    #[error(transparent)]
    Read(#[from] wincode::ReadError),
}
