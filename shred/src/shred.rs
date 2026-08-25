//! The shred itself: [`Shred<K, S, P>`](Shred), the transitions between its states, and the
//! accessors each state and kind allows.
//!
//! The states and what they mean are in [`state`](crate::state); the sections the accessors read
//! are in [`view`](crate::view). What lives here is the cascade: [`parse`], `verify` and `resign`
//! are the only ways to construct a shred, so a state is reachable only through the check that
//! establishes it.
//!
//! [`AnyShred`] lives here too, beside the typed shred rather than in a module of its own: the two
//! share the private constructor that every door into a state goes through, and keeping them in one
//! file is what keeps that constructor private.

use {
    crate::{
        error::{InvalidDataSize, ParseError, Reject},
        headers::{AnyHeader, CommonHeader, ShredFlags},
        kind::{Code, Data, ShredLayout},
        merkle,
        policy::{self, AdmissionPolicy},
        provenance::{Provenance, ProvenanceKind, Received, SelfProduced, Stored, Unspecified},
        shred_variant::{ShredKind, ShredVariant},
        state::{Parsed, ShredState, Verified},
        view::{self, AnyShredView, ShredView, ShredViewMut},
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

/// A shred of kind `K` that has reached validation state `S`, having arrived by way of `P`.
///
/// The bytes are held as [`Bytes`], so moving a shred between pipeline stages and cloning it for
/// several consumers costs a refcount rather than a copy. Only the header scalars are materialized;
/// the signature, Merkle root, proof and retransmitter signature are handed out as references into
/// the bytes.
///
/// `S` and `P` are uninhabited markers, so all three parameters cost nothing at runtime.
pub struct Shred<K: ShredLayout, S: ShredState, P: Provenance> {
    bytes: Bytes,
    common: CommonHeader,
    header: K::Header,
    _state: PhantomData<(S, P)>,
}

/// Reads `bytes` as a shred of whichever kind its variant byte selects, splitting off a trailing
/// repair nonce if one is present.
///
/// This is the entry point to the cascade. It checks the length, validates the variant, resolves
/// the layout and deserializes the headers: no hashing and no signature work, so it is cheap
/// enough to run on every packet that arrives.
///
/// What comes back is kind-erased, because the variant byte is the only thing that says which kind
/// the bytes are and reading it is what this function is for. A caller's next move is
/// [`AnyShred::into_data`] or [`AnyShred::into_code`], which moves the kind into the type for every
/// stage that follows, so the match the variant byte forces is the only one the read path pays.
///
/// Wire bytes are the one thing only a [`Received`] shred has, so this is where that provenance
/// enters and the only place it can.
pub fn parse(bytes: Bytes) -> Result<(AnyShred<Parsed, Received>, Option<Nonce>), ParseError> {
    match view::peek_variant(&bytes)?.shred_kind() {
        ShredKind::Data => {
            let (shred, nonce) = Shred::<Data, Parsed, Received>::parse(bytes)?;
            Ok((shred.into(), nonce))
        }
        ShredKind::Code => {
            let (shred, nonce) = Shred::<Code, Parsed, Received>::parse(bytes)?;
            Ok((shred.into(), nonce))
        }
    }
}

impl<K: ShredLayout> Shred<K, Parsed, Received> {
    /// Reads `bytes` as a shred of this specific kind, followed by an optional repair nonce.
    ///
    /// Every check the bytes have to pass is [`ShredView::read_packet`]'s; all this adds is keeping
    /// the header scalars and dropping the nonce from the buffer. Fails with
    /// [`ParseError::UnexpectedKind`] if the variant byte selects the other kind, which is what a
    /// caller that knew the kind in advance (from a kind-specific blockstore column, say) sees when
    /// that expectation was wrong.
    pub fn parse(mut bytes: Bytes) -> Result<(Self, Option<Nonce>), ParseError> {
        // The view borrows from `bytes`, so it has to go out of scope before the buffer is trimmed.
        let (common, header, nonce) = {
            let (view, nonce) = ShredView::<K>::read_wire_packet(&bytes)?;
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

    /// Checks the headers against `policy`, then the leader's signature over the Merkle root this
    /// shred's proof reconstructs.
    ///
    /// The two are one transition because nothing in the pipeline stands between them: a sigverify
    /// worker takes one shred off the queue and runs both. The order still matters and is kept,
    /// cheap first. The policy checks are the right cluster, a plausible slot, an index within the
    /// per-slot limit, FEC set alignment and the kind-specific checks in [`ShredLayout::admit`], none
    /// of which hash anything, so a shred they reject never reaches the one hash of the leaf, the
    /// six to climb the proof, and the ed25519 verification.
    ///
    /// Only shreds that arrived over the network reach here. The other provenances establish their
    /// signature by construction, each through its own constructor.
    pub fn verify(
        self,
        policy: &AdmissionPolicy,
        leader: &Pubkey,
    ) -> Result<Shred<K, Verified, Received>, Reject> {
        self.check_admissible(policy)?;
        let root = self.merkle_root()?;
        if !self.signature().verify(leader.as_ref(), root.as_ref()) {
            return Err(Reject::InvalidSignature);
        }
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

/// The ways into a state that do not check a signature, each pinned to the one provenance whose
/// bytes justify it.
impl<K: ShredLayout> Shred<K, Verified, SelfProduced> {
    /// Takes bytes this node assembled and signed itself as a verified shred.
    ///
    /// The leader signature is good by construction: it was produced here, over a root computed
    /// from these bytes, so the shred starts where the read path ends up.
    pub(crate) fn assume_built(bytes: Bytes) -> Result<Self, ParseError> {
        Self::from_trusted_bytes(bytes)
    }
}

impl<K: ShredLayout, S: ShredState> Shred<K, S, Stored> {
    /// Takes bytes read back from the blockstore, in whatever state the caller needs them.
    ///
    /// The blockstore only stores shreds whose signature was checked before they were inserted, and
    /// what it holds is never unwound, so a shred that comes out of it is as verified as it was
    /// going in. The caller vouches that the bytes came from there and nowhere else.
    ///
    /// Generic over the state because there is no cascade to walk. The checks the states stand for
    /// were all passed before the shred was stored, so replaying them on the way out would pay for
    /// a signature check to learn what is already known. The state is whatever the reading code
    /// needs, and inference picks it from how the shred is used.
    pub fn from_blockstore(bytes: Bytes) -> Result<Self, ParseError> {
        Self::from_trusted_bytes(bytes)
    }
}

impl<K: ShredLayout, S: ShredState, P: Provenance> Shred<K, S, P> {
    /// Shared body of the constructors above. Private, so [`Received`], which has to earn its state
    /// by passing the checks, cannot reach it.
    ///
    /// The bytes are put through [`ShredView::read_exact`], which is what makes the reader's rules
    /// the writer's test: a misplaced section surfaces as the [`ParseError`] a receiver would have
    /// raised.
    fn from_trusted_bytes(bytes: Bytes) -> Result<Self, ParseError> {
        let (common, header) = {
            let view = ShredView::<K>::read_exact(&bytes)?;
            (view.common, view.header)
        };
        Ok(Self {
            bytes,
            common,
            header,
            _state: PhantomData,
        })
    }
}

impl<K: ShredLayout> Shred<K, Verified, Received> {
    /// Signs the Merkle root as the retransmitter in the Turbine tree, leaving the leader's
    /// signature intact.
    ///
    /// Reachable only from [`Verified`], so a shred can never be resigned on this node's authority
    /// before its leader signature was checked, and only for a [`Received`] provenance: a node
    /// retransmits what it was sent, and the shreds it produced itself go out with the
    /// retransmitter signature they were built with, which is all zeroes.
    ///
    /// Fails with [`Reject::NotResignable`] when the variant has no room for a retransmitter
    /// signature, which depends on a wire bit and so cannot be settled by the type system. The
    /// shred is dropped in that case rather than handed back: a variant with nowhere to put a
    /// retransmitter signature has no business on the retransmit path.
    ///
    /// The retransmitter signature covers the same Merkle root the leader signed, which is what
    /// lets a downstream node check it without knowing anything about this shred's contents.
    ///
    /// The state does not change. A separate `Resigned` state would record something no consumer
    /// gates on, and would force a widening on the insert path, which takes verified shreds of
    /// several provenances and does not care whether any of them was retransmit-signed. What does
    /// carry weight, that a shred is never resigned before its leader signature was checked, is held
    /// by this method living on [`Verified`]. The cost is that resigning twice is a logic bug rather
    /// than a compile error; the second write is idempotent under the same key, so it wastes work
    /// and forges nothing.
    pub fn resign(mut self, keypair: &Keypair) -> Result<Self, Reject> {
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
        Ok(self)
    }
}

/// Accessors available in every state, whatever the shred's provenance.
impl<K: ShredLayout, S: ShredState, P: Provenance> Shred<K, S, P> {
    /// Where this shred came from, as a value.
    #[inline]
    pub const fn provenance(&self) -> ProvenanceKind {
        P::KIND
    }

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
        ShredView::read_exact(&self.bytes).expect("the bytes parsed as this kind of shred already")
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
    /// carries no root of its own, only the previous batch's, so it has to be recomputed.
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

    /// Drops the record of where this shred came from, so that shreds of different provenance can
    /// be held together.
    ///
    /// One-way, and it grants nothing: the result can be read and no more. Blockstore insertion is
    /// the path that needs it, taking received and recovered shreds in one batch and neither
    /// verifying nor resigning them. Anything that reports the origin has to read
    /// [`provenance`](Self::provenance) before widening, because nothing puts it back.
    #[inline]
    pub fn forget_provenance(self) -> Shred<K, S, Unspecified> {
        self.retag()
    }

    fn transition<T: ShredState>(self) -> Shred<K, T, P> {
        self.retag()
    }

    /// Rewrites the two phantom parameters. Private: the transitions and the widenings above are
    /// the only ones whose target is sound.
    fn retag<T: ShredState, Q: Provenance>(self) -> Shred<K, T, Q> {
        Shred {
            bytes: self.bytes,
            common: self.common,
            header: self.header,
            _state: PhantomData,
        }
    }
}

/// Accessors that only exist for data shreds.
impl<S: ShredState, P: Provenance> Shred<Data, S, P> {
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
impl<S: ShredState, P: Provenance> Shred<Code, S, P> {
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

impl<K: ShredLayout, S: ShredState, P: Provenance> Clone for Shred<K, S, P> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            common: self.common,
            header: self.header,
            _state: PhantomData,
        }
    }
}

impl<K: ShredLayout, S: ShredState, P: Provenance> fmt::Debug for Shred<K, S, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shred")
            .field("state", &S::NAME)
            .field("provenance", &P::NAME)
            .field("common", &self.common)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<K: ShredLayout, S: ShredState, P: Provenance> AsRef<[u8]> for Shred<K, S, P> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// A shred whose kind is a runtime tag rather than a type parameter, for the places that have to
/// hold both kinds at once.
///
/// Only the header field is erased. Everything else about a shred is either common to both kinds or
/// derived from the variant byte. This exists to facilitate channels which handle mixed shreds.
/// - the channel out of sigverify, where a worker has finished with a typed shred and blockstore
///   wants one stream of both kinds;
/// - the output of the shredder, where Reed-Solomon produces both kinds at once and both insert and
///   broadcast want them flat;
/// - blockstore insert, which runs one pipeline for both kinds until erasure recovery.
///
/// A blockstore read is not one of them: data and code shreds live in separate column families, so
/// the kind is known from the column the bytes were read out of, and
/// [`Shred::from_blockstore`] can be typed.
///
/// Recovery is where they come apart again, which is what [`into_data`](Self::into_data) and
/// [`into_code`](Self::into_code) are for. Code that works on a shred rather than moving it should
/// take a [`Shred<K, S, P>`](Shred).
pub struct AnyShred<S: ShredState, P: Provenance> {
    bytes: Bytes,
    common: CommonHeader,
    header: AnyHeader,
    _state: PhantomData<(S, P)>,
}

impl<K: ShredLayout, S: ShredState, P: Provenance> From<Shred<K, S, P>> for AnyShred<S, P> {
    /// Erasing the kind is infallible and costs ~nothing.
    #[inline]
    fn from(shred: Shred<K, S, P>) -> Self {
        Self {
            bytes: shred.bytes,
            common: shred.common,
            header: shred.header.into(),
            _state: PhantomData,
        }
    }
}

impl<S: ShredState, P: Provenance> AnyShred<S, P> {
    #[inline]
    pub const fn kind(&self) -> ShredKind {
        self.common.variant.shred_kind()
    }

    /// Moves the kind back into the type, handing the shred back if it is the other kind.
    ///
    /// Returns the shred on mismatch rather than dropping it, because guessing wrong about the kind
    /// says nothing against the shred, and a caller splitting a mixed batch needs both halves.
    pub fn into_data(self) -> Result<DataShred<S, P>, Self> {
        match self.header {
            AnyHeader::Data(header) => Ok(Shred {
                bytes: self.bytes,
                common: self.common,
                header,
                _state: PhantomData,
            }),
            AnyHeader::Code(_) => Err(self),
        }
    }

    /// See [`into_data`](Self::into_data).
    pub fn into_code(self) -> Result<CodeShred<S, P>, Self> {
        match self.header {
            AnyHeader::Code(header) => Ok(Shred {
                bytes: self.bytes,
                common: self.common,
                header,
                _state: PhantomData,
            }),
            AnyHeader::Data(_) => Err(self),
        }
    }

    /// Borrows the shred as a typed one, for a caller that needs a kind-specific accessor and not
    /// ownership. Costs a refcount on the bytes and a header copy.
    pub fn as_data(&self) -> Option<DataShred<S, P>> {
        match self.header {
            AnyHeader::Data(header) => Some(Shred {
                bytes: self.bytes.clone(),
                common: self.common,
                header,
                _state: PhantomData,
            }),
            AnyHeader::Code(_) => None,
        }
    }

    /// See [`as_data`](Self::as_data).
    pub fn as_code(&self) -> Option<CodeShred<S, P>> {
        match self.header {
            AnyHeader::Code(header) => Some(Shred {
                bytes: self.bytes.clone(),
                common: self.common,
                header,
                _state: PhantomData,
            }),
            AnyHeader::Data(_) => None,
        }
    }

    /// Where this shred came from, as a value.
    #[inline]
    pub const fn provenance(&self) -> ProvenanceKind {
        P::KIND
    }

    /// The header fields common to both kinds.
    #[inline]
    pub const fn common(&self) -> &CommonHeader {
        &self.common
    }

    /// The header of whichever kind this shred is.
    #[inline]
    pub const fn header(&self) -> &AnyHeader {
        &self.header
    }

    /// The shred's sections, borrowed from its bytes.
    ///
    /// The one match the erased shred pays for. Every layout accessor below reads a field of what
    /// this returns, so the kind is resolved once rather than once per accessor.
    pub fn view(&self) -> AnyShredView<'_> {
        match self.header {
            AnyHeader::Data(_) => ShredView::<Data>::read_exact(&self.bytes)
                .expect("the bytes parsed as a data shred already")
                .into(),
            AnyHeader::Code(_) => ShredView::<Code>::read_exact(&self.bytes)
                .expect("the bytes parsed as a code shred already")
                .into(),
        }
    }

    /// The slot this shred belongs to.
    #[inline]
    pub const fn slot(&self) -> Slot {
        self.common.slot
    }

    /// This shred's index within its slot.
    #[inline]
    pub const fn index(&self) -> u32 {
        self.common.index
    }

    /// The cluster's shred version.
    #[inline]
    pub const fn version(&self) -> u16 {
        self.common.version
    }

    /// Index of the first data shred of this shred's FEC set.
    #[inline]
    pub const fn fec_set_index(&self) -> u32 {
        self.common.fec_set_index
    }

    /// The kind and layout selector.
    #[inline]
    pub const fn variant(&self) -> ShredVariant {
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

    /// This shred's index among its FEC set's erasure shards.
    ///
    /// The second and last match: this is the one thing a kind-erased shred cannot derive from the
    /// layout, because a code shred's leaf index is a function of its own header.
    pub fn erasure_shard_index(&self) -> Option<usize> {
        match &self.header {
            AnyHeader::Data(header) => Data::erasure_shard_index(&self.common, header),
            AnyHeader::Code(header) => Code::erasure_shard_index(&self.common, header),
        }
    }

    /// The Merkle root this shred's proof reconstructs from its own leaf.
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
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Consumes the shred and returns its bytes.
    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    /// Drops the record of where this shred came from, so that shreds of different provenance can
    /// be held together.
    ///
    /// Blockstore insert is what needs this, and it needs the erased shred too: it holds shreds that
    /// just arrived, shreds already stored and shreds rebuilt by erasure recovery, of both kinds, in
    /// one pipeline that only reads them. See [`Shred::forget_provenance`].
    #[inline]
    pub fn forget_provenance(self) -> AnyShred<S, Unspecified> {
        AnyShred {
            bytes: self.bytes,
            common: self.common,
            header: self.header,
            _state: PhantomData,
        }
    }
}

impl<S: ShredState, P: Provenance> Clone for AnyShred<S, P> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            common: self.common,
            header: self.header,
            _state: PhantomData,
        }
    }
}

impl<S: ShredState, P: Provenance> fmt::Debug for AnyShred<S, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyShred")
            .field("state", &S::NAME)
            .field("provenance", &P::NAME)
            .field("common", &self.common)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<S: ShredState, P: Provenance> AsRef<[u8]> for AnyShred<S, P> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Convenience aliases used by callers that only ever hold one kind.
pub type DataShred<S, P> = Shred<Data, S, P>;
/// See [`DataShred`].
pub type CodeShred<S, P> = Shred<Code, S, P>;

#[cfg(all(test, feature = "dev-context-only-utils"))]
mod tests {
    use {
        super::*,
        crate::{fixtures, wire_format::OFFSET_OF_VARIANT},
        assert_matches::assert_matches,
        test_case::test_case,
    };

    /// A policy that admits the fixture, so each test can perturb exactly one field.
    fn fixture_policy() -> AdmissionPolicy {
        AdmissionPolicy {
            shred_version: 42,
            root: fixtures::FIXTURE_SLOT.saturating_sub(1),
            max_slot: fixtures::FIXTURE_SLOT.saturating_add(1_000),
            max_data_shreds_per_slot: 32_768,
            max_code_shreds_per_slot: 32_768,
        }
    }

    #[test]
    fn parse_fixture_matches_expected_sections() {
        let (parsed, nonce) = parse(fixtures::DATA_SHRED).unwrap();
        assert_matches!(nonce, None);
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        assert_eq!(shred.variant(), ShredVariant::MerkleData);
        assert_eq!(shred.slot(), fixtures::FIXTURE_SLOT);
        assert_eq!(shred.index(), 64);
        assert_eq!(shred.version(), 42);
        assert_eq!(shred.fec_set_index(), 64);
        assert_eq!(
            shred.parent_slot(),
            Some(fixtures::FIXTURE_SLOT.saturating_sub(1))
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
    fn shred_kind_tags_are_the_legacy_wire_bytes() {
        for (kind, byte) in [
            (ShredKind::Data, 0b1010_0101),
            (ShredKind::Code, 0b0101_1010),
        ] {
            let bytes = wincode::serialize(&kind).unwrap();
            assert_eq!(bytes, [byte]);
            assert_eq!(wincode::deserialize::<ShredKind>(&bytes).unwrap(), kind);
        }
        // Every valid variant byte must be rejected as a shred kind, and vice versa.
        assert_matches!(wincode::deserialize::<ShredKind>(&[0x96]), Err(_));
    }

    #[test]
    fn cascade_reaches_verified() {
        let (parsed, _) = parse(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        let shred = shred
            .verify(&fixture_policy(), &fixtures::leader())
            .unwrap();
        assert_eq!(shred.data().unwrap().len(), 963);

        // The same shred against any other signer.
        let (parsed, _) = parse(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        assert_matches!(
            shred.verify(&fixture_policy(), &Pubkey::new_from_array([9u8; 32])),
            Err(Reject::InvalidSignature)
        );
    }

    /// Each of the four valid layouts, from bytes to `Verified`, with `resign` reachable exactly
    /// for the two that reserve room for a retransmitter signature.
    #[test]
    fn every_layout_reaches_verified() {
        let policy = fixture_policy();
        let leader = fixtures::leader();
        for (bytes, variant) in [
            (fixtures::DATA_SHRED, ShredVariant::MerkleData),
            (
                fixtures::DATA_SHRED_RESIGNED,
                ShredVariant::MerkleDataResigned,
            ),
        ] {
            let (parsed, _) = parse(bytes).unwrap();
            let shred = parsed
                .into_data()
                .unwrap_or_else(|_| panic!("{variant:?} is a data shred, not a code shred"));
            assert_eq!(shred.variant(), variant);
            let shred = shred.verify(&policy, &leader).unwrap();
            // The first data shred of a batch carries a full chunk, which is the body's length.
            assert_eq!(shred.data().unwrap().len(), shred.view().body.len());
            assert_eq!(shred.erasure_shard_index(), Some(0));
            assert_resignable(shred, variant.resigned());
        }
        for (bytes, variant) in [
            (fixtures::CODE_SHRED, ShredVariant::MerkleCode),
            (
                fixtures::CODE_SHRED_RESIGNED,
                ShredVariant::MerkleCodeResigned,
            ),
        ] {
            let (parsed, _) = parse(bytes).unwrap();
            let shred = parsed
                .into_code()
                .unwrap_or_else(|_| panic!("{variant:?} is a code shred, not a data shred"));
            assert_eq!(shred.variant(), variant);
            let shred = shred.verify(&policy, &leader).unwrap();
            assert_eq!(shred.num_data_shreds(), 32);
            assert_eq!(shred.num_code_shreds(), 32);
            assert_eq!(shred.position(), 0);
            // Code shards follow the 32 data shards in the batch's Merkle tree.
            assert_eq!(shred.erasure_shard_index(), Some(32));
            assert_resignable(shred, variant.resigned());
        }
    }

    /// A verified shred may be resigned exactly when its variant reserves room for the signature.
    fn assert_resignable<K: ShredLayout>(shred: Shred<K, Verified, Received>, resignable: bool) {
        let resigned = shred.resign(&fixtures::leader_keypair());
        if resignable {
            let resigned = resigned.expect("a resigned variant reserves room for the signature");
            assert_eq!(
                resigned.view().retransmitter_signature,
                Some(
                    &fixtures::leader_keypair()
                        .sign_message(resigned.merkle_root().unwrap().as_ref())
                )
            );
        } else {
            assert_matches!(resigned, Err(Reject::NotResignable));
        }
    }

    #[test]
    fn truncation_never_panics() {
        let shred = fixtures::DATA_SHRED;
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
        let mut bytes = fixtures::DATA_SHRED.to_vec();
        bytes[OFFSET_OF_VARIANT] = byte;
        assert_matches!(
            parse(Bytes::from(bytes)),
            Err(ParseError::InvalidVariant(found)) if found == byte
        );
    }

    #[test]
    fn trailing_repair_nonce_is_split_off() {
        let mut bytes = fixtures::DATA_SHRED.to_vec();
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
        AdmissionPolicy { max_slot: fixtures::FIXTURE_SLOT.saturating_sub(1), ..fixture_policy() },
        Reject::SlotOutOfRange { slot: fixtures::FIXTURE_SLOT }
    )]
    #[test_case(
        AdmissionPolicy { max_data_shreds_per_slot: 64, ..fixture_policy() },
        Reject::IndexOutOfBounds { index: 64 }
    )]
    #[test_case(
        AdmissionPolicy { root: fixtures::FIXTURE_SLOT, ..fixture_policy() },
        Reject::BadParentOffset { slot: fixtures::FIXTURE_SLOT, parent_offset: 1 }
    )]
    fn admit_rejects_out_of_policy(policy: AdmissionPolicy, expected: Reject) {
        let (parsed, _) = parse(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        let reason = shred
            .verify(&policy, &fixtures::leader())
            .expect_err("policy must reject");
        assert_eq!(reason, expected);
    }

    #[test]
    fn parsing_as_the_wrong_kind_is_rejected() {
        assert_matches!(
            Shred::<Code, Parsed, Received>::parse(fixtures::DATA_SHRED),
            Err(ParseError::UnexpectedKind {
                expected: ShredKind::Code,
                found: ShredKind::Data,
            })
        );
    }
}
