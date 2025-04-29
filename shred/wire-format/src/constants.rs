//! Shred wire layout parameters.
//!
//! Every offset in the shred format is a function of the
//! [`ShredVariant`](crate::shred_variant::ShredVariant) byte. It defines whether it is a data or
//! code shred, and whether the shred is `resigned`.
//!
//! ```text
//! +------------+--------+--------+---------+----------+---------+-----------+
//! | signature  | common | kind's | body    | chained  | merkle  | [retrans  |
//! |            | header | header |         | merkle   | proof   |  mitter   |
//! |            |        |        |         | root     |         |  sig]     |
//! +------------+--------+--------+---------+----------+---------+-----------+
//!       64         19      5/6       (*)       32        120        [64]
//! ```
//!
//! `(*)` The body is whatever the fixed sections leave, so it is the one length that depends on
//! both inputs: [`SIZE_OF_BODY`](ShredLayout::SIZE_OF_BODY) and
//! [`SIZE_OF_BODY_RESIGNED`](ShredLayout::SIZE_OF_BODY_RESIGNED) per kind, four values in all. It
//! is a length, not a count of useful bytes; see `solana_shred::shred::DataShred::data` for what a
//! data shred's body actually holds. `README.md`'s section 7 tabulates all four.
//!
//! The wire format should be written down once, declaratively, in the order the bytes appear.
//! Deriving a single wincode schema for a whole shred is still not possible, because the two layouts
//! differ in the middle rather than only at the end. Instead, a single [`const fn`](sections) adds
//! the section sizes up in wire order, and every boundary is derived from it.
//!
//! The sizes below are read off the wincode schemas of the types that occupy each section, so the
//! shred's own header definitions are the only place they are stated. The `const_assert_eq!`s at the
//! bottom pin them: a schema change that moves a boundary is a protocol change, and a compile error.

use {
    crate::{
        headers::{CodeHeader, CommonHeader, DataHeader},
        kind::{Code, Data, ShredLayout},
    },
    bytes::{Bytes, BytesMut},
    solana_hash::Hash,
    solana_packet::PACKET_DATA_SIZE,
    solana_signature::Signature,
    std::ops::Range,
    wincode::{SchemaRead, TypeMeta, config::DefaultConfig},
};

/// The nonce a repair response carries after the shred, tying it to the request it answers.
pub type Nonce = u32;

/// Number of data shreds, and of code shreds, in every FEC set.
///
/// The erasure configuration is fixed at 32:32, which is what makes every offset below a constant
/// and the Merkle proof a fixed six entries deep. A shred claiming any other configuration
/// describes a batch shape that is not allowed.
pub const DATA_SHREDS_PER_FEC_BLOCK: u32 = 32;
/// Number of data shards in an erasure batch, as an index bound.
pub const DATA_SHREDS: usize = DATA_SHREDS_PER_FEC_BLOCK as usize;
/// Number of code shards in an erasure batch, which is the same as the number of data shards.
pub const CODE_SHREDS: usize = DATA_SHREDS;
/// Number of shards in an erasure batch, which is also the number of leaves in its Merkle tree.
pub const SHARDS: usize = DATA_SHREDS + CODE_SHREDS;

/// One entry of a Merkle proof, which is a hash truncated to its first 20 bytes.
///
/// The hashing itself is `solana-shred-verify`, which asserts that its own copy of this width
/// agrees with this one.
pub type ProofEntry = [u8; 20];
/// Size of one Merkle proof entry.
pub const SIZE_OF_MERKLE_PROOF_ENTRY: usize = size_of::<ProofEntry>();

/// The serialized size of `T`, as defined by wincode.
pub const fn serialized_size_of<T>() -> usize
where
    T: SchemaRead<'static, DefaultConfig>,
{
    match <T as SchemaRead<'static, DefaultConfig>>::TYPE_META {
        TypeMeta::Static { size, .. } => size,
        TypeMeta::Dynamic => panic!("shred sections are fixed-width, so their schemas must be too"),
    }
}

/// Size of the producing leader's signature.
pub const SIZE_OF_SIGNATURE: usize = serialized_size_of::<Signature>();
/// Size of the fixed header shared by both shred kinds, which follows the signature.
pub const SIZE_OF_COMMON_HEADER: usize = serialized_size_of::<CommonHeader>();
/// Size of the data-shred-specific header that follows the common header.
pub const SIZE_OF_DATA_HEADER: usize = serialized_size_of::<DataHeader>();
/// Size of the code-shred-specific header that follows the common header.
pub const SIZE_OF_CODE_HEADER: usize = serialized_size_of::<CodeHeader>();
/// Size of a Merkle root.
pub const SIZE_OF_MERKLE_ROOT: usize = serialized_size_of::<Hash>();
/// Number of Merkle proof entries in every valid shred.
///
/// A batch's Merkle tree has [`SHARDS`] leaves, so a proof for one of them is that many leaves'
/// worth of levels deep. A shred whose variant byte says otherwise describes a batch shape that is
/// not allowed, and is rejected rather than parsed.
pub const MERKLE_PROOF_ENTRIES: usize = SHARDS.next_power_of_two().ilog2() as usize;
/// Size of the Merkle proof.
pub const SIZE_OF_MERKLE_PROOF: usize = MERKLE_PROOF_ENTRIES * SIZE_OF_MERKLE_PROOF_ENTRY;
/// Size of everything that follows the body: the chained Merkle root and the proof.
pub const SIZE_OF_TRAILER: usize = SIZE_OF_MERKLE_ROOT + SIZE_OF_MERKLE_PROOF;
/// Size of the trailer of a resigned shred, which ends with a retransmitter signature.
pub const SIZE_OF_TRAILER_RESIGNED: usize = SIZE_OF_TRAILER + SIZE_OF_SIGNATURE;
/// Size of the repair nonce that may trail a shred in a repair response packet.
pub const SIZE_OF_NONCE: usize = serialized_size_of::<Nonce>();

/// Total wire size of a code shred.
///
/// This is the constant every other size in the shred format follows from, and the only one whose
/// cause is outside the format itself: a shred was designed to fit a packet that fits the minimum
/// IPv6 MTU, with room left for the nonce a repair response appends.
pub const SIZE_OF_CODE_PAYLOAD: usize = PACKET_DATA_SIZE - SIZE_OF_NONCE;
/// Total wire size of a data shred.
///
/// Code shreds erasure-code the entirety of a data shred except its signature, and the erasure
/// algorithm needs equal-length inputs, so a data shred is exactly a code shred's coded region
/// with a signature in front (signature is not covered by error correction as it is the same
/// for all shreds in a FEC set).
pub const SIZE_OF_DATA_PAYLOAD: usize =
    SIZE_OF_CODE_PAYLOAD - Code::SIZE_OF_HEADERS + SIZE_OF_SIGNATURE;

/// The size every shred buffer is allocated at, whichever kind it holds.
///
/// A code shred is the longer of the two payloads, and a repair response appends a nonce to
/// whatever it serves, so this is the most any one shred buffer ever has to hold. Allocating both
/// kinds at the same size gives the shred path a single allocation size, so a freed buffer can be
/// handed straight back out for the next shred either way, and leaves the nonce room to be written
/// where the payload already is instead of into a copy of it.
pub const SIZE_OF_SHRED_BUFFER: usize = SIZE_OF_CODE_PAYLOAD.saturating_add(SIZE_OF_NONCE);

/// Offset of the [`ShredVariant`](crate::shred_variant::ShredVariant) byte, which follows the
/// signature.
pub const OFFSET_OF_VARIANT: usize = SIZE_OF_SIGNATURE;

// this may be a bit excessive, but it is a tripwire in case of breaking changes in wincode
static_assertions::const_assert_eq!(MERKLE_PROOF_ENTRIES, 6);
static_assertions::const_assert_eq!(SIZE_OF_MERKLE_PROOF_ENTRY, 20);
static_assertions::const_assert_eq!(SIZE_OF_SIGNATURE, 64);
static_assertions::const_assert_eq!(SIZE_OF_NONCE, 4);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER, 19);
static_assertions::const_assert_eq!(SIZE_OF_MERKLE_ROOT, 32);
static_assertions::const_assert_eq!(SIZE_OF_TRAILER, 152);
static_assertions::const_assert_eq!(SIZE_OF_TRAILER_RESIGNED, 216);
static_assertions::const_assert_eq!(SIZE_OF_DATA_PAYLOAD, 1203);
static_assertions::const_assert_eq!(SIZE_OF_CODE_PAYLOAD, 1228);
static_assertions::const_assert_eq!(Data::SIZE_OF_HEADERS, 88);
static_assertions::const_assert_eq!(Code::SIZE_OF_HEADERS, 89);
// The four body sizes README.md's section 7 tabulates. Kept here so the one table this crate does
// not derive at compile time cannot go stale without the crate failing to build.
static_assertions::const_assert_eq!(Data::SIZE_OF_BODY, 963);
static_assertions::const_assert_eq!(Data::SIZE_OF_BODY_RESIGNED, 899);
static_assertions::const_assert_eq!(Code::SIZE_OF_BODY, 987);
static_assertions::const_assert_eq!(Code::SIZE_OF_BODY_RESIGNED, 923);
static_assertions::const_assert_eq!(SIZE_OF_SHRED_BUFFER, PACKET_DATA_SIZE);

/// A zeroed payload buffer for a shred of kind `K`, allocated at [`SIZE_OF_SHRED_BUFFER`].
///
/// Every shred this crate writes starts here, so the write path allocates one size and the spare
/// bytes past the payload are where [`repair_response`] puts the nonce.
pub fn payload_buffer<K: ShredLayout>() -> Vec<u8> {
    let mut payload = Vec::with_capacity(SIZE_OF_SHRED_BUFFER);
    payload.resize(K::SIZE_OF_PAYLOAD, 0);
    payload
}

/// Form the repair response: `payload` followed by the nonce of the request it answers.
///
/// The inverse of the split [`read_repair_packet`](crate::view::ShredView::read_repair_packet).
/// Neither signature covers the nonce, which is why appending it to a finished shred is sound.
/// Takes the payload by value so the nonce can be written into existing allocation (if it has room).
pub fn form_repair_response(payload: Bytes, nonce: Nonce) -> Bytes {
    let mut packet = payload.try_into_mut().unwrap_or_else(|payload| {
        let mut copy = BytesMut::with_capacity(payload.len().saturating_add(SIZE_OF_NONCE));
        copy.extend_from_slice(&payload);
        copy
    });
    packet.extend_from_slice(&nonce_bytes(nonce));
    packet.freeze()
}

/// A repair nonce as it appears on the wire.
pub fn nonce_bytes(nonce: Nonce) -> [u8; SIZE_OF_NONCE] {
    let mut bytes = [0u8; SIZE_OF_NONCE];
    wincode::serialize_into(bytes.as_mut_slice(), &nonce)
        .expect("a nonce fits the bytes its own schema asks for");
    bytes
}

/// A range over shred's bytes.
// Not a [`Range`], so that [`Sections`] can be `Copy`: these are boundaries, never iterated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Section {
    /// Offset of the section's first byte.
    pub start: usize,
    /// Offset one past the section's last byte.
    pub end: usize,
}

impl Section {
    /// Length of the section.
    #[inline]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the section is empty, which no section of a shred is. Exists because a type with
    /// `len` and no `is_empty` is a lint.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// Where each of a shred's sections lives in its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sections {
    /// The leader's signature.
    pub signature: Section,
    /// The common header and the kind's own header.
    pub headers: Section,
    /// Ledger data or erasure codes, zero padding included.
    pub body: Section,
    /// The Merkle root of the preceding erasure batch.
    pub chained_merkle_root: Section,
    /// The Merkle proof entries.
    pub merkle_proof: Section,
    /// The retransmitter signature, for resigned variants only.
    pub retransmitter_signature: Option<Section>,
    /// The region hashed into this shred's Merkle leaf.
    pub merkle_leaf: Section,
    /// The region the erasure coding covers.
    pub erasure_shard: Section,
}

/// The section layout of a shred of kind `K`
pub const fn sections<K: ShredLayout>(resigned: bool) -> Sections {
    let end_of_signature = SIZE_OF_SIGNATURE;
    let end_of_headers = K::SIZE_OF_HEADERS;
    let end_of_body = end_of_headers.saturating_add(if resigned {
        K::SIZE_OF_BODY_RESIGNED
    } else {
        K::SIZE_OF_BODY
    });
    let end_of_chained_merkle_root = end_of_body.saturating_add(SIZE_OF_MERKLE_ROOT);
    let end_of_merkle_proof = end_of_chained_merkle_root.saturating_add(SIZE_OF_MERKLE_PROOF);
    let retransmitter_signature = if resigned {
        Some(Section {
            start: end_of_merkle_proof,
            end: end_of_merkle_proof.saturating_add(SIZE_OF_SIGNATURE),
        })
    } else {
        None
    };
    Sections {
        signature: Section {
            start: 0,
            end: end_of_signature,
        },
        headers: Section {
            start: end_of_signature,
            end: end_of_headers,
        },
        body: Section {
            start: end_of_headers,
            end: end_of_body,
        },
        chained_merkle_root: Section {
            start: end_of_body,
            end: end_of_chained_merkle_root,
        },
        merkle_proof: Section {
            start: end_of_chained_merkle_root,
            end: end_of_merkle_proof,
        },
        retransmitter_signature,
        // The leaf covers everything the leader signs over: the headers, the body and the root it
        // chains to, but not the signature itself nor the proof that witnesses the leaf.
        merkle_leaf: Section {
            start: end_of_signature,
            end: end_of_chained_merkle_root,
        },
        // A data shred's own headers are erasure coded, a code shred's cannot be: the codes are
        // generated before the headers that describe them exist.
        erasure_shard: Section {
            start: K::ERASURE_SHARD_START,
            end: end_of_body,
        },
    }
}

/// The payload is exactly the sections, with nothing left over, for all four layouts.
const _: () = {
    const fn end_of_shred<K: ShredLayout>(resigned: bool) -> usize {
        let sections = sections::<K>(resigned);
        match sections.retransmitter_signature {
            Some(retransmitter_signature) => retransmitter_signature.end,
            None => sections.merkle_proof.end,
        }
    }
    assert!(end_of_shred::<Data>(false) == SIZE_OF_DATA_PAYLOAD);
    assert!(end_of_shred::<Data>(true) == SIZE_OF_DATA_PAYLOAD);
    assert!(end_of_shred::<Code>(false) == SIZE_OF_CODE_PAYLOAD);
    assert!(end_of_shred::<Code>(true) == SIZE_OF_CODE_PAYLOAD);
};

/// Both kinds' erasure-coded regions are the same length, which is what sets the two payload sizes
/// apart: Reed-Solomon needs equal-length shards, and a code shred spends on headers what a data
/// shred spends on its signature.
const _: () = {
    const fn erasure_shard_len<K: ShredLayout>(resigned: bool) -> usize {
        sections::<K>(resigned).erasure_shard.len()
    }
    assert!(erasure_shard_len::<Data>(false) == erasure_shard_len::<Code>(false));
    assert!(erasure_shard_len::<Data>(true) == erasure_shard_len::<Code>(true));
};

/// Every section is exactly as wide as the wincode schema of the type that occupies it, for all
/// four layouts.
const _: () = {
    const fn check_sections_match_schemas<K: ShredLayout>(resigned: bool) {
        let s = sections::<K>(resigned);
        assert!(s.signature.len() == serialized_size_of::<Signature>());
        assert!(s.headers.len() == serialized_size_of::<(CommonHeader, K::Header)>());
        assert!(s.chained_merkle_root.len() == serialized_size_of::<Hash>());
        assert!(s.merkle_proof.len() == serialized_size_of::<[ProofEntry; MERKLE_PROOF_ENTRIES]>());
        match s.retransmitter_signature {
            Some(section) => assert!(section.len() == serialized_size_of::<Signature>()),
            None => assert!(!resigned),
        }
    }
    check_sections_match_schemas::<Data>(false);
    check_sections_match_schemas::<Data>(true);
    check_sections_match_schemas::<Code>(false);
    check_sections_match_schemas::<Code>(true);
};
