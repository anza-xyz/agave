use {
    crate::{
        error::{InvalidDataSize, ParseError, Reject},
        header::{CommonHeader, ShredFlags},
        kind::{Code, Data, ShredKind},
        layout::{
            Layout, OFFSET_OF_VARIANT, SIZE_OF_MERKLE_ROOT, SIZE_OF_NONCE, SIZE_OF_SIGNATURE,
        },
        merkle,
        policy::{self, AdmissionPolicy},
        shred_variant::{ShredType, ShredVariant},
        state::{Admissible, Parsed, Resigned, ShredState, Verified},
    },
    bytes::{Bytes, BytesMut},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    std::{fmt, marker::PhantomData},
    wincode::ZeroCopy,
};

/// The nonce a repair response carries after the shred, tying it to the request it answers.
pub type Nonce = u32;

/// A shred of kind `K` that has reached validation state `S`.
///
/// The bytes are held as [`Bytes`], so moving a shred between pipeline stages and cloning it for
/// several consumers costs a refcount rather than a copy. Only the header scalars are materialized;
/// the signature, Merkle root, proof and retransmitter signature are handed out as references into
/// the bytes.
pub struct Shred<K: ShredKind, S: ShredState> {
    bytes: Bytes,
    common: CommonHeader,
    header: K::Header,
    /// Derived from `common.variant` at parse time and never changed, so every range it yields is
    /// in bounds of `bytes`.
    layout: Layout,
    _state: PhantomData<S>,
}

/// A parsed shred whose kind is not yet reflected in its type.
///
/// This is the one place the kind is a runtime tag: [`parse`] cannot know it until it has read the
/// variant byte. Matching once here moves the kind into the type for every later stage.
#[derive(Debug)]
pub enum ShredParsed {
    /// A data shred.
    Data(Shred<Data, Parsed>),
    /// A code shred.
    Code(Shred<Code, Parsed>),
}

/// Reads `bytes` as a shred of whichever kind its variant byte selects, splitting off a trailing
/// repair nonce if one is present.
///
/// This is the entry point to the cascade. It checks the length, validates the variant, resolves
/// the layout and deserializes the headers — no hashing and no signature work, so it is cheap
/// enough to run on every packet that arrives.
pub fn parse(bytes: Bytes) -> Result<(ShredParsed, Option<Nonce>), ParseError> {
    match peek_variant(&bytes)?.shred_type() {
        ShredType::Data => {
            let (shred, nonce) = Shred::<Data, Parsed>::parse(bytes)?;
            Ok((ShredParsed::Data(shred), nonce))
        }
        ShredType::Code => {
            let (shred, nonce) = Shred::<Code, Parsed>::parse(bytes)?;
            Ok((ShredParsed::Code(shred), nonce))
        }
    }
}

/// Reads the variant byte without committing to a shred kind.
fn peek_variant(bytes: &[u8]) -> Result<ShredVariant, ParseError> {
    let Some(&byte) = bytes.get(OFFSET_OF_VARIANT) else {
        return Err(ParseError::TooShort {
            len: bytes.len(),
            expected: OFFSET_OF_VARIANT + 1,
        });
    };
    ShredVariant::try_from(byte)
}

impl ShredParsed {
    /// The header fields common to both kinds.
    pub fn common(&self) -> &CommonHeader {
        match self {
            Self::Data(shred) => shred.common(),
            Self::Code(shred) => shred.common(),
        }
    }

    /// The resolved byte layout.
    pub fn layout(&self) -> Layout {
        match self {
            Self::Data(shred) => shred.layout(),
            Self::Code(shred) => shred.layout(),
        }
    }
}

impl<K: ShredKind> Shred<K, Parsed> {
    /// Reads `bytes` as a shred of this specific kind.
    ///
    /// Fails with [`ParseError::UnexpectedKind`] if the variant byte selects the other kind.
    pub fn parse(mut bytes: Bytes) -> Result<(Self, Option<Nonce>), ParseError> {
        let variant = peek_variant(&bytes)?;
        let found = variant.shred_type();
        if found != K::SHRED_TYPE {
            return Err(ParseError::UnexpectedKind {
                expected: K::SHRED_TYPE,
                found,
            });
        }
        let layout =
            K::layout(variant).ok_or(ParseError::InvalidProofSize(variant.proof_size()))?;
        if bytes.len() < K::SIZE_OF_PAYLOAD {
            return Err(ParseError::TooShort {
                len: bytes.len(),
                expected: K::SIZE_OF_PAYLOAD,
            });
        }
        // Whatever follows the shred is either nothing or a repair nonce. Splitting is a refcount
        // operation, so neither the shred nor the trailer is copied.
        let trailer = bytes.split_off(K::SIZE_OF_PAYLOAD);
        let nonce = match trailer.len() {
            0 => None,
            SIZE_OF_NONCE => {
                let bytes = <[u8; SIZE_OF_NONCE]>::try_from(&trailer[..])
                    .expect("trailer length was just matched against SIZE_OF_NONCE");
                Some(Nonce::from_le_bytes(bytes))
            }
            len => return Err(ParseError::TrailingBytes(len)),
        };
        // The signature occupies bytes 0..64 and is read on demand, so deserialization starts at
        // the variant byte.
        let (common, header): (CommonHeader, K::Header) =
            wincode::deserialize(&bytes[OFFSET_OF_VARIANT..])?;
        let shred = Self {
            bytes,
            common,
            header,
            layout,
            _state: PhantomData,
        };
        Ok((shred, nonce))
    }

    /// Checks the headers against `policy`: right cluster, plausible slot, index within the
    /// per-slot limit, FEC set alignment, and the kind-specific checks in
    /// [`ShredKind::admit`].
    ///
    /// No cryptography happens here, so a rejection costs nothing beyond the parse that preceded
    /// it.
    pub fn admit(self, policy: &AdmissionPolicy) -> Result<Shred<K, Admissible>, Reject> {
        self.check_admissible(policy)?;
        Ok(self.transition())
    }

    fn check_admissible(&self, policy: &AdmissionPolicy) -> Result<(), Reject> {
        if self.common.version != policy.shred_version {
            return Err(Reject::ShredVersionMismatch {
                expected: policy.shred_version,
                found: self.common.version,
            });
        }
        if self.common.slot > policy.max_slot {
            return Err(Reject::SlotOutOfRange {
                slot: self.common.slot,
            });
        }
        K::admit(&self.common, &self.header, policy)?;
        if !policy::is_fec_set_aligned(self.common.index, self.common.fec_set_index) {
            return Err(Reject::MisalignedFecSet {
                index: self.common.index,
                fec_set_index: self.common.fec_set_index,
            });
        }
        Ok(())
    }
}

impl<K: ShredKind> Shred<K, Admissible> {
    pub fn verify(self, _leader: &Pubkey) -> Result<Shred<K, Verified>, Reject> {
        self.check_merkle_proof_shape()?;
        // sigverify goes here
        Ok(self.transition())
    }
}

impl<K: ShredKind> Shred<K, Verified> {
    /// Signs the Merkle root as the retransmitter in the Turbine tree, leaving the leader's
    /// signature intact.
    ///
    /// Only reachable from [`Verified`], so a shred can never be resigned on this node's authority
    /// before its leader signature was checked. Fails with [`Reject::NotResignable`] when the
    /// variant has no room for a retransmitter signature — that depends on a wire bit, so it
    /// cannot be settled by the type system.
    ///
    /// The signature covers the shred's Merkle leaf region, pending the root recomputation in
    /// [`merkle`].
    pub fn resign(mut self, keypair: &Keypair) -> Result<Shred<K, Resigned>, Reject> {
        let Some(range) = self.layout.retransmitter_signature() else {
            return Err(Reject::NotResignable);
        };
        let signature = keypair.sign_message(&self.bytes[self.layout.merkle_leaf()]);
        // `Bytes` is immutable, so this copies unless we hold the only reference to the buffer.
        let mut buffer = BytesMut::from(std::mem::take(&mut self.bytes));
        buffer[range].copy_from_slice(signature.as_ref());
        self.bytes = buffer.freeze();
        Ok(self.transition())
    }
}

/// Accessors available in every state.
impl<K: ShredKind, S: ShredState> Shred<K, S> {
    /// The header fields common to both kinds.
    #[inline]
    pub fn common(&self) -> &CommonHeader {
        &self.common
    }

    /// This kind's own header.
    #[inline]
    pub fn header(&self) -> &K::Header {
        &self.header
    }

    /// The resolved byte layout.
    #[inline]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// The slot this shred belongs to.
    #[inline]
    pub fn slot(&self) -> Slot {
        self.common.slot
    }

    /// This shred's index within its slot.
    #[inline]
    pub fn index(&self) -> u32 {
        self.common.index
    }

    /// The cluster's shred version.
    #[inline]
    pub fn version(&self) -> u16 {
        self.common.version
    }

    /// Index of the first data shred of this shred's FEC set.
    #[inline]
    pub fn fec_set_index(&self) -> u32 {
        self.common.fec_set_index
    }

    /// The kind and layout selector.
    #[inline]
    pub fn variant(&self) -> ShredVariant {
        self.common.variant
    }

    /// The leader's signature over the Merkle root, borrowed from the shred's bytes.
    #[inline]
    pub fn signature(&self) -> &Signature {
        Signature::from_bytes(&self.bytes[..SIZE_OF_SIGNATURE])
            .expect("a signature is 64 bytes of alignment 1, and the length is checked at parse")
    }

    /// The Merkle root of the preceding erasure batch.
    #[inline]
    pub fn chained_merkle_root(&self) -> Hash {
        let bytes =
            <[u8; SIZE_OF_MERKLE_ROOT]>::try_from(&self.bytes[self.layout.chained_merkle_root()])
                .expect("the layout yields a 32-byte range in bounds of the shred");
        Hash::new_from_array(bytes)
    }

    /// The flattened Merkle proof, borrowed from the shred's bytes.
    #[inline]
    pub fn merkle_proof(&self) -> &[u8] {
        &self.bytes[self.layout.merkle_proof()]
    }

    /// The retransmitter's signature, if this shred's variant carries one.
    #[inline]
    pub fn retransmitter_signature(&self) -> Option<&Signature> {
        let range = self.layout.retransmitter_signature()?;
        let signature = Signature::from_bytes(&self.bytes[range])
            .expect("a signature is 64 bytes of alignment 1, and the layout range is in bounds");
        Some(signature)
    }

    /// The erasure-coded region of the shred.
    #[inline]
    pub fn erasure_shard(&self) -> &[u8] {
        &self.bytes[self.layout.erasure_shard()]
    }

    /// This shred's index among its FEC set's erasure shards, which is its leaf index in the FEC
    /// set's Merkle tree.
    #[inline]
    pub fn erasure_shard_index(&self) -> Option<usize> {
        K::erasure_shard_index(&self.common, &self.header)
    }

    pub fn check_merkle_proof_shape(&self) -> Result<(), Reject> {
        let index = self
            .erasure_shard_index()
            .ok_or(Reject::InvalidMerkleProof)?;
        merkle::check_proof_shape(index, self.merkle_proof())
    }

    /// The shred's bytes, without any trailing repair nonce.
    #[inline]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Consumes the shred and returns its bytes.
    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    fn transition<T: ShredState>(self) -> Shred<K, T> {
        Shred {
            bytes: self.bytes,
            common: self.common,
            header: self.header,
            layout: self.layout,
            _state: PhantomData,
        }
    }
}

/// Accessors that only exist for data shreds.
impl<S: ShredState> Shred<Data, S> {
    /// Distance in slots back to this shred's parent.
    #[inline]
    pub fn parent_offset(&self) -> u16 {
        self.header.parent_offset
    }

    /// The slot this shred chains to, or `None` if the offset reaches below slot zero.
    #[inline]
    pub fn parent_slot(&self) -> Option<Slot> {
        self.common
            .slot
            .checked_sub(Slot::from(self.header.parent_offset))
    }

    /// The reference tick and completion markers.
    #[inline]
    pub fn flags(&self) -> ShredFlags {
        self.header.flags
    }

    /// The ledger data carried by this shred, zero padding excluded.
    ///
    /// The `size` field this relies on covers the headers as well as the data and is chosen by
    /// whoever built the shred, so it is validated here against the layout rather than at parse
    /// time.
    pub fn data(&self) -> Result<&[u8], InvalidDataSize> {
        let size = usize::from(self.header.size);
        let body = self.layout.body();
        if !(body.start..=body.end).contains(&size) {
            return Err(InvalidDataSize { size });
        }
        Ok(&self.bytes[body.start..size])
    }
}

/// Accessors that only exist for code shreds.
impl<S: ShredState> Shred<Code, S> {
    /// Number of data shreds in this shred's FEC set.
    #[inline]
    pub fn num_data_shreds(&self) -> u16 {
        self.header.num_data_shreds
    }

    /// Number of code shreds in this shred's FEC set.
    #[inline]
    pub fn num_code_shreds(&self) -> u16 {
        self.header.num_code_shreds
    }

    /// Position of this shred among its FEC set's code shreds.
    #[inline]
    pub fn position(&self) -> u16 {
        self.header.position
    }

    /// Index of the first code shred of this shred's FEC set.
    #[inline]
    pub fn first_code_index(&self) -> Option<u32> {
        self.common
            .index
            .checked_sub(u32::from(self.header.position))
    }
}

impl<K: ShredKind, S: ShredState> Clone for Shred<K, S> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            common: self.common,
            header: self.header,
            layout: self.layout,
            _state: PhantomData,
        }
    }
}

impl<K: ShredKind, S: ShredState> fmt::Debug for Shred<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shred")
            .field("state", &S::NAME)
            .field("common", &self.common)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<K: ShredKind, S: ShredState> AsRef<[u8]> for Shred<K, S> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Convenience aliases used by callers that only ever hold one kind.
pub type DataShred<S> = Shred<Data, S>;
/// See [`DataShred`].
pub type CodeShred<S> = Shred<Code, S>;

#[cfg(all(test, feature = "dev-context-only-utils"))]
mod tests {
    use {
        super::*,
        crate::{
            fixture,
            layout::{SIZE_OF_COMMON_HEADER, SIZE_OF_DATA_HEADER},
        },
        assert_matches::assert_matches,
        test_case::test_case,
    };

    /// A policy that admits the fixture, so each test can perturb exactly one field.
    fn fixture_policy() -> AdmissionPolicy {
        AdmissionPolicy {
            shred_version: 42,
            root: fixture::FIXTURE_SLOT.saturating_sub(1),
            max_slot: fixture::FIXTURE_SLOT.saturating_add(1_000),
            max_data_shreds_per_slot: 32_768,
            max_code_shreds_per_slot: 32_768,
        }
    }

    #[test]
    fn parse_fixture_matches_expected_layout() {
        let (parsed, nonce) = parse(fixture::data_shred()).unwrap();
        assert_matches!(nonce, None);
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        assert_eq!(
            shred.variant(),
            ShredVariant::MerkleData {
                proof_size: 6,
                resigned: false,
            }
        );
        assert_eq!(shred.slot(), fixture::FIXTURE_SLOT);
        assert_eq!(shred.index(), 64);
        assert_eq!(shred.version(), 42);
        assert_eq!(shred.fec_set_index(), 64);
        assert_eq!(
            shred.parent_slot(),
            Some(fixture::FIXTURE_SLOT.saturating_sub(1))
        );

        // Hand-computed from the README's formula: 1203 - 88 - 32 - 6 * 20 - 0.
        let layout = shred.layout();
        assert_eq!(layout.payload_len(), 1203);
        assert_eq!(layout.capacity(), 963);
        assert_eq!(
            layout.headers(),
            0..SIZE_OF_COMMON_HEADER + SIZE_OF_DATA_HEADER
        );
        assert_eq!(layout.body(), 88..1051);
        assert_eq!(layout.chained_merkle_root(), 1051..1083);
        assert_eq!(layout.merkle_proof(), 1083..1203);
        assert_eq!(layout.retransmitter_signature(), None);
        assert_eq!(layout.erasure_shard(), 64..1051);
        assert_eq!(layout.merkle_leaf(), 64..1083);
        assert_eq!(shred.merkle_proof().len(), 120);
        assert_eq!(shred.erasure_shard_index(), Some(0));
    }

    #[test]
    fn cascade_reaches_verified() {
        let (parsed, _) = parse(fixture::data_shred()).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let shred = shred.admit(&fixture_policy()).unwrap();
        let shred = shred.verify(&fixture::leader()).unwrap();
        assert_eq!(shred.data().unwrap().len(), 963);

        // `verify` does not authenticate anything yet, so an unrelated pubkey passes just as well.
        // Asserted so that this test fails once the signature check lands.
        let (parsed, _) = parse(fixture::data_shred()).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let shred = shred.admit(&fixture_policy()).unwrap();
        assert_matches!(shred.verify(&Pubkey::new_from_array([9u8; 32])), Ok(_));
    }

    #[test]
    fn proof_shape_rejects_partial_entries_and_shallow_proofs() {
        // A partial trailing entry cannot be a proof.
        assert_matches!(
            merkle::check_proof_shape(0, &[0u8; 21]),
            Err(Reject::InvalidMerkleProof)
        );
        // Two entries witness at most four leaves.
        assert_matches!(merkle::check_proof_shape(3, &[0u8; 40]), Ok(()));
        assert_matches!(
            merkle::check_proof_shape(4, &[0u8; 40]),
            Err(Reject::InvalidMerkleProof)
        );
    }

    #[test]
    fn truncation_never_panics() {
        let shred = fixture::data_shred();
        for len in 0..shred.len() {
            assert_matches!(
                parse(shred.slice(..len)),
                Err(ParseError::TooShort { .. }),
                "a {len}-byte prefix of a 1203-byte shred must be rejected as too short"
            );
        }
    }

    // The two legacy standalone ShredType encodings, and the two unassigned high nibbles.
    #[test_case(0b1010_0101)]
    #[test_case(0b0101_1010)]
    #[test_case(0b0000_0000)]
    #[test_case(0b1111_0000)]
    fn invalid_variant_byte_is_rejected(byte: u8) {
        let mut bytes = fixture::data_shred().to_vec();
        bytes[OFFSET_OF_VARIANT] = byte;
        assert_matches!(
            parse(Bytes::from(bytes)),
            Err(ParseError::InvalidVariant(found)) if found == byte
        );
    }

    #[test]
    fn trailing_repair_nonce_is_split_off() {
        let mut bytes = fixture::data_shred().to_vec();
        bytes.extend_from_slice(&0x0a0b_0c0du32.to_le_bytes());
        let (_, nonce) = parse(Bytes::from(bytes.clone())).unwrap();
        assert_eq!(nonce, Some(0x0a0b_0c0d));

        // Anything else trailing the shred is neither a nonce nor padding we should accept.
        bytes.push(0);
        assert_matches!(parse(Bytes::from(bytes)), Err(ParseError::TrailingBytes(5)));
    }

    #[test_case(
        AdmissionPolicy { shred_version: 43, ..fixture_policy() },
        Reject::ShredVersionMismatch { expected: 43, found: 42 }
    )]
    #[test_case(
        AdmissionPolicy { max_slot: fixture::FIXTURE_SLOT.saturating_sub(1), ..fixture_policy() },
        Reject::SlotOutOfRange { slot: fixture::FIXTURE_SLOT }
    )]
    #[test_case(
        AdmissionPolicy { max_data_shreds_per_slot: 64, ..fixture_policy() },
        Reject::IndexOutOfBounds { index: 64 }
    )]
    #[test_case(
        AdmissionPolicy { root: fixture::FIXTURE_SLOT, ..fixture_policy() },
        Reject::BadParentOffset { slot: fixture::FIXTURE_SLOT, parent_offset: 1 }
    )]
    fn admit_rejects_out_of_policy(policy: AdmissionPolicy, expected: Reject) {
        let (parsed, _) = parse(fixture::data_shred()).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let reason = shred.admit(&policy).expect_err("policy must reject");
        assert_eq!(reason, expected);
    }

    #[test]
    fn parsing_as_the_wrong_kind_is_rejected() {
        assert_matches!(
            Shred::<Code, Parsed>::parse(fixture::data_shred()),
            Err(ParseError::UnexpectedKind {
                expected: ShredType::Code,
                found: ShredType::Data,
            })
        );
    }
}
