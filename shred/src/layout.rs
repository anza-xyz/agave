//! Byte layout of a shred, and the arithmetic that derives it.
//!
//! Every offset in the shred format is a function of three bits of information, all of which live
//! in the [`ShredVariant`](crate::ShredVariant) byte at offset 64: the kind (data or code), the
//! Merkle `proof_size`, and whether the shred is `resigned`. This module is the only place that
//! arithmetic is written.
//!
//! ```text
//! 0            64  65      83/84     body_end   proof     proof_end   payload_len
//! +------------+---+--------+---------+----------+---------+-----------+
//! | signature  | v | header | body    | chained  | merkle  | [retrans  |
//! |            | a |        |         | merkle   | proof   |  mitter   |
//! |            | r |        |         | root     |         |  sig]     |
//! +------------+---+--------+---------+----------+---------+-----------+
//! ```

use std::ops::Range;

/// Size of an Ed25519 signature; the leader's signature occupies the first 64 bytes of every shred.
pub const SIZE_OF_SIGNATURE: usize = 64;
/// Size of the header shared by both shred kinds, signature included.
pub const SIZE_OF_COMMON_HEADER: usize = 83;
/// Size of the data-shred-specific header that follows the common header.
pub const SIZE_OF_DATA_HEADER: usize = 5;
/// Size of the code-shred-specific header that follows the common header.
pub const SIZE_OF_CODE_HEADER: usize = 6;
/// Size of a Merkle root, i.e. of a [`solana_hash::Hash`].
pub const SIZE_OF_MERKLE_ROOT: usize = 32;
/// Size of one entry of a Merkle proof: the 20-byte prefix of a node hash.
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
pub const OFFSET_OF_VARIANT: usize = SIZE_OF_SIGNATURE;

static_assertions::const_assert_eq!(SIZE_OF_DATA_PAYLOAD, 1203);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER, 88);
static_assertions::const_assert_eq!(SIZE_OF_COMMON_HEADER + SIZE_OF_CODE_HEADER, 89);

/// Resolved byte boundaries of a single shred.
///
/// Constructed by [`Layout::try_new`] from a kind's constants plus the `proof_size` and `resigned`
/// bits of its variant. Every boundary is computed with checked arithmetic there and stored, so a
/// `Layout` that constructs successfully describes ranges that are all in bounds of the shred and
/// every accessor below is a field read that cannot overflow or panic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    headers_end: usize,
    body_end: usize,
    proof_start: usize,
    proof_end: usize,
    retransmitter_signature_start: Option<usize>,
    erasure_shard_start: usize,
    capacity: usize,
    payload_len: usize,
}

impl Layout {
    /// Derives the layout, or returns `None` when `proof_size` is too large for the payload to
    /// hold the headers, the chained Merkle root, the proof and the optional retransmitter
    /// signature.
    ///
    /// `proof_size` comes off the wire, so every step here is checked.
    pub(crate) const fn try_new(
        payload_len: usize,
        size_of_headers: usize,
        erasure_shard_start: usize,
        proof_size: u8,
        resigned: bool,
    ) -> Option<Self> {
        let Some(proof_len) = (proof_size as usize).checked_mul(SIZE_OF_MERKLE_PROOF_ENTRY) else {
            return None;
        };
        let signature_len = if resigned { SIZE_OF_SIGNATURE } else { 0 };
        // headers | body | chained merkle root | proof | [retransmitter signature]
        let Some(fixed) = checked_sum(&[
            size_of_headers,
            SIZE_OF_MERKLE_ROOT,
            proof_len,
            signature_len,
        ]) else {
            return None;
        };
        let Some(capacity) = payload_len.checked_sub(fixed) else {
            return None;
        };
        let Some(body_end) = size_of_headers.checked_add(capacity) else {
            return None;
        };
        let Some(proof_start) = body_end.checked_add(SIZE_OF_MERKLE_ROOT) else {
            return None;
        };
        let Some(proof_end) = proof_start.checked_add(proof_len) else {
            return None;
        };
        Some(Self {
            headers_end: size_of_headers,
            body_end,
            proof_start,
            proof_end,
            retransmitter_signature_start: if resigned { Some(proof_end) } else { None },
            erasure_shard_start,
            capacity,
            payload_len,
        })
    }

    /// Total length of the shred, excluding any trailing repair nonce.
    #[inline]
    pub const fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Number of body bytes this layout leaves for ledger data or erasure codes.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The common header followed by the kind-specific header.
    #[inline]
    pub const fn headers(&self) -> Range<usize> {
        0..self.headers_end
    }

    /// Ledger data for a data shred, erasure codes for a code shred.
    ///
    /// For data shreds this is the whole capacity; the meaningful prefix of it is given by
    /// [`DataHeader::size`](crate::DataHeader::size).
    #[inline]
    pub const fn body(&self) -> Range<usize> {
        self.headers_end..self.body_end
    }

    /// The Merkle root of the preceding erasure batch.
    #[inline]
    pub const fn chained_merkle_root(&self) -> Range<usize> {
        self.body_end..self.proof_start
    }

    /// The flattened Merkle proof, `proof_size` entries of 20 bytes each.
    #[inline]
    pub const fn merkle_proof(&self) -> Range<usize> {
        self.proof_start..self.proof_end
    }

    /// The retransmitter's signature, present only for resigned variants.
    #[inline]
    pub const fn retransmitter_signature(&self) -> Option<Range<usize>> {
        match self.retransmitter_signature_start {
            Some(start) => Some(start..self.payload_len),
            None => None,
        }
    }

    /// The region covered by erasure coding: everything past the signature for data shreds,
    /// everything past the headers for code shreds.
    #[inline]
    pub const fn erasure_shard(&self) -> Range<usize> {
        self.erasure_shard_start..self.body_end
    }

    /// The region hashed to produce this shred's Merkle leaf: everything between the leader's
    /// signature and the proof, the chained Merkle root included.
    #[inline]
    pub const fn merkle_leaf(&self) -> Range<usize> {
        SIZE_OF_SIGNATURE..self.proof_start
    }
}

/// Sums `terms`, returning `None` on overflow.
const fn checked_sum(terms: &[usize]) -> Option<usize> {
    let mut total = 0usize;
    let mut i = 0;
    while i < terms.len() {
        total = match total.checked_add(terms[i]) {
            Some(total) => total,
            None => return None,
        };
        i = match i.checked_add(1) {
            Some(i) => i,
            None => return None,
        };
    }
    Some(total)
}
