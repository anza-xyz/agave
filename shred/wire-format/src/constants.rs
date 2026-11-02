//! Shred wire layout parameters.
//!
//! Every offset in the shred format is a function of the
//! [`ShredVariant`] byte. It defines whether it is a data or
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
//! `(*)` The body is what the fixed sections leave. A data shred's body ends in zero padding;
//! [`DataHeader::size`] says where the useful bytes stop.
//!
//! Section sizes come from the wincode schemas of the types occupying them. The
//! `const_assert_eq!`s below pin them, since moving a boundary is a protocol change.

use {
    crate::{
        headers::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
        kind::{Code, Data, ShredLayout},
        shred_variant::ShredVariant,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_packet::PACKET_DATA_SIZE,
    solana_signature::Signature,
    std::ops::Range,
    wincode::{SchemaRead, TypeMeta, config::DefaultConfig},
};

/// Repair response nonce, appended after the shred.
pub type Nonce = u32;

/// Number of data shreds, and of code shreds, in every FEC set (fixed 32:32).
pub const DATA_SHREDS_PER_FEC_BLOCK: u32 = 32;
pub const DATA_SHREDS: usize = DATA_SHREDS_PER_FEC_BLOCK as usize;
pub const CODE_SHREDS: usize = DATA_SHREDS;
/// Shards per erasure batch, also the number of Merkle tree leaves.
pub const SHARDS: usize = DATA_SHREDS + CODE_SHREDS;

/// One Merkle proof entry: a hash truncated to 20 bytes.
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

pub const SIZE_OF_SIGNATURE: usize = serialized_size_of::<Signature>();
pub const SIZE_OF_COMMON_HEADER: usize = serialized_size_of::<CommonHeader>();
pub const SIZE_OF_DATA_HEADER: usize = serialized_size_of::<DataHeader>();
pub const SIZE_OF_CODE_HEADER: usize = serialized_size_of::<CodeHeader>();
pub const SIZE_OF_MERKLE_ROOT: usize = serialized_size_of::<Hash>();
/// Merkle proof depth for a tree of [`SHARDS`] leaves.
pub const MERKLE_PROOF_ENTRIES: usize = SHARDS.next_power_of_two().ilog2() as usize;
pub const SIZE_OF_MERKLE_PROOF: usize = MERKLE_PROOF_ENTRIES * SIZE_OF_MERKLE_PROOF_ENTRY;
/// Everything after the body: chained Merkle root and proof.
pub const SIZE_OF_TRAILER: usize = SIZE_OF_MERKLE_ROOT + SIZE_OF_MERKLE_PROOF;
/// Trailer plus the retransmitter signature.
pub const SIZE_OF_TRAILER_RESIGNED: usize = SIZE_OF_TRAILER + SIZE_OF_SIGNATURE;
pub const SIZE_OF_NONCE: usize = serialized_size_of::<Nonce>();

/// Total wire size of a code shred: a packet minus room for the repair nonce.
pub const SIZE_OF_CODE_PAYLOAD: usize = PACKET_DATA_SIZE - SIZE_OF_NONCE;
/// Total wire size of a data shred.
///
/// Erasure coding covers the data shred's headers and body. It excludes the leader's signature,
/// chained Merkle root, Merkle proof, and optional retransmitter signature. Reed-Solomon needs
/// equal-length shards, so a data shred spends on its signature what a code shred spends on its
/// headers.
pub const SIZE_OF_DATA_PAYLOAD: usize =
    SIZE_OF_CODE_PAYLOAD - Code::SIZE_OF_HEADERS + SIZE_OF_SIGNATURE;

// Header field offsets: running sums of the wincode sizes of the preceding field types.

/// Offset of [`CommonHeader::variant`].
pub const OFFSET_OF_VARIANT: usize = SIZE_OF_SIGNATURE;
/// Offset of [`CommonHeader::slot`].
pub const OFFSET_OF_SLOT: usize = OFFSET_OF_VARIANT + serialized_size_of::<ShredVariant>();
/// Offset of [`CommonHeader::index`].
pub const OFFSET_OF_INDEX: usize = OFFSET_OF_SLOT + serialized_size_of::<Slot>();
/// Offset of [`CommonHeader::version`].
pub const OFFSET_OF_VERSION: usize = OFFSET_OF_INDEX + serialized_size_of::<u32>();
/// Offset of [`CommonHeader::fec_set_index`].
pub const OFFSET_OF_FEC_SET_INDEX: usize = OFFSET_OF_VERSION + serialized_size_of::<u16>();
/// Offset of the kind-specific header.
pub const OFFSET_OF_KIND_HEADER: usize = OFFSET_OF_FEC_SET_INDEX + serialized_size_of::<u32>();

/// Offset of [`DataHeader::parent_offset`].
pub const OFFSET_OF_PARENT_OFFSET: usize = OFFSET_OF_KIND_HEADER;
/// Offset of [`DataHeader::flags`].
pub const OFFSET_OF_FLAGS: usize = OFFSET_OF_PARENT_OFFSET + serialized_size_of::<u16>();
/// Offset of [`DataHeader::size`].
pub const OFFSET_OF_DATA_SIZE: usize = OFFSET_OF_FLAGS + serialized_size_of::<ShredFlags>();

/// Offset of [`CodeHeader::num_data_shreds`].
pub const OFFSET_OF_NUM_DATA_SHREDS: usize = OFFSET_OF_KIND_HEADER;
/// Offset of [`CodeHeader::num_code_shreds`].
pub const OFFSET_OF_NUM_CODE_SHREDS: usize =
    OFFSET_OF_NUM_DATA_SHREDS + serialized_size_of::<u16>();
/// Offset of [`CodeHeader::position`].
pub const OFFSET_OF_POSITION: usize = OFFSET_OF_NUM_CODE_SHREDS + serialized_size_of::<u16>();

// Tripwire for protocol or wincode changes.
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
static_assertions::const_assert_eq!(Data::SIZE_OF_BODY, 963);
static_assertions::const_assert_eq!(Data::SIZE_OF_BODY_RESIGNED, 899);
static_assertions::const_assert_eq!(Code::SIZE_OF_BODY, 987);
static_assertions::const_assert_eq!(Code::SIZE_OF_BODY_RESIGNED, 923);

// Each header's field offsets end exactly at the header's end.
static_assertions::const_assert_eq!(
    OFFSET_OF_KIND_HEADER,
    SIZE_OF_SIGNATURE.saturating_add(SIZE_OF_COMMON_HEADER)
);
static_assertions::const_assert_eq!(
    OFFSET_OF_DATA_SIZE.saturating_add(serialized_size_of::<u16>()),
    Data::SIZE_OF_HEADERS
);
static_assertions::const_assert_eq!(
    OFFSET_OF_POSITION.saturating_add(serialized_size_of::<u16>()),
    Code::SIZE_OF_HEADERS
);
static_assertions::const_assert_eq!(OFFSET_OF_VARIANT, 64);
static_assertions::const_assert_eq!(OFFSET_OF_SLOT, 65);
static_assertions::const_assert_eq!(OFFSET_OF_INDEX, 73);
static_assertions::const_assert_eq!(OFFSET_OF_VERSION, 77);
static_assertions::const_assert_eq!(OFFSET_OF_FEC_SET_INDEX, 79);
static_assertions::const_assert_eq!(OFFSET_OF_KIND_HEADER, 83);
static_assertions::const_assert_eq!(OFFSET_OF_PARENT_OFFSET, 83);
static_assertions::const_assert_eq!(OFFSET_OF_FLAGS, 85);
static_assertions::const_assert_eq!(OFFSET_OF_DATA_SIZE, 86);
static_assertions::const_assert_eq!(OFFSET_OF_NUM_DATA_SHREDS, 83);
static_assertions::const_assert_eq!(OFFSET_OF_NUM_CODE_SHREDS, 85);
static_assertions::const_assert_eq!(OFFSET_OF_POSITION, 87);

/// A byte range within a shred.
// Not a [`Range`], so that [`Sections`] can be `Copy`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Section {
    pub start: usize,
    pub end: usize,
}

impl Section {
    #[inline]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

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
    pub signature: Section,
    /// Common header and the kind's own header.
    pub headers: Section,
    /// Ledger data or erasure codes, zero padding included.
    pub body: Section,
    /// Merkle root of the preceding erasure batch.
    pub chained_merkle_root: Section,
    pub merkle_proof: Section,
    /// Present for resigned variants only.
    pub retransmitter_signature: Option<Section>,
    /// Region hashed into this shred's Merkle leaf.
    pub merkle_leaf: Section,
    /// Region covered by erasure coding.
    pub erasure_shard: Section,
}

/// Fixed-depth layout, used only by the `const` checks below.
const fn sections<K: ShredLayout>(resigned: bool) -> Sections {
    match sections_with_proof_entries::<K>(MERKLE_PROOF_ENTRIES, resigned) {
        Some(sections) => sections,
        None => panic!("the fixed proof length leaves a body in every payload, as asserted below"),
    }
}

/// Section layout of a shred of kind `K` with `proof_entries` proof entries, or `None` if the proof
/// leaves no room for a body.
///
/// Variable depth is needed while SIMD-317 enforcement of 32:32 batches is slot-gated.
//TODO: Replace with a fixed-depth layout once SIMD-317 enforcement is unconditional, and delete
// `solana-ledger`'s `test_sections_match_legacy`.
pub const fn sections_with_proof_entries<K: ShredLayout>(
    proof_entries: usize,
    resigned: bool,
) -> Option<Sections> {
    let Some(size_of_merkle_proof) = proof_entries.checked_mul(SIZE_OF_MERKLE_PROOF_ENTRY) else {
        return None;
    };
    let end_of_signature = SIZE_OF_SIGNATURE;
    let end_of_headers = K::SIZE_OF_HEADERS;
    let size_of_trailer = SIZE_OF_MERKLE_ROOT
        .saturating_add(size_of_merkle_proof)
        .saturating_add(if resigned { SIZE_OF_SIGNATURE } else { 0 });
    let end_of_body = match K::SIZE_OF_PAYLOAD.checked_sub(size_of_trailer) {
        Some(end_of_body) if end_of_body >= end_of_headers => end_of_body,
        _ => return None,
    };
    let end_of_chained_merkle_root = end_of_body.saturating_add(SIZE_OF_MERKLE_ROOT);
    let end_of_merkle_proof = end_of_chained_merkle_root.saturating_add(size_of_merkle_proof);
    let retransmitter_signature = if resigned {
        Some(Section {
            start: end_of_merkle_proof,
            end: end_of_merkle_proof.saturating_add(SIZE_OF_SIGNATURE),
        })
    } else {
        None
    };
    Some(Sections {
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
        merkle_leaf: Section {
            start: end_of_signature,
            end: end_of_chained_merkle_root,
        },
        erasure_shard: Section {
            start: K::ERASURE_SHARD_START,
            end: end_of_body,
        },
    })
}

// Sections cover the whole payload.
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

// Body lengths agree with `ShredLayout`.
const _: () = {
    const fn check_body_len<K: ShredLayout>() {
        assert!(sections::<K>(false).body.len() == K::SIZE_OF_BODY);
        assert!(sections::<K>(true).body.len() == K::SIZE_OF_BODY_RESIGNED);
    }
    check_body_len::<Data>();
    check_body_len::<Code>();
};

// Reed-Solomon needs equal-length shards.
const _: () = {
    const fn erasure_shard_len<K: ShredLayout>(resigned: bool) -> usize {
        sections::<K>(resigned).erasure_shard.len()
    }
    assert!(erasure_shard_len::<Data>(false) == erasure_shard_len::<Code>(false));
    assert!(erasure_shard_len::<Data>(true) == erasure_shard_len::<Code>(true));
};

// Section widths match their wincode schemas.
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
