//! The two shred kinds, and the layout constants that distinguish them.
//!
//! The kind is a type parameter of [`ShredView`](crate::view::ShredView) rather than a runtime tag,
//! so that the header it hands out is the one kind's own (`parent_offset` on data shreds,
//! `position` on code shreds).
//!
//! # Where the kind has to be a runtime tag
//!
//! [`AnyShredView`](crate::view::AnyShredView) holds the same shred with the kind's header
//! represented as an enum. Everything else about a shred is either common to both kinds or derived
//! from the variant byte.

use {
    crate::{
        constants::{
            self, SIZE_OF_CODE_HEADER, SIZE_OF_CODE_PAYLOAD, SIZE_OF_COMMON_HEADER,
            SIZE_OF_DATA_HEADER, SIZE_OF_DATA_PAYLOAD, SIZE_OF_TRAILER, SIZE_OF_TRAILER_RESIGNED,
        },
        error::ParseError,
        headers::{AnyHeader, CodeHeader, CommonHeader, DataHeader},
        shred_variant::ShredKind,
    },
    std::fmt::Debug,
    wincode::{SchemaRead, SchemaWrite, config::DefaultConfig},
};

mod sealed {
    pub trait Sealed {}
}

/// The layout and header type of one kind of shred, as type-level data.
pub trait ShredLayout: sealed::Sealed + 'static {
    /// The header this kind carries after the common header.
    ///
    /// [`Into<AnyHeader>`] is required so that kind-generic code can turn a view into the
    /// kind-erased [`AnyShredView`](crate::view::AnyShredView) without knowing which kind it holds.
    type Header: Copy
        + Debug
        + Into<AnyHeader>
        + for<'de> SchemaRead<'de, DefaultConfig, Dst = Self::Header>
        + SchemaWrite<DefaultConfig, Src = Self::Header>;

    /// The kind this layout corresponds to on the wire.
    const SHRED_KIND: ShredKind;
    /// Total on-the-wire length of a shred of this kind.
    const SIZE_OF_PAYLOAD: usize;
    /// Length of everything before the body: the signature, the common header and this kind's own.
    const SIZE_OF_HEADERS: usize;
    /// Where this kind's erasure-coded region starts.
    const ERASURE_SHARD_START: usize;
    /// Length of the body of a shred of this kind, which is what the headers and the trailer leave.
    const SIZE_OF_BODY: usize = Self::SIZE_OF_PAYLOAD - Self::SIZE_OF_HEADERS - SIZE_OF_TRAILER;
    /// Length of the body of a resigned shred, whose trailer is a retransmitter signature longer.
    const SIZE_OF_BODY_RESIGNED: usize =
        Self::SIZE_OF_PAYLOAD - Self::SIZE_OF_HEADERS - SIZE_OF_TRAILER_RESIGNED;

    /// Index of this shred's erasure shard within its FEC set, which is also the index of its leaf
    /// in the FEC set's Merkle tree. Data shards come first, then code shards.
    ///
    /// `None` if the headers describe no shard. [`check_header`](Self::check_header) rejects such
    /// headers, so it is never `None` for headers read through a [`ShredView`](crate::view::ShredView).
    fn erasure_shard_index(common: &CommonHeader, header: &Self::Header) -> Option<usize>;

    /// Checks what this kind's headers claim about the shard and about the bytes the layout leaves
    /// for it, given that `body`.
    ///
    /// Runs while the shred is being read, so it holds for every shred that exists, whatever door
    /// it came through: the wire, the blockstore, erasure recovery or this crate's own writer. That
    /// is what lets the kind-specific accessors below it be infallible.
    fn check_header(
        common: &CommonHeader,
        header: &Self::Header,
        body: &[u8],
    ) -> Result<(), ParseError>;
}

/// A shred carrying ledger entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Data;

/// A shred carrying Reed-Solomon erasure codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Code;

impl sealed::Sealed for Data {}
impl ShredLayout for Data {
    type Header = DataHeader;

    const SHRED_KIND: ShredKind = ShredKind::Data;
    const SIZE_OF_PAYLOAD: usize = SIZE_OF_DATA_PAYLOAD;
    const SIZE_OF_HEADERS: usize =
        constants::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER;
    // A data shred's own signature is not erasure coded; everything after it is.
    const ERASURE_SHARD_START: usize = constants::SIZE_OF_SIGNATURE;

    fn erasure_shard_index(common: &CommonHeader, _header: &DataHeader) -> Option<usize> {
        let shard = common.index.checked_sub(common.fec_set_index)?;
        usize::try_from(shard).ok()
    }

    /// The `size` field covers the headers as well as the ledger data, and whoever built the shred
    /// chose it, so it is checked against the layout here rather than trusted by whoever reads the
    /// data through [`data_len`]. Invalid flag combinations result in error.
    fn check_header(
        common: &CommonHeader,
        header: &DataHeader,
        body: &[u8],
    ) -> Result<(), ParseError> {
        if !header.flags.is_valid() {
            return Err(ParseError::InvalidShredFlags {
                flags: header.flags.bits(),
            });
        }
        if common.index < common.fec_set_index {
            return Err(ParseError::IndexBeforeFecSet {
                index: common.index,
                fec_set_index: common.fec_set_index,
            });
        }
        match data_len(header, body.len()) {
            Some(_) => Ok(()),
            None => Err(ParseError::InvalidDataSize { size: header.size }),
        }
    }
}

/// Length of the ledger data a data shred's header claims, or `None` if the claim does not describe
/// a region inside a body of `body_len` bytes.
///
/// Shared by [`Data::check_header`], which is where the `None` case is turned into a
/// [`ParseError`], and by readers of a data shred's ledger data, which is why that case cannot
/// happen for a shred read through a [`ShredView`](crate::view::ShredView).
pub fn data_len(header: &DataHeader, body_len: usize) -> Option<usize> {
    let len = usize::from(header.size).checked_sub(Data::SIZE_OF_HEADERS)?;
    if len > body_len {
        return None;
    }
    Some(len)
}

impl sealed::Sealed for Code {}
impl ShredLayout for Code {
    type Header = CodeHeader;

    const SHRED_KIND: ShredKind = ShredKind::Code;
    const SIZE_OF_PAYLOAD: usize = SIZE_OF_CODE_PAYLOAD;
    const SIZE_OF_HEADERS: usize =
        constants::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER;
    // Code shred headers cannot be erasure coded: the codes are generated before them.
    const ERASURE_SHARD_START: usize = Self::SIZE_OF_HEADERS;

    /// Both fields are `u16`, so their sum cannot leave the shard index's type. Whether the batch
    /// shape the header claims is allowed is a question about the batch, which is checked where one
    /// is assembled.
    fn erasure_shard_index(_common: &CommonHeader, header: &CodeHeader) -> Option<usize> {
        usize::from(header.num_data_shreds).checked_add(usize::from(header.position))
    }

    /// A code shred's body is its erasure codes, which the header claims nothing about. The
    /// position is checked against the index and the number of code shreds: the index minus the
    /// position is the FEC set's first code index, and a position past the last code shred
    /// describes no shard.
    fn check_header(
        common: &CommonHeader,
        header: &CodeHeader,
        _body: &[u8],
    ) -> Result<(), ParseError> {
        if common.index < u32::from(header.position) || header.position >= header.num_code_shreds {
            return Err(ParseError::InvalidCodePosition {
                index: common.index,
                position: header.position,
                num_code_shreds: header.num_code_shreds,
            });
        }
        Ok(())
    }
}
