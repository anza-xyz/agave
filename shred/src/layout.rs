//! Sizes of the shred's fixed-width sections.
//!
//! Every offset in the shred format is a function of three bits of information, all of which live
//! in the [`ShredVariant`](crate::ShredVariant) byte at offset 64: the kind (data or code), the
//! Merkle `proof_size`, and whether the shred is `resigned`. Nothing here spells an offset out:
//! [`ShredView`](crate::ShredView) walks the sections in wire order and lets the reader's cursor be
//! the offset.
//!
//! ```text
//! +------------+---+--------+---------+----------+---------+-----------+
//! | signature  | v | header | body    | chained  | merkle  | [retrans  |
//! |            | a |        |         | merkle   | proof   |  mitter   |
//! |            | r |        |         | root     |         |  sig]     |
//! +------------+---+--------+---------+----------+---------+-----------+
//! ```
//!
//! The sizes below are read off the wincode schemas of the types that occupy each section, so the
//! shred's own header definitions are the only place they are stated. The `const_assert_eq!`s at
//! the bottom pin them to the numbers in `README.md`: a schema change that moves a boundary is a
//! protocol change, and fails the build here.

use {
    crate::header::{CodeHeader, CommonHeader, DataHeader},
    solana_hash::Hash,
    solana_signature::Signature,
    wincode::{SchemaRead, TypeMeta, config::DefaultConfig},
};

/// One entry of a Merkle proof: the 20-byte prefix of a node hash.
pub type ProofEntry = [u8; SIZE_OF_MERKLE_PROOF_ENTRY];

/// The serialized size of `T`, which must have a statically known one.
pub const fn size_of_schema<T>() -> usize
where
    T: SchemaRead<'static, DefaultConfig>,
{
    match <T as SchemaRead<'static, DefaultConfig>>::TYPE_META {
        TypeMeta::Static { size, .. } => size,
        TypeMeta::Dynamic => panic!("shred sections are fixed-width, so their schemas must be too"),
    }
}

/// Size of the leader's signature, which occupies the first bytes of every shred.
pub const SIZE_OF_SIGNATURE: usize = size_of_schema::<Signature>();
/// Size of the header shared by both shred kinds, signature included.
pub const SIZE_OF_COMMON_HEADER: usize = SIZE_OF_SIGNATURE + size_of_schema::<CommonHeader>();
/// Size of the data-shred-specific header that follows the common header.
pub const SIZE_OF_DATA_HEADER: usize = size_of_schema::<DataHeader>();
/// Size of the code-shred-specific header that follows the common header.
pub const SIZE_OF_CODE_HEADER: usize = size_of_schema::<CodeHeader>();
/// Size of a Merkle root.
pub const SIZE_OF_MERKLE_ROOT: usize = size_of_schema::<Hash>();
/// Size of one entry of a Merkle proof.
pub const SIZE_OF_MERKLE_PROOF_ENTRY: usize = 20;
/// Size of the repair nonce that may trail a shred in a repair response packet.
pub const SIZE_OF_NONCE: usize = 4;

/// Total on-the-wire size of a code shred, which is one packet minus the repair nonce.
pub const SIZE_OF_CODE_PAYLOAD: usize = 1228;
/// Total on-the-wire size of a data shred.
///
/// Code shreds erasure-code the entirety of a data shred except its signature, and the erasure
/// algorithm needs equal-length inputs, so a data shred is exactly a code shred's coded region
/// with a signature in front.
pub const SIZE_OF_DATA_PAYLOAD: usize =
    SIZE_OF_CODE_PAYLOAD - (SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER) + SIZE_OF_SIGNATURE;

/// Offset of the [`ShredVariant`](crate::ShredVariant) byte, which follows the signature.
///
/// The one offset the crate names, because peeking at the variant is what selects a kind before
/// there is anything to walk sections with.
pub const OFFSET_OF_VARIANT: usize = SIZE_OF_SIGNATURE;

static_assertions::const_assert_eq!(SIZE_OF_SIGNATURE, 64);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER, 83);
static_assertions::const_assert_eq!(SIZE_OF_MERKLE_ROOT, 32);
static_assertions::const_assert_eq!(SIZE_OF_DATA_PAYLOAD, 1203);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER, 88);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER, 89);
