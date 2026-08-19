//! A borrowed view over a shred's sections, read in wire order by wincode.
//!
//! This is where the wire format is written down: [`ShredView::read`] walks a shred once, from the
//! signature to the trailer, taking each section from the reader in the order it appears. The
//! reader's cursor is the only offset involved, and every section that is not a scalar is borrowed
//! from the buffer rather than copied.
//!
//! [`Shred`](crate::Shred) owns the bytes and holds the header scalars; it hands out a view for
//! anything that lives in the buffer.

use {
    crate::{
        error::ParseError,
        header::CommonHeader,
        kind::ShredKind,
        layout::{ProofEntry, SIZE_OF_MERKLE_PROOF_ENTRY, SIZE_OF_MERKLE_ROOT, SIZE_OF_SIGNATURE},
    },
    solana_hash::Hash,
    solana_signature::Signature,
    wincode::{SchemaRead, SchemaReadContext, config::DefaultConfig, context, io::Reader},
};

/// The sections of one shred, borrowed from its bytes.
///
/// Obtained from [`Shred::view`](crate::Shred::view) or, before there is a `Shred`, from
/// [`ShredView::read`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShredView<'a, K: ShredKind> {
    /// The leader's signature over the FEC set's Merkle root.
    pub signature: &'a Signature,
    /// The header fields common to both kinds.
    pub common: CommonHeader,
    /// This kind's own header.
    pub header: K::Header,
    /// Ledger data for a data shred, erasure codes for a code shred, zero padding included.
    pub body: &'a [u8],
    /// The Merkle root of the preceding erasure batch.
    pub chained_merkle_root: &'a Hash,
    /// The Merkle proof witnessing this shred's leaf in its FEC set's tree.
    pub merkle_proof: &'a [ProofEntry],
    /// The retransmitter's signature, present only for resigned variants.
    pub retransmitter_signature: Option<&'a Signature>,
    /// The region hashed to produce this shred's Merkle leaf: everything between the leader's
    /// signature and the proof, the chained Merkle root included.
    pub merkle_leaf: &'a [u8],
    /// The region covered by erasure coding, which starts where this kind's
    /// [`ERASURE_SHARD_START`](ShredKind::ERASURE_SHARD_START) says and ends with the body.
    pub erasure_shard: &'a [u8],
}

impl<'a, K: ShredKind> ShredView<'a, K> {
    /// Reads `bytes` as a shred of kind `K`.
    ///
    /// `bytes` must be exactly one shred: any trailing repair nonce is expected to have been split
    /// off by [`Shred::parse`](crate::Shred::parse) already, and bytes left over after the trailer
    /// are an error.
    pub fn read(bytes: &'a [u8]) -> Result<Self, ParseError> {
        if bytes.len() != K::SIZE_OF_PAYLOAD {
            return Err(ParseError::TooShort {
                len: bytes.len(),
                expected: K::SIZE_OF_PAYLOAD,
            });
        }
        // `&[u8]` is a wincode reader that borrows from its backing storage and advances as it is
        // read, so `tail` is the cursor and `bytes.len() - tail.len()` is the position.
        let mut tail = bytes;
        let signature = read::<&Signature>(&mut tail)?;
        let (common, header) = read::<(CommonHeader, K::Header)>(&mut tail)?;

        // The variant just read fixes the size of everything after the body, which leaves the body
        // whatever is between here and the trailer.
        let proof_size = usize::from(common.variant.proof_size());
        let trailer = proof_size
            .checked_mul(SIZE_OF_MERKLE_PROOF_ENTRY)
            .and_then(|proof| proof.checked_add(SIZE_OF_MERKLE_ROOT))
            .and_then(|trailer| {
                trailer.checked_add(if common.variant.resigned() {
                    SIZE_OF_SIGNATURE
                } else {
                    0
                })
            });
        let capacity = trailer
            .and_then(|trailer| tail.len().checked_sub(trailer))
            .ok_or(ParseError::InvalidProofSize(common.variant.proof_size()))?;

        let body = tail
            .take_borrowed(capacity)
            .map_err(wincode::ReadError::from)?;
        let erasure_shard = span(bytes, K::ERASURE_SHARD_START, &tail);
        let chained_merkle_root = read::<&Hash>(&mut tail)?;
        let merkle_leaf = span(bytes, SIZE_OF_SIGNATURE, &tail);
        let merkle_proof =
            <&[ProofEntry] as SchemaReadContext<DefaultConfig, _>>::get_with_context(
                context::Len(proof_size),
                &mut tail,
            )?;
        let retransmitter_signature = match common.variant.resigned() {
            true => Some(read::<&Signature>(&mut tail)?),
            false => None,
        };
        if !tail.is_empty() {
            return Err(ParseError::TrailingBytes(tail.len()));
        }

        Ok(Self {
            signature,
            common,
            header,
            body,
            chained_merkle_root,
            merkle_proof,
            retransmitter_signature,
            merkle_leaf,
            erasure_shard,
        })
    }
}

/// Reads one section of the shred, advancing `reader` past it.
fn read<'a, T>(reader: &mut &'a [u8]) -> Result<T::Dst, ParseError>
where
    T: SchemaRead<'a, DefaultConfig>,
{
    Ok(T::get(reader)?)
}

/// The bytes from `start` up to the cursor `tail` borrows into.
///
/// Sections that span several reads — the Merkle leaf, the erasure shard — are cut out of the
/// original buffer this way rather than by adding up section sizes.
fn span<'a>(bytes: &'a [u8], start: usize, tail: &&'a [u8]) -> &'a [u8] {
    let end = bytes.len().saturating_sub(tail.len());
    bytes
        .get(start..end)
        .expect("the cursor is inside the shred, and every section start precedes it")
}
