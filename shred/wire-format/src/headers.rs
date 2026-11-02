//! The header structs, as read by wincode.

use {
    crate::{
        constants::{OFFSET_OF_KIND_HEADER, OFFSET_OF_VARIANT},
        error::ParseError,
        kind::ShredLayout,
        shred_variant::ShredVariant,
    },
    solana_clock::Slot,
    wincode::{SchemaRead, SchemaWrite},
};

/// Header common to both shred kinds. Excludes the leading signature (bytes `0..64`).
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

/// Data shred flags: two completion markers and a 6-bit reference tick.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, SchemaRead, SchemaWrite)]
#[repr(transparent)]
pub struct ShredFlags(u8);

// TODO: only needed for migration, inline later.
#[inline]
pub fn peek_variant_byte(bytes: &[u8]) -> Result<u8, ParseError> {
    let Some(&byte) = bytes.get(OFFSET_OF_VARIANT) else {
        return Err(ParseError::TooShort {
            len: bytes.len(),
            expected: OFFSET_OF_VARIANT.saturating_add(1),
        });
    };
    Ok(byte)
}

/// Reads the kind-specific header. The caller must check the shred is of kind `K`.
//TODO: only needed for migration, delete after.
#[inline]
pub fn peek_header<K: ShredLayout>(bytes: &[u8]) -> Result<K::Header, ParseError> {
    let Some(mut header) = bytes.get(OFFSET_OF_KIND_HEADER..K::SIZE_OF_HEADERS) else {
        return Err(ParseError::TooShort {
            len: bytes.len(),
            expected: K::SIZE_OF_HEADERS,
        });
    };
    Ok(K::Header::get(&mut header)?)
}
