use {
    crate::{
        error::{InvalidDataSize, ParseError, Reject},
        header::{CommonHeader, ShredFlags},
        kind::{Code, Data, ShredKind},
        merkle,
        policy::{self, AdmissionPolicy},
        shred_variant::{ShredType, ShredVariant},
        state::{Admissible, Parsed, Resigned, ShredState, Verified},
        view::{self, ShredView, ShredViewMut},
        wire_format::{Nonce, ProofEntry},
    },
    bytes::{Bytes, BytesMut},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    std::{fmt, marker::PhantomData},
};

/// The Merkle tree of an erasure batch.
///
/// The file is a symlink to `ledger/src/shred/merkle_tree.rs`, so this is the very tree the cluster
/// already runs and not a reimplementation of it. It compiles unchanged in both crates because its
/// module path is the same in both, and because of the [`Error`] alias below.
// The shared file is written under `solana-ledger`'s lint configuration, which allows plain
// arithmetic; this crate's denies it. Scoped to the one module rather than relaxed crate-wide.
#[allow(clippy::arithmetic_side_effects)]
#[path = "merkle_tree.rs"]
pub mod merkle_tree;

/// The error the shared [`merkle_tree`] file raises, under the name it knows it by.
pub use crate::error::MerkleError as Error;

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
    match view::peek_variant(&bytes)?.shred_type() {
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

impl ShredParsed {
    /// The header fields common to both kinds.
    pub fn common(&self) -> &CommonHeader {
        match self {
            Self::Data(shred) => shred.common(),
            Self::Code(shred) => shred.common(),
        }
    }
}

impl<K: ShredKind> Shred<K, Parsed> {
    /// Reads `bytes` as a shred of this specific kind, followed by an optional repair nonce.
    ///
    /// Every check the bytes have to pass is [`ShredView::read_packet`]'s; all this adds is keeping
    /// the header scalars and dropping the nonce from the buffer. Fails with
    /// [`ParseError::UnexpectedKind`] if the variant byte selects the other kind, which is what a
    /// caller that knew the kind in advance — from a kind-specific blockstore column, say — sees
    /// when that expectation was wrong.
    pub fn parse(mut bytes: Bytes) -> Result<(Self, Option<Nonce>), ParseError> {
        // The view borrows from `bytes`, so it has to go out of scope before the buffer is trimmed.
        let (common, header, nonce) = {
            let (view, nonce) = ShredView::<K>::read_packet(&bytes)?;
            (view.common, view.header, nonce)
        };
        // Drop any repair nonce, which shortens the buffer without copying it. From here on the
        // bytes are exactly one shred, which is what `view()` relies on.
        bytes.truncate(K::SIZE_OF_PAYLOAD);
        let shred = Self {
            bytes,
            common,
            header,
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
    /// Checks the leader's signature over the Merkle root this shred's proof reconstructs.
    ///
    /// This is the expensive stage: one hash of the leaf region, six to climb the proof, and one
    /// ed25519 verification. Everything cheap enough to reject a shred on has already run.
    pub fn verify(self, leader: &Pubkey) -> Result<Shred<K, Verified>, Reject> {
        let root = self.merkle_root()?;
        if !self.signature().verify(leader.as_ref(), root.as_ref()) {
            return Err(Reject::InvalidSignature);
        }
        Ok(self.transition())
    }
}

impl<K: ShredKind> Shred<K, Verified> {
    /// Takes bytes this node assembled and signed itself as a verified shred.
    ///
    /// The leader signature is good by construction — it was produced here, over a root computed
    /// from these bytes — so the shred starts where the read path ends up. The bytes are still put
    /// through [`ShredView::read`], which is what makes the reader's rules the writer's test: a
    /// misplaced section surfaces as the [`ParseError`] a receiver would have raised.
    pub(crate) fn assume_signed(bytes: Bytes) -> Result<Self, ParseError> {
        let (common, header) = {
            let view = ShredView::<K>::read(&bytes)?;
            (view.common, view.header)
        };
        Ok(Self {
            bytes,
            common,
            header,
            _state: PhantomData,
        })
    }

    /// Signs the Merkle root as the retransmitter in the Turbine tree, leaving the leader's
    /// signature intact.
    ///
    /// Only reachable from [`Verified`], so a shred can never be resigned on this node's authority
    /// before its leader signature was checked. Fails with [`Reject::NotResignable`] when the
    /// variant has no room for a retransmitter signature — that depends on a wire bit, so it
    /// cannot be settled by the type system.
    ///
    /// The retransmitter signature covers the same Merkle root the leader signed, which is what
    /// lets a downstream node check it without knowing anything about this shred's contents.
    pub fn resign(mut self, keypair: &Keypair) -> Result<Shred<K, Resigned>, Reject> {
        if self.view().retransmitter_signature.is_none() {
            return Err(Reject::NotResignable);
        }
        let signature = keypair.sign_message(self.merkle_root()?.as_ref());
        // `Bytes` is immutable, so this copies unless we hold the only reference to the buffer.
        let mut buffer = BytesMut::from(std::mem::take(&mut self.bytes));
        ShredViewMut::<K>::new(&mut buffer, self.common.variant)
            .expect("the bytes parsed as this kind of shred already")
            .retransmitter_signature_mut()
            .expect("the variant was just checked to reserve a retransmitter signature")
            .copy_from_slice(signature.as_ref());
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

    /// The shred's sections, borrowed from its bytes.
    #[inline]
    pub fn view(&self) -> ShredView<'_, K> {
        ShredView::read(&self.bytes).expect("the bytes parsed as this kind of shred already")
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
        self.view().signature
    }

    /// The Merkle root of the preceding erasure batch.
    #[inline]
    pub fn chained_merkle_root(&self) -> &Hash {
        self.view().chained_merkle_root
    }

    /// The Merkle proof witnessing this shred's leaf in its FEC set's tree.
    #[inline]
    pub fn merkle_proof(&self) -> &[ProofEntry] {
        self.view().merkle_proof
    }

    /// The retransmitter's signature, if this shred's variant carries one.
    #[inline]
    pub fn retransmitter_signature(&self) -> Option<&Signature> {
        self.view().retransmitter_signature
    }

    /// The erasure-coded region of the shred.
    #[inline]
    pub fn erasure_shard(&self) -> &[u8] {
        self.view().erasure_shard
    }

    /// This shred's index among its FEC set's erasure shards, which is its leaf index in the FEC
    /// set's Merkle tree.
    #[inline]
    pub fn erasure_shard_index(&self) -> Option<usize> {
        K::erasure_shard_index(&self.common, &self.header)
    }

    /// The Merkle root this shred's proof reconstructs from its own leaf.
    ///
    /// This is the message both the leader's and the retransmitter's signatures are over. A shred
    /// carries no root of its own — only the previous batch's — so it has to be recomputed.
    pub fn merkle_root(&self) -> Result<Hash, Reject> {
        let index = self
            .erasure_shard_index()
            .ok_or(Reject::InvalidMerkleProof)?;
        let view = self.view();
        merkle_tree::get_merkle_root(index, merkle::leaf(view.merkle_leaf), view.merkle_proof)
            .map_err(Reject::from)
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
        let body = self.view().body;
        let data_len = size
            .checked_sub(Data::SIZE_OF_HEADERS)
            .filter(|len| *len <= body.len())
            .ok_or(InvalidDataSize { size })?;
        Ok(&body[..data_len])
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
        crate::{fixture, wire_format::OFFSET_OF_VARIANT},
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
    fn parse_fixture_matches_expected_sections() {
        let (parsed, nonce) = parse(fixture::DATA_SHRED).unwrap();
        assert_matches!(nonce, None);
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        assert_eq!(shred.variant(), ShredVariant::MerkleData);
        assert_eq!(shred.slot(), fixture::FIXTURE_SLOT);
        assert_eq!(shred.index(), 64);
        assert_eq!(shred.version(), 42);
        assert_eq!(shred.fec_set_index(), 64);
        assert_eq!(
            shred.parent_slot(),
            Some(fixture::FIXTURE_SLOT.saturating_sub(1))
        );

        // Section sizes, hand-computed from the README's formula: 1203 - 88 - 32 - 6 * 20 - 0.
        let view = shred.view();
        assert_eq!(Data::SIZE_OF_HEADERS, 88);
        assert_eq!(view.body.len(), 963);
        assert_eq!(view.merkle_proof.len(), 6);
        assert_matches!(view.retransmitter_signature, None);
        assert_eq!(view.erasure_shard.len(), 987);
        assert_eq!(view.merkle_leaf.len(), 1019);
        assert_eq!(shred.erasure_shard_index(), Some(0));

        // Each borrowed section points into the shred's own buffer rather than a copy of it.
        let bytes = shred.bytes();
        assert_eq!(bytes.as_ptr(), view.signature.as_ref().as_ptr());
        assert_eq!(
            bytes[88..1051].as_ptr(),
            view.body.as_ptr(),
            "the body follows the 88 bytes of headers"
        );
    }

    #[test]
    fn shred_type_tags_are_the_legacy_wire_bytes() {
        for (shred_type, byte) in [
            (ShredType::Data, 0b1010_0101),
            (ShredType::Code, 0b0101_1010),
        ] {
            let bytes = wincode::serialize(&shred_type).unwrap();
            assert_eq!(bytes, [byte]);
            assert_eq!(
                wincode::deserialize::<ShredType>(&bytes).unwrap(),
                shred_type
            );
        }
        // Every valid variant byte must be rejected as a shred type, and vice versa.
        assert_matches!(wincode::deserialize::<ShredType>(&[0x96]), Err(_));
    }

    #[test]
    fn cascade_reaches_verified() {
        let (parsed, _) = parse(fixture::DATA_SHRED).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let shred = shred.admit(&fixture_policy()).unwrap();
        let shred = shred.verify(&fixture::leader()).unwrap();
        assert_eq!(shred.data().unwrap().len(), 963);

        // The same shred against any other signer.
        let (parsed, _) = parse(fixture::DATA_SHRED).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let shred = shred.admit(&fixture_policy()).unwrap();
        assert_matches!(
            shred.verify(&Pubkey::new_from_array([9u8; 32])),
            Err(Reject::InvalidSignature)
        );
    }

    /// Each of the four valid layouts, from bytes to `Verified`, with `resign` reachable exactly
    /// for the two that reserve room for a retransmitter signature.
    #[test]
    fn every_layout_reaches_verified() {
        let policy = fixture_policy();
        let leader = fixture::leader();
        for (bytes, variant) in [
            (fixture::DATA_SHRED, ShredVariant::MerkleData),
            (
                fixture::DATA_SHRED_RESIGNED,
                ShredVariant::MerkleDataResigned,
            ),
        ] {
            let (parsed, _) = parse(bytes).unwrap();
            let ShredParsed::Data(shred) = parsed else {
                panic!("{variant:?} is a data shred, not a code shred");
            };
            assert_eq!(shred.variant(), variant);
            let shred = shred.admit(&policy).unwrap().verify(&leader).unwrap();
            // The first data shred of a batch carries a full chunk, which is the body's length.
            assert_eq!(shred.data().unwrap().len(), shred.view().body.len());
            assert_eq!(shred.erasure_shard_index(), Some(0));
            assert_resignable(shred, variant.resigned());
        }
        for (bytes, variant) in [
            (fixture::CODE_SHRED, ShredVariant::MerkleCode),
            (
                fixture::CODE_SHRED_RESIGNED,
                ShredVariant::MerkleCodeResigned,
            ),
        ] {
            let (parsed, _) = parse(bytes).unwrap();
            let ShredParsed::Code(shred) = parsed else {
                panic!("{variant:?} is a code shred, not a data shred");
            };
            assert_eq!(shred.variant(), variant);
            let shred = shred.admit(&policy).unwrap().verify(&leader).unwrap();
            assert_eq!(shred.num_data_shreds(), 32);
            assert_eq!(shred.num_code_shreds(), 32);
            assert_eq!(shred.position(), 0);
            // Code shards follow the 32 data shards in the batch's Merkle tree.
            assert_eq!(shred.erasure_shard_index(), Some(32));
            assert_resignable(shred, variant.resigned());
        }
    }

    /// A verified shred may be resigned exactly when its variant reserves room for the signature.
    fn assert_resignable<K: ShredKind>(shred: Shred<K, Verified>, resignable: bool) {
        let resigned = shred.resign(&fixture::leader_keypair());
        if resignable {
            let resigned = resigned.expect("a resigned variant reserves room for the signature");
            assert_eq!(
                resigned.view().retransmitter_signature,
                Some(
                    &fixture::leader_keypair()
                        .sign_message(resigned.merkle_root().unwrap().as_ref())
                )
            );
        } else {
            assert_matches!(resigned, Err(Reject::NotResignable));
        }
    }

    #[test]
    fn truncation_never_panics() {
        let shred = fixture::DATA_SHRED;
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
        let mut bytes = fixture::DATA_SHRED.to_vec();
        bytes[OFFSET_OF_VARIANT] = byte;
        assert_matches!(
            parse(Bytes::from(bytes)),
            Err(ParseError::InvalidVariant(found)) if found == byte
        );
    }

    #[test]
    fn trailing_repair_nonce_is_split_off() {
        let mut bytes = fixture::DATA_SHRED.to_vec();
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
        let (parsed, _) = parse(fixture::DATA_SHRED).unwrap();
        let ShredParsed::Data(shred) = parsed else {
            panic!("the fixture is a data shred, not a code shred");
        };
        let reason = shred.admit(&policy).expect_err("policy must reject");
        assert_eq!(reason, expected);
    }

    #[test]
    fn parsing_as_the_wrong_kind_is_rejected() {
        assert_matches!(
            Shred::<Code, Parsed>::parse(fixture::DATA_SHRED),
            Err(ParseError::UnexpectedKind {
                expected: ShredType::Code,
                found: ShredType::Data,
            })
        );
    }
}
