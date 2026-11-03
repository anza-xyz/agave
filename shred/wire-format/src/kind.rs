//! The two shred kinds, and the layout constants that distinguish them.

use {
    crate::{
        constants::{
            self, SIZE_OF_CODE_HEADER, SIZE_OF_CODE_PAYLOAD, SIZE_OF_COMMON_HEADER,
            SIZE_OF_DATA_HEADER, SIZE_OF_DATA_PAYLOAD, SIZE_OF_TRAILER, SIZE_OF_TRAILER_RESIGNED,
        },
        headers::{CodeHeader, DataHeader},
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
    type Header: Copy
        + Debug
        + for<'de> SchemaRead<'de, DefaultConfig, Dst = Self::Header>
        + SchemaWrite<DefaultConfig, Src = Self::Header>;

    /// Wire length of a shred of this kind.
    const SIZE_OF_PAYLOAD: usize;
    /// Signature plus common and kind headers.
    const SIZE_OF_HEADERS: usize;
    const ERASURE_SHARD_START: usize;
    const SIZE_OF_BODY: usize = Self::SIZE_OF_PAYLOAD - Self::SIZE_OF_HEADERS - SIZE_OF_TRAILER;
    const SIZE_OF_BODY_RESIGNED: usize =
        Self::SIZE_OF_PAYLOAD - Self::SIZE_OF_HEADERS - SIZE_OF_TRAILER_RESIGNED;
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

    const SIZE_OF_PAYLOAD: usize = SIZE_OF_DATA_PAYLOAD;
    const SIZE_OF_HEADERS: usize =
        constants::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER;
    // The erasure-coded region starts after the leader's signature and ends before the chained
    // Merkle root.
    const ERASURE_SHARD_START: usize = constants::SIZE_OF_SIGNATURE;
}

impl sealed::Sealed for Code {}
impl ShredLayout for Code {
    type Header = CodeHeader;

    const SIZE_OF_PAYLOAD: usize = SIZE_OF_CODE_PAYLOAD;
    const SIZE_OF_HEADERS: usize =
        constants::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER;
    // Code shred headers cannot be erasure coded: the codes are generated before them.
    const ERASURE_SHARD_START: usize = Self::SIZE_OF_HEADERS;
}
