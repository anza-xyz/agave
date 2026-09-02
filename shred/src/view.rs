//! Borrowed views over a shred's sections, one per direction.
//!
//! This contains the segmentation logic of the crate, which slices bytes in a buffer in a way
//! that allows them to be interpreted by e.g. wincode parsers.

use {
    crate::{
        error::ParseError,
        headers::{AnyHeader, CommonHeader},
        kind::ShredLayout,
        shred_variant::ShredVariant,
        wire_format::{
            MERKLE_PROOF_ENTRIES, Nonce, OFFSET_OF_VARIANT, ProofEntry, SIZE_OF_NONCE, Section,
            Sections, sections,
        },
    },
    solana_hash::Hash,
    solana_signature::Signature,
    std::marker::PhantomData,
    wincode::{SchemaRead, config::DefaultConfig},
};

/// The sections of one shred, borrowed from its bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShredView<'a, K: ShredLayout> {
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
    pub merkle_proof: &'a [ProofEntry; MERKLE_PROOF_ENTRIES],
    /// The retransmitter's signature, present only for resigned variants.
    pub retransmitter_signature: Option<&'a Signature>,
    /// The region hashed to produce this shred's Merkle leaf: everything between the leader's
    /// signature and the proof, the chained Merkle root included.
    pub merkle_leaf: &'a [u8],
    /// The region covered by erasure coding, which starts where this kind's
    /// [`ERASURE_SHARD_START`](ShredLayout::ERASURE_SHARD_START) says and ends with the body.
    pub erasure_shard: &'a [u8],
}

/// The sections of one shred whose kind is a runtime tag rather than a type parameter.
///
/// Every field but [`header`](Self::header) is identical to [`ShredView`]'s, which is the whole
/// reason a kind-erased shred is cheap: one match builds this, and every accessor on
/// [`AnyShred`](crate::shred::AnyShred) that would otherwise need its own match becomes a field
/// read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnyShredView<'a> {
    /// The leader's signature over the FEC set's Merkle root.
    pub signature: &'a Signature,
    /// The header fields common to both kinds.
    pub common: CommonHeader,
    /// The header of whichever kind this shred turned out to be.
    pub header: AnyHeader,
    /// Ledger data for a data shred, erasure codes for a code shred, zero padding included.
    pub body: &'a [u8],
    /// The Merkle root of the preceding erasure batch.
    pub chained_merkle_root: &'a Hash,
    /// The Merkle proof witnessing this shred's leaf in its FEC set's tree.
    pub merkle_proof: &'a [ProofEntry; MERKLE_PROOF_ENTRIES],
    /// The retransmitter's signature, present only for resigned variants.
    pub retransmitter_signature: Option<&'a Signature>,
    /// The region hashed to produce this shred's Merkle leaf.
    pub merkle_leaf: &'a [u8],
    /// The region covered by erasure coding.
    pub erasure_shard: &'a [u8],
}

impl<'a, K: ShredLayout> From<ShredView<'a, K>> for AnyShredView<'a> {
    fn from(view: ShredView<'a, K>) -> Self {
        Self {
            signature: view.signature,
            common: view.common,
            header: view.header.into(),
            body: view.body,
            chained_merkle_root: view.chained_merkle_root,
            merkle_proof: view.merkle_proof,
            retransmitter_signature: view.retransmitter_signature,
            merkle_leaf: view.merkle_leaf,
            erasure_shard: view.erasure_shard,
        }
    }
}

/// Reads the variant byte without committing to a shred kind.
///
/// This is all a caller needs to pick the `K` that [`ShredView`] should be instantiated with.
pub fn peek_variant(bytes: &[u8]) -> Result<ShredVariant, ParseError> {
    let Some(&byte) = bytes.get(OFFSET_OF_VARIANT) else {
        return Err(ParseError::TooShort {
            len: bytes.len(),
            expected: OFFSET_OF_VARIANT.saturating_add(1),
        });
    };
    ShredVariant::try_from(byte)
}

impl<'a, K: ShredLayout> ShredView<'a, K> {
    /// Reads `bytes` as exactly one shred of kind `K`, with nothing following it.
    pub fn read_exact(bytes: &'a [u8]) -> Result<Self, ParseError> {
        let (view, trailer) = Self::read_prefix(bytes)?;
        if !trailer.is_empty() {
            return Err(ParseError::TrailingBytes(trailer.len()));
        }
        Ok(view)
    }

    /// Reads `bytes` as one shred of kind `K` followed by the repair nonce, which is how a shred
    /// arrives in a repair response.
    ///
    /// The nonce is not optional. Whether one follows the shred is settled by the socket the packet
    /// came from. A Turbine packet goes through [`read_exact`](Self::read_exact) instead, which
    /// rejects the same four bytes it requires here.
    pub fn read_repair_packet(bytes: &'a [u8]) -> Result<(Self, Nonce), ParseError> {
        let (view, mut trailer) = Self::read_prefix(bytes)?;
        match trailer.len() {
            SIZE_OF_NONCE => Ok((view, read::<Nonce>(&mut trailer)?)),
            0 => Err(ParseError::MissingNonce),
            len => Err(ParseError::TrailingBytes(len)),
        }
    }

    /// Reads the shred at the start of `bytes`, returning it and whatever follows it.
    fn read_prefix(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), ParseError> {
        // The kind is checked before the length, so that bytes of the other kind are reported as
        // such instead of as a shred of this kind that came out the wrong size.
        let variant = peek_variant(bytes)?;
        let kind = variant.shred_kind();
        if kind != K::SHRED_KIND {
            return Err(ParseError::UnexpectedKind {
                expected: K::SHRED_KIND,
                found: kind,
            });
        }
        let Some((payload, trailer)) = bytes.split_at_checked(K::SIZE_OF_PAYLOAD) else {
            return Err(ParseError::TooShort {
                len: bytes.len(),
                expected: K::SIZE_OF_PAYLOAD,
            });
        };

        // The variant byte fixes the layout, and the layout is where every boundary below comes
        // from. The variant is read a second time as part of the common header, which is what makes
        // the header's copy and the byte the layout was chosen from provably the same.
        let s = sections::<K>(variant.resigned());
        let signature = read_section::<&Signature>(payload, s.signature)?;
        let (common, header) = read_section::<(CommonHeader, K::Header)>(payload, s.headers)?;
        let chained_merkle_root = read_section::<&Hash>(payload, s.chained_merkle_root)?;
        let merkle_proof =
            read_section::<&[ProofEntry; MERKLE_PROOF_ENTRIES]>(payload, s.merkle_proof)?;
        let retransmitter_signature = match s.retransmitter_signature {
            Some(section) => Some(read_section::<&Signature>(payload, section)?),
            None => None,
        };

        let body = section(payload, s.body);
        K::check_header(&header, body)?;

        let view = Self {
            signature,
            common,
            header,
            body,
            chained_merkle_root,
            merkle_proof,
            retransmitter_signature,
            merkle_leaf: section(payload, s.merkle_leaf),
            erasure_shard: section(payload, s.erasure_shard),
        };
        Ok((view, trailer))
    }
}

/// Shred's sections, handed out for writing.
///
/// The mutable counterpart of [`ShredView`] cannot be a struct of `&mut` sections the way the
/// borrowed one is: the Merkle leaf and the erasure shard overlap the sections they span, and two
/// mutable slices may not alias. So the payload is held whole and each section is borrowed from it
/// on request, which is also the order a shred is built in: headers, body, chained root, then the
/// proof and signature the FEC set's tree produces.
pub struct ShredViewMut<'a, K: ShredLayout> {
    payload: &'a mut [u8],
    sections: Sections,
    _kind: PhantomData<K>,
}

impl<'a, K: ShredLayout> ShredViewMut<'a, K> {
    /// Takes `payload` as the buffer of one shred of kind `K` with the layout `variant` selects.
    ///
    /// The buffer must be exactly one shred long. Its contents are not read, so this is how a shred
    /// under construction is addressed as well as how a finished one is modified.
    pub fn new(payload: &'a mut [u8], variant: ShredVariant) -> Result<Self, ParseError> {
        let kind = variant.shred_kind();
        if kind != K::SHRED_KIND {
            return Err(ParseError::UnexpectedKind {
                expected: K::SHRED_KIND,
                found: kind,
            });
        }
        if payload.len() != K::SIZE_OF_PAYLOAD {
            return Err(ParseError::TooShort {
                len: payload.len(),
                expected: K::SIZE_OF_PAYLOAD,
            });
        }
        Ok(Self {
            payload,
            sections: sections::<K>(variant.resigned()),
            _kind: PhantomData,
        })
    }

    /// Writes the headers, the variant byte included.
    #[inline]
    pub fn write_headers(
        &mut self,
        common: &CommonHeader,
        header: &K::Header,
    ) -> Result<(), wincode::WriteError> {
        let dst = section_mut(self.payload, self.sections.headers);
        wincode::serialize_into(dst, &(*common, *header))
    }

    /// The body, to be filled with ledger data or erasure codes.
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        section_mut(self.payload, self.sections.body)
    }

    /// The chained Merkle root, to be set to the preceding erasure batch's root.
    #[inline]
    pub fn chained_merkle_root_mut(&mut self) -> &mut [u8] {
        section_mut(self.payload, self.sections.chained_merkle_root)
    }

    /// The Merkle proof, to be set from the FEC set's tree.
    #[inline]
    pub fn merkle_proof_mut(&mut self) -> &mut [u8] {
        section_mut(self.payload, self.sections.merkle_proof)
    }

    /// The leader's signature over the FEC set's Merkle root.
    #[inline]
    pub fn signature_mut(&mut self) -> &mut [u8] {
        section_mut(self.payload, self.sections.signature)
    }

    /// The retransmitter signature, or `None` if this shred's variant reserves no room for one.
    #[inline]
    pub fn retransmitter_signature_mut(&mut self) -> Option<&mut [u8]> {
        let section = self.sections.retransmitter_signature?;
        Some(section_mut(self.payload, section))
    }

    /// The erasure-coded region, which the Reed-Solomon coder reads from and writes to.
    #[inline]
    pub fn erasure_shard_mut(&mut self) -> &mut [u8] {
        section_mut(self.payload, self.sections.erasure_shard)
    }

    /// The erasure-coded region, borrowed for as long as the payload is.
    ///
    /// Coding an erasure batch needs all 64 shards borrowed at once, which outlives any one view.
    #[inline]
    pub fn into_erasure_shard(self) -> &'a mut [u8] {
        section_mut(self.payload, self.sections.erasure_shard)
    }

    /// The region this shred's Merkle leaf hashes, which is only meaningful once everything before
    /// the proof has been written.
    #[inline]
    pub fn merkle_leaf(&self) -> &[u8] {
        section(self.payload, self.sections.merkle_leaf)
    }
}

/// Reads one section of the shred as a `T`.
fn read_section<'a, T>(payload: &'a [u8], section: Section) -> Result<T::Dst, ParseError>
where
    T: SchemaRead<'a, DefaultConfig>,
{
    read::<T>(&mut self::section(payload, section))
}

/// Reads a `T` from `reader`, advancing it past what it took.
fn read<'a, T>(reader: &mut &'a [u8]) -> Result<T::Dst, ParseError>
where
    T: SchemaRead<'a, DefaultConfig>,
{
    Ok(T::get(reader)?)
}

/// The bytes of `section`, which the layout puts inside a payload of the right length.
#[inline]
fn section(payload: &[u8], section: Section) -> &[u8] {
    payload
        .get(section.as_range())
        .expect("every section of a fixed-size layout is inside a payload of that size")
}

/// See [`section`].
#[inline]
fn section_mut(payload: &mut [u8], section: Section) -> &mut [u8] {
    payload
        .get_mut(section.as_range())
        .expect("every section of a fixed-size layout is inside a payload of that size")
}
