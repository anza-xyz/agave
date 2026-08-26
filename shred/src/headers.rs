//! The header structs, as read by wincode.
//!
//! The leader's signature occupies bytes `0..64` and is deliberately absent from [`CommonHeader`]:
//! it is 64 of the 88 (data) or 89 (code) header bytes, only sigverify needs it, and it can be
//! handed out as a zero-copy reference into the shred instead of being copied at parse time.
//! Deserialization therefore starts at
//! [`OFFSET_OF_VARIANT`](crate::wire_format::OFFSET_OF_VARIANT).

use {
    crate::shred_variant::ShredVariant,
    solana_clock::Slot,
    wincode::{SchemaRead, SchemaWrite},
};

/// A kind's own header, with the kind as a runtime tag.
///
/// This is what makes a kind-erased shred possible: erasing the header field is enough, because
/// everything else about a shred is either common to both kinds or derived from the variant byte.
/// See [`AnyShred`](crate::shred::AnyShred).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnyHeader {
    /// A data shred's header.
    Data(DataHeader),
    /// A code shred's header.
    Code(CodeHeader),
}

impl From<DataHeader> for AnyHeader {
    #[inline]
    fn from(header: DataHeader) -> Self {
        Self::Data(header)
    }
}

impl From<CodeHeader> for AnyHeader {
    #[inline]
    fn from(header: CodeHeader) -> Self {
        Self::Code(header)
    }
}

/// The part of the header that is common to both shred kinds, signature excluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, SchemaRead, SchemaWrite)]
pub struct CommonHeader {
    /// Selects the shred's kind and the layout of its trailer.
    pub variant: ShredVariant,
    /// Slot this shred belongs to.
    pub slot: Slot,
    /// Index of this shred within its slot, counted separately per kind.
    pub index: u32,
    /// Cluster shred version, derived from the genesis hash and the cluster's hard forks.
    pub version: u16,
    /// Index of the first data shred of this shred's FEC set.
    pub fec_set_index: u32,
}

/// The header carried only by data shreds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, SchemaRead, SchemaWrite)]
pub struct DataHeader {
    /// Distance in slots back to the parent slot.
    pub parent_offset: u16,
    /// Reference tick and the FEC-set / slot completion markers.
    pub flags: ShredFlags,
    /// Length of the headers plus the meaningful ledger data, padding excluded.
    pub size: u16,
}

/// The header carried only by code shreds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, SchemaRead, SchemaWrite)]
pub struct CodeHeader {
    /// Number of data shreds in this FEC set.
    pub num_data_shreds: u16,
    /// Number of code shreds in this FEC set.
    pub num_code_shreds: u16,
    /// Position of this shred among the FEC set's code shreds.
    pub position: u16,
}

/// Data shred flags: a 6-bit reference tick plus two completion markers.
///
/// Every one of the 256 byte values is a valid combination, so reading these flags cannot fail.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, SchemaRead, SchemaWrite)]
#[repr(transparent)]
pub struct ShredFlags(u8);

impl ShredFlags {
    /// Mask of the bits holding the reference tick.
    pub const REFERENCE_TICK_MASK: u8 = 0b0011_1111;
    /// Marks the last data shred of an FEC set.
    pub const DATA_COMPLETE_SHRED: u8 = 0b0100_0000;
    /// Marks the last data shred of a slot. Implies [`Self::DATA_COMPLETE_SHRED`].
    pub const LAST_SHRED_IN_SLOT: u8 = 0b1100_0000;

    /// The reference tick, saturated at [`Self::REFERENCE_TICK_MASK`] by the sender.
    #[inline]
    pub const fn reference_tick(self) -> u8 {
        self.0 & Self::REFERENCE_TICK_MASK
    }

    /// Whether this is the last data shred of its FEC set.
    #[inline]
    pub const fn data_complete(self) -> bool {
        self.0 & Self::DATA_COMPLETE_SHRED == Self::DATA_COMPLETE_SHRED
    }

    /// Whether this is the last data shred of its slot.
    #[inline]
    pub const fn last_in_slot(self) -> bool {
        self.0 & Self::LAST_SHRED_IN_SLOT == Self::LAST_SHRED_IN_SLOT
    }
}

impl From<u8> for ShredFlags {
    #[inline]
    fn from(bits: u8) -> Self {
        Self(bits)
    }
}
