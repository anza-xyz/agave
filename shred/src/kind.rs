//! The two shred kinds, and the layout constants that distinguish them.
//!
//! The kind is a type parameter of [`Shred`](crate::shred::Shred) rather than a runtime tag, so
//! that accessors which only make sense for one kind (`parent_offset` on data shreds, `position` on
//! code shreds) simply do not exist on the other, instead of returning an error at runtime. In
//! `ledger/src/shred.rs` that was a comment plus a `debug_assert`; here the wrong accessor is not
//! there to be called, and the layout differences between the kinds are resolved at compile time.
//!
//! # Where the kind has to be a runtime tag
//!
//! [`AnyShred`](crate::shred::AnyShred) is that form: the same shred with the header field erased
//! to an enum. Everything else about a shred is either common to both kinds or derived from the
//! variant byte, so erasing one field is enough, and the two kinds then differ by one discriminant.

use {
    crate::{
        error::{ParseError, Reject},
        headers::{AnyHeader, CodeHeader, CommonHeader, DataHeader},
        policy::{self, AdmissionPolicy, DATA_SHREDS_PER_FEC_BLOCK},
        shred_variant::ShredKind,
        wire_format::{
            self, SIZE_OF_CODE_HEADER, SIZE_OF_CODE_PAYLOAD, SIZE_OF_COMMON_HEADER,
            SIZE_OF_DATA_HEADER, SIZE_OF_DATA_PAYLOAD, SIZE_OF_TRAILER, SIZE_OF_TRAILER_RESIGNED,
        },
    },
    solana_clock::Slot,
    std::fmt::Debug,
    wincode::{SchemaRead, SchemaWrite, config::DefaultConfig},
};

mod sealed {
    pub trait Sealed {}
}

/// The layout and header type of one kind of shred, as type-level data. Sealed; the kinds are
/// exactly [`Data`] and [`Code`], and [`ShredKind`] is the same distinction as a runtime value.
pub trait ShredLayout: sealed::Sealed + 'static {
    /// The header this kind carries after the common header.
    ///
    /// [`Into<AnyHeader>`] is required so that kind-generic code can hand a shred to the
    /// kind-erased [`AnyShred`](crate::shred::AnyShred) without knowing which kind it holds.
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
    /// Returns `None` when the headers are mutually inconsistent.
    fn erasure_shard_index(common: &CommonHeader, header: &Self::Header) -> Option<usize>;

    /// Checks what this kind's header claims about the bytes the layout leaves for it, given that
    /// `body`.
    ///
    /// Runs while the shred is being read, so it holds for every shred that exists, whatever door
    /// it came through: the wire, the blockstore, erasure recovery or this crate's own writer. That
    /// is what lets the kind-specific accessors below it be infallible.
    fn check_header(header: &Self::Header, body: &[u8]) -> Result<(), ParseError>;

    /// Applies the admission checks that are specific to this kind.
    fn admit(
        common: &CommonHeader,
        header: &Self::Header,
        policy: &AdmissionPolicy,
    ) -> Result<(), Reject>;
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
        wire_format::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER;
    // A data shred's own signature is not erasure coded; everything after it is.
    const ERASURE_SHARD_START: usize = wire_format::SIZE_OF_SIGNATURE;

    fn erasure_shard_index(common: &CommonHeader, _header: &DataHeader) -> Option<usize> {
        usize::try_from(common.index.checked_sub(common.fec_set_index)?).ok()
    }

    /// The `size` field covers the headers as well as the ledger data, and whoever built the shred
    /// chose it, so it is checked against the layout here rather than trusted by
    /// [`data`](crate::shred::DataShred::data).
    fn check_header(header: &DataHeader, body: &[u8]) -> Result<(), ParseError> {
        match data_len(header, body.len()) {
            Some(_) => Ok(()),
            None => Err(ParseError::InvalidDataSize { size: header.size }),
        }
    }

    fn admit(
        common: &CommonHeader,
        header: &DataHeader,
        policy: &AdmissionPolicy,
    ) -> Result<(), Reject> {
        if !policy.is_data_index_in_bounds(common.index) {
            return Err(Reject::IndexOutOfBounds {
                index: common.index,
            });
        }
        let bad_parent = || Reject::BadParentOffset {
            slot: common.slot,
            parent_offset: header.parent_offset,
        };
        let parent = common
            .slot
            .checked_sub(Slot::from(header.parent_offset))
            .ok_or_else(bad_parent)?;
        if !policy.are_slots_chainable(common.slot, parent) {
            return Err(bad_parent());
        }
        // Under the fixed erasure configuration, an FEC set is complete exactly at its last index.
        let ends_fec_set = common
            .fec_set_index
            .checked_add(DATA_SHREDS_PER_FEC_BLOCK)
            .and_then(|end| end.checked_sub(1))
            == Some(common.index);
        if header.flags.data_complete() && !ends_fec_set {
            return Err(Reject::UnexpectedDataCompleteShred);
        }
        if header.flags.last_in_slot() && !policy::can_end_slot(common.index) {
            return Err(Reject::MisalignedLastDataIndex);
        }
        Ok(())
    }
}

/// Length of the ledger data a data shred's header claims, or `None` if the claim does not describe
/// a region inside a body of `body_len` bytes.
///
/// Shared by [`Data::check_header`], which is where the `None` case is turned into a
/// [`ParseError`], and [`data`](crate::shred::DataShred::data), which is why that case cannot
/// happen.
pub(crate) fn data_len(header: &DataHeader, body_len: usize) -> Option<usize> {
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
        wire_format::SIZE_OF_SIGNATURE + SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER;
    // Code shred headers cannot be erasure coded: the codes are generated before them.
    const ERASURE_SHARD_START: usize = Self::SIZE_OF_HEADERS;

    fn erasure_shard_index(_common: &CommonHeader, header: &CodeHeader) -> Option<usize> {
        usize::from(header.num_data_shreds).checked_add(usize::from(header.position))
    }

    /// A code shred's body is its erasure codes, which the header claims nothing about.
    fn check_header(_header: &CodeHeader, _body: &[u8]) -> Result<(), ParseError> {
        Ok(())
    }

    fn admit(
        common: &CommonHeader,
        header: &CodeHeader,
        policy: &AdmissionPolicy,
    ) -> Result<(), Reject> {
        if !policy.is_code_index_in_bounds(common.index) {
            return Err(Reject::IndexOutOfBounds {
                index: common.index,
            });
        }
        if common.slot <= policy.root {
            return Err(Reject::SlotOutOfRange { slot: common.slot });
        }
        let fixed = u32::from(header.num_data_shreds) == DATA_SHREDS_PER_FEC_BLOCK
            && u32::from(header.num_code_shreds) == DATA_SHREDS_PER_FEC_BLOCK;
        if !fixed {
            return Err(Reject::MisalignedErasureConfig {
                num_data_shreds: header.num_data_shreds,
                num_code_shreds: header.num_code_shreds,
            });
        }
        // `position` is what places this shred's leaf in its FEC set's Merkle tree, so a value that
        // does not agree with the shred's own index would prove a leaf of a batch this shred does
        // not belong to. The two counters advance together under the fixed configuration, so the
        // position is the distance from the FEC set index.
        let position = u32::from(header.position);
        let aligned = position < DATA_SHREDS_PER_FEC_BLOCK
            && common.index.checked_sub(common.fec_set_index) == Some(position);
        if !aligned {
            return Err(Reject::MisalignedCodePosition {
                index: common.index,
                fec_set_index: common.fec_set_index,
                position: header.position,
            });
        }
        Ok(())
    }
}
