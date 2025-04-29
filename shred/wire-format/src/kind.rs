//! The two shred kinds, and the layout constants that distinguish them.
//!
//! The kind is a type parameter of `solana_shred::shred::Shred` rather than a runtime tag, so
//! that accessors which only make sense for one kind (`parent_offset` on data shreds, `position` on
//! code shreds) simply do not exist on the other.
//!
//! # Where the kind has to be a runtime tag
//!
//! `solana_shred::shred::AnyShred` allows for the same shred with the header field represented
//! as an enum. Everything else about a shred is either common to both kinds or derived from the
//! variant byte.

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
    /// [`Into<AnyHeader>`] is required so that kind-generic code can hand a shred to the
    /// kind-erased `solana_shred::shred::AnyShred` without knowing which kind it holds.
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
    /// Infallible because [`check_header`](Self::check_header) rejects headers that describe no
    /// shard, so a shred that exists at all has one.
    fn erasure_shard_index(common: &CommonHeader, header: &Self::Header) -> usize;

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

    fn erasure_shard_index(common: &CommonHeader, _header: &DataHeader) -> usize {
        let shard = common
            .index
            .checked_sub(common.fec_set_index)
            .expect("checked while reading: a data shred's index is not below its FEC set's");
        usize::try_from(shard).expect("a u32 fits a usize on every target this runs on")
    }

    /// The `size` field covers the headers as well as the ledger data, and whoever built the shred
    /// chose it, so it is checked against the layout here rather than trusted by
    /// `solana_shred::shred::DataShred::data`. The index is checked against the FEC set's for the
    /// same reason: their difference is the shard index, which
    /// [`erasure_shard_index`](Self::erasure_shard_index) then reads off infallibly.
    fn check_header(
        common: &CommonHeader,
        header: &DataHeader,
        body: &[u8],
    ) -> Result<(), ParseError> {
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
/// [`ParseError`], and `solana_shred::shred::DataShred::data`, which is why that case cannot
/// happen.
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

    /// Both fields are `u16`, so their sum cannot leave the shard index's type. Whether it lands
    /// inside the batch is a question about the batch, which is checked where one is assembled.
    fn erasure_shard_index(_common: &CommonHeader, header: &CodeHeader) -> usize {
        usize::from(header.num_data_shreds).saturating_add(usize::from(header.position))
    }

    /// A code shred's body is its erasure codes, which the header claims nothing about.
    fn check_header(
        _common: &CommonHeader,
        _header: &CodeHeader,
        _body: &[u8],
    ) -> Result<(), ParseError> {
        Ok(())
    }
}
