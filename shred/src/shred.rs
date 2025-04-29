//! The shred itself: [`Shred<K, S>`](Shred), the transitions between its states, and the
//! accessors each state and kind allows.
//!
//! The states and what they mean are in [`state`](crate::state); the sections the accessors read
//! are in [`view`]. What lives here is the cascade: [`parse_turbine`], [`parse_repair`],
//! `check_policy` and `verify` are the only ways to construct a shred that arrived over the
//! network, so a state is reachable only through the check that establishes it.
//!
//! [`AnyShred`] lives here too, beside the typed shred rather than in a module of its own: the two
//! share the private constructor that every door into a state goes through, and keeping them in one
//! file is what keeps that constructor private.

use {
    crate::{
        error::{ParseError, RejectReason},
        policy::{self, AdmissionPolicy},
        provenance::{Provenance, ShredSource},
        state::{Admissible, Parsed, ShredState, Verified},
    },
    bytes::{Bytes, BytesMut},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_shred_verify::merkle,
    solana_shred_wire_format::{
        constants::{self, Nonce, ProofEntry},
        headers::{AnyHeader, CommonHeader, ShredFlags},
        kind::{self, Code, Data, ShredLayout},
        shred_variant::{ShredKind, ShredVariant},
        view::{self, AnyShredView, ShredView, ShredViewMut},
    },
    solana_signature::Signature,
    std::{fmt, marker::PhantomData},
};

/// A shred of kind `K` that has reached validation state `S`.
///
/// The bytes are held as [`Bytes`], so moving a shred between pipeline stages and cloning it for
/// several consumers costs a refcount rather than a copy. Only the header scalars are materialized;
/// the signature, Merkle root, proof and retransmitter signature are handed out as references into
/// the bytes.
///
/// `S` is an uninhabited marker, so both parameters cost nothing at runtime.
/// [`Provenance`] is the one thing about a shred that is neither on the wire nor a type: see its
/// documentation for why.
pub struct Shred<K: ShredLayout, S: ShredState> {
    bytes: Bytes,
    common: CommonHeader,
    header: K::Header,
    provenance: Provenance,
    _state: PhantomData<S>,
}

/// Reads a Turbine packet as a shred of whichever kind its variant byte selects.
pub fn parse_turbine(bytes: Bytes) -> Result<AnyShred<Parsed>, ParseError> {
    match view::peek_variant(&bytes)?.shred_kind() {
        ShredKind::Data => Ok(Shred::<Data, Parsed>::parse_turbine(bytes)?.into()),
        ShredKind::Code => Ok(Shred::<Code, Parsed>::parse_turbine(bytes)?.into()),
    }
}

/// Reads a repair response as a shred of whichever kind its variant byte selects, plus the nonce of
/// the request it answers.
pub fn parse_repair(bytes: Bytes) -> Result<(AnyShred<Parsed>, Nonce), ParseError> {
    match view::peek_variant(&bytes)?.shred_kind() {
        ShredKind::Data => {
            let (shred, nonce) = Shred::<Data, Parsed>::parse_repair(bytes)?;
            Ok((shred.into(), nonce))
        }
        ShredKind::Code => {
            let (shred, nonce) = Shred::<Code, Parsed>::parse_repair(bytes)?;
            Ok((shred.into(), nonce))
        }
    }
}

impl<K: ShredLayout> Shred<K, Parsed> {
    /// Reads `bytes` as a Turbine packet carrying a shred of this specific kind.
    ///
    /// Fails with [`ParseError::UnexpectedKind`] if the variant byte selects
    /// the other kind.
    pub fn parse_turbine(bytes: Bytes) -> Result<Self, ParseError> {
        // The view borrows from `bytes`, so it has to go out of scope before the buffer is moved.
        let (common, header) = {
            let view = ShredView::<K>::read_exact(&bytes)?;
            (view.common, view.header)
        };
        Ok(Self::received(bytes, common, header, ShredSource::Turbine))
    }

    /// Reads `bytes` as a repair response carrying a shred of this specific kind, plus the nonce of
    /// the request it answers.
    pub fn parse_repair(mut bytes: Bytes) -> Result<(Self, Nonce), ParseError> {
        let (common, header, nonce) = {
            let (view, nonce) = ShredView::<K>::read_repair_packet(&bytes)?;
            (view.common, view.header, nonce)
        };
        // Drop the nonce, which shortens the buffer without copying it. From here on the bytes are
        // exactly one shred, which is what `view()` relies on.
        bytes.truncate(K::SIZE_OF_PAYLOAD);
        Ok((
            Self::received(bytes, common, header, ShredSource::Repair),
            nonce,
        ))
    }

    /// Shared tail of the two parses: the headers are read, the bytes are exactly one shred.
    fn received(
        bytes: Bytes,
        common: CommonHeader,
        header: K::Header,
        source: ShredSource,
    ) -> Self {
        Self {
            bytes,
            common,
            header,
            provenance: Provenance::Received(source),
            _state: PhantomData,
        }
    }

    /// Checks the headers against `policy`.
    ///
    /// The checks are the right cluster, a plausible slot, an index within the per-slot limit, FEC
    /// set alignment and the kind-specific checks in [`ShredLayout::admit`]. None of them hash
    /// anything, so this runs on every shred that arrives, and a shred it rejects never reaches the
    /// one hash of the leaf, the six to climb the proof, and the ed25519 verification in
    /// [`verify`](Shred::verify).
    pub fn check_policy(
        self,
        policy: &AdmissionPolicy,
    ) -> Result<Shred<K, Admissible>, RejectReason> {
        if self.common.version != policy.shred_version {
            return Err(RejectReason::ShredVersionMismatch {
                expected: policy.shred_version,
                found: self.common.version,
            });
        }
        if self.common.slot > policy.max_slot {
            return Err(RejectReason::SlotOutOfRange {
                slot: self.common.slot,
            });
        }
        match self.header.into() {
            AnyHeader::Data(header) => policy::admit_data(&self.common, &header, policy)?,
            AnyHeader::Code(header) => policy::admit_code(&self.common, &header, policy)?,
        }
        if !policy::is_fec_set_aligned(self.common.index, self.common.fec_set_index) {
            return Err(RejectReason::MisalignedFecSet {
                index: self.common.index,
                fec_set_index: self.common.fec_set_index,
            });
        }
        Ok(self.transition())
    }
}

impl<K: ShredLayout> Shred<K, Admissible> {
    /// Checks the leader's signature over the Merkle root this shred's proof reconstructs.
    pub fn verify(self, leader: &Pubkey) -> Result<Shred<K, Verified>, RejectReason> {
        let root = self.merkle_root()?;
        if !solana_shred_verify::verify(self.signature(), leader, &root) {
            return Err(RejectReason::InvalidSignature);
        }
        Ok(self.transition())
    }
}

/// The ways into a state that do not check a signature, each naming the provenance whose bytes
/// justify it.
impl<K: ShredLayout> Shred<K, Verified> {
    /// Takes bytes this node assembled and signed itself as a verified shred.
    ///
    /// The leader signature is good by construction: it was produced here, over a root computed
    /// from these bytes, so the shred starts where the read path ends up.
    pub(crate) fn assume_built(bytes: Bytes) -> Result<Self, ParseError> {
        Self::from_trusted_bytes(bytes, Provenance::BlockProduction)
    }

    /// Takes bytes erasure recovery rebuilt as a verified shred.
    ///
    /// What the leader signed is the Merkle root of the whole FEC set, and
    /// [`recover`](crate::recover::recover) only hands bytes here once the tree over the rebuilt
    /// batch hashes to the root the surviving shreds carry. The signature copied into these bytes
    /// is therefore the leader's over them, checked the same way a received shred's was.
    pub(crate) fn assume_recovered(bytes: Bytes) -> Result<Self, ParseError> {
        Self::from_trusted_bytes(bytes, Provenance::Recovered)
    }

    /// Takes bytes read back from the blockstore as a verified shred.
    ///
    /// The blockstore only stores shreds whose signature was checked before they were inserted, and
    /// what it holds is never unwound, so a shred that comes out of it is as Verified as it was
    /// going in. The caller vouches that the bytes came from there and nowhere else.
    pub fn from_blockstore(bytes: Bytes) -> Result<Self, ParseError> {
        Self::from_trusted_bytes(bytes, Provenance::Blockstore)
    }

    /// Shared body of the constructors above.
    fn from_trusted_bytes(bytes: Bytes, provenance: Provenance) -> Result<Self, ParseError> {
        let (common, header) = {
            let view = ShredView::<K>::read_exact(&bytes)?;
            (view.common, view.header)
        };
        Ok(Self {
            bytes,
            common,
            header,
            provenance,
            _state: PhantomData,
        })
    }
}

impl<K: ShredLayout> Shred<K, Verified> {
    /// Signs the Merkle root as the retransmitter in the Turbine tree, leaving the leader's
    /// signature intact.
    ///
    /// A variant with no room for a retransmitter signature is handed back untouched rather than
    /// rejected. Only the last FEC set of a slot is resigned, so most of what crosses the retransmit
    /// path has nothing to sign, and forwarding it is what a node is supposed to do. Making that the
    /// error case would have every caller distinguishing "must not be forwarded" from "needs no
    /// signature", off a variant bit it already holds.
    ///
    /// The payload is copied whenever this shred's [`Bytes`] is not the sole owner of its whole
    /// allocation.
    // The state does not change. A separate `Resigned` state would record something no consumer
    // gates on.
    pub fn resign(mut self, keypair: &Keypair) -> Result<Self, RejectReason> {
        if !self.provenance.is_received() {
            return Err(RejectReason::NotReceived);
        }
        if !self.common.variant.resigned() {
            return Ok(self);
        }
        let signature = solana_shred_verify::sign(keypair, &self.merkle_root()?);
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

    /// Checks `retransmitter`'s signature over this shred's Merkle root.
    ///
    /// The read side of [`resign`](Self::resign), and reachable only from [`Verified`]: the
    /// retransmitter's signature says which peer forwarded these bytes, which is worth nothing
    /// until the leader's signature says the bytes are the leader's. Who the retransmitter should
    /// be is the caller's to work out, since it is a function of the slot's leader and this node's
    /// position in that slot's tree, neither of which is in the shred.
    pub fn verify_retransmitter(&self, retransmitter: &Pubkey) -> Result<(), RejectReason> {
        let signature = self
            .view()
            .retransmitter_signature
            .copied()
            .ok_or(RejectReason::MissingRetransmitterSignature)?;
        let root = self.merkle_root()?;
        if !solana_shred_verify::verify(&signature, retransmitter, &root) {
            return Err(RejectReason::InvalidRetransmitterSignature);
        }
        Ok(())
    }
}

/// Accessors available in every state.
impl<K: ShredLayout, S: ShredState> Shred<K, S> {
    /// Where this shred came from.
    #[inline]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
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
    pub fn erasure_shard_index(&self) -> usize {
        K::erasure_shard_index(&self.common, &self.header)
    }

    /// The Merkle root this shred's proof reconstructs from its own leaf.
    ///
    /// This is the message both the leader's and the retransmitter's signatures are over. A shred
    /// carries no root of its own, only the previous batch's, so it has to be recomputed.
    pub fn merkle_root(&self) -> Result<Hash, RejectReason> {
        merkle::root_of(&self.view()).map_err(RejectReason::from)
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

    /// The wire bytes of a repair response carrying this shred: the payload followed by the `nonce`
    /// of the request it answers.
    ///
    /// The counterpart of the nonce [`parse_repair`] splits off on the way in. Any provenance may
    /// be served: a repair response says "here are the bytes you asked for", so what matters is
    /// that the shred is the one requested, not how this node came to hold it.
    ///
    /// Consumes the shred so the nonce can be appended in place when no one else holds the payload;
    /// clone it first to keep the shred and pay for a copy instead.
    #[inline]
    pub fn into_repair_response(self, nonce: Nonce) -> Bytes {
        constants::form_repair_response(self.bytes, nonce)
    }

    /// Rewrites the state parameter. Private: the transitions above are the only ones whose target
    /// is sound.
    fn transition<T: ShredState>(self) -> Shred<K, T> {
        Shred {
            bytes: self.bytes,
            common: self.common,
            header: self.header,
            provenance: self.provenance,
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
    /// Infallible: the `size` field this relies on covers the headers as well as the data and is
    /// chosen by whoever built the shred, so it is checked against the layout by
    /// [`Data::check_header`] while the shred is being read. A shred that exists at all has a
    /// readable body, whatever door it came through.
    pub fn data(&self) -> &[u8] {
        let body = self.view().body;
        let len = kind::data_len(&self.header, body.len())
            .expect("the data size was checked against the body when the shred was read");
        body.get(..len)
            .expect("the checked data length is within the body")
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

impl<K: ShredLayout, S: ShredState> Clone for Shred<K, S> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            common: self.common,
            header: self.header,
            provenance: self.provenance,
            _state: PhantomData,
        }
    }
}

impl<K: ShredLayout, S: ShredState> fmt::Debug for Shred<K, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shred")
            .field("state", &S::NAME)
            .field("provenance", &self.provenance)
            .field("common", &self.common)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<K: ShredLayout, S: ShredState> AsRef<[u8]> for Shred<K, S> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// A shred whose kind is a runtime tag rather than a type parameter, for the places that have to
/// hold both kinds at once.
///
/// Only the header field is erased. Everything else about a shred is either common to both kinds or
/// derived from the variant byte. This exists to facilitate the channels which handle mixed shreds.
pub struct AnyShred<S: ShredState> {
    bytes: Bytes,
    common: CommonHeader,
    header: AnyHeader,
    provenance: Provenance,
    _state: PhantomData<S>,
}

impl<K: ShredLayout, S: ShredState> From<Shred<K, S>> for AnyShred<S> {
    /// Erasing the kind is infallible and costs ~nothing.
    #[inline]
    fn from(shred: Shred<K, S>) -> Self {
        Self {
            bytes: shred.bytes,
            common: shred.common,
            header: shred.header.into(),
            provenance: shred.provenance,
            _state: PhantomData,
        }
    }
}

impl<S: ShredState> AnyShred<S> {
    #[inline]
    pub const fn kind(&self) -> ShredKind {
        self.common.variant.shred_kind()
    }

    /// Moves the kind back into the type, handing the shred back if it is the other kind.
    ///
    /// Returns the shred on mismatch rather than dropping it, because guessing wrong about the kind
    /// says nothing against the shred, and a caller splitting a mixed batch needs both halves.
    pub fn into_data(self) -> Result<DataShred<S>, Self> {
        match self.header {
            AnyHeader::Data(header) => Ok(Shred {
                bytes: self.bytes,
                common: self.common,
                header,
                provenance: self.provenance,
                _state: PhantomData,
            }),
            AnyHeader::Code(_) => Err(self),
        }
    }

    /// See [`into_data`](Self::into_data).
    pub fn into_code(self) -> Result<CodeShred<S>, Self> {
        match self.header {
            AnyHeader::Code(header) => Ok(Shred {
                bytes: self.bytes,
                common: self.common,
                header,
                provenance: self.provenance,
                _state: PhantomData,
            }),
            AnyHeader::Data(_) => Err(self),
        }
    }

    /// Where this shred came from.
    #[inline]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
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
    pub fn erasure_shard_index(&self) -> usize {
        match &self.header {
            AnyHeader::Data(header) => Data::erasure_shard_index(&self.common, header),
            AnyHeader::Code(header) => Code::erasure_shard_index(&self.common, header),
        }
    }

    /// The Merkle root this shred's proof reconstructs from its own leaf.
    ///
    /// [`merkle::root_of`] needs the kind in the type to find the leaf's index, so the kind-erased
    /// form takes the index from its own header and climbs the proof directly.
    pub fn merkle_root(&self) -> Result<Hash, RejectReason> {
        let index = self.erasure_shard_index();
        let view = self.view();
        merkle::root(index, merkle::leaf(view.merkle_leaf), view.merkle_proof)
            .map_err(RejectReason::from)
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

    /// The wire bytes of a repair response carrying this shred, followed by the `nonce` of the
    /// request it answers.
    ///
    /// See [`Shred::into_repair_response`].
    #[inline]
    pub fn into_repair_response(self, nonce: Nonce) -> Bytes {
        constants::form_repair_response(self.bytes, nonce)
    }
}

impl AnyShred<Verified> {
    /// Checks `retransmitter`'s signature over this shred's Merkle root.
    ///
    /// See [`Shred::verify_retransmitter`].
    pub fn verify_retransmitter(&self, retransmitter: &Pubkey) -> Result<(), RejectReason> {
        let signature = self
            .view()
            .retransmitter_signature
            .copied()
            .ok_or(RejectReason::MissingRetransmitterSignature)?;
        let root = self.merkle_root()?;
        if !solana_shred_verify::verify(&signature, retransmitter, &root) {
            return Err(RejectReason::InvalidRetransmitterSignature);
        }
        Ok(())
    }
}

impl<S: ShredState> Clone for AnyShred<S> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            common: self.common,
            header: self.header,
            provenance: self.provenance,
            _state: PhantomData,
        }
    }
}

impl<S: ShredState> fmt::Debug for AnyShred<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyShred")
            .field("state", &S::NAME)
            .field("provenance", &self.provenance)
            .field("common", &self.common)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl<S: ShredState> AsRef<[u8]> for AnyShred<S> {
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
        crate::{constants::OFFSET_OF_VARIANT, fixtures},
        assert_matches::assert_matches,
        solana_signer::Signer,
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
        let parsed = parse_turbine(fixtures::DATA_SHRED).unwrap();
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
        assert_eq!(shred.erasure_shard_index(), 0);

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
        let parsed = parse_turbine(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        let shred = shred
            .check_policy(&fixture_policy())
            .and_then(|shred| shred.verify(&fixtures::leader()))
            .unwrap();
        assert_eq!(shred.data().len(), 963);

        // The same shred against any other signer.
        let parsed = parse_turbine(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        assert_matches!(
            shred
                .check_policy(&fixture_policy())
                .and_then(|shred| shred.verify(&Pubkey::new_from_array([9u8; 32]))),
            Err(RejectReason::InvalidSignature)
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
            let parsed = parse_turbine(bytes).unwrap();
            let shred = parsed
                .into_data()
                .unwrap_or_else(|_| panic!("{variant:?} is a data shred, not a code shred"));
            assert_eq!(shred.variant(), variant);
            let shred = shred
                .check_policy(&policy)
                .and_then(|shred| shred.verify(&leader))
                .unwrap();
            // The first data shred of a batch carries a full chunk, which is the body's length.
            assert_eq!(shred.data().len(), shred.view().body.len());
            assert_eq!(shred.erasure_shard_index(), 0);
            assert_resigns(shred, variant.resigned());
        }
        for (bytes, variant) in [
            (fixtures::CODE_SHRED, ShredVariant::MerkleCode),
            (
                fixtures::CODE_SHRED_RESIGNED,
                ShredVariant::MerkleCodeResigned,
            ),
        ] {
            let parsed = parse_turbine(bytes).unwrap();
            let shred = parsed
                .into_code()
                .unwrap_or_else(|_| panic!("{variant:?} is a code shred, not a data shred"));
            assert_eq!(shred.variant(), variant);
            let shred = shred
                .check_policy(&policy)
                .and_then(|shred| shred.verify(&leader))
                .unwrap();
            assert_eq!(shred.num_data_shreds(), 32);
            assert_eq!(shred.num_code_shreds(), 32);
            assert_eq!(shred.position(), 0);
            // Code shards follow the 32 data shards in the batch's Merkle tree.
            assert_eq!(shred.erasure_shard_index(), 32);
            assert_resigns(shred, variant.resigned());
        }
    }

    /// Resigning writes a signature exactly when the variant reserves room for one, and hands back
    /// the untouched shred otherwise.
    fn assert_resigns<K: ShredLayout>(shred: Shred<K, Verified>, resignable: bool) {
        let before = shred.bytes().clone();
        let resigned = shred
            .resign(&fixtures::leader_keypair())
            .expect("a received shred is resignable whatever its variant");
        if resignable {
            assert_eq!(
                resigned.view().retransmitter_signature,
                Some(
                    &fixtures::leader_keypair()
                        .sign_message(resigned.merkle_root().unwrap().as_ref())
                )
            );
        } else {
            assert_matches!(resigned.view().retransmitter_signature, None);
            assert_eq!(
                resigned.bytes(),
                &before,
                "a variant with no room for a retransmitter signature is forwarded unchanged",
            );
        }
    }

    #[test]
    fn truncation_never_panics() {
        let shred = fixtures::DATA_SHRED;
        for len in 0..shred.len() {
            assert_matches!(
                parse_turbine(shred.slice(..len)),
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
            parse_turbine(Bytes::from(bytes)),
            Err(ParseError::InvalidVariant(found)) if found == byte
        );
    }

    #[test]
    fn repair_nonce_is_split_off() {
        let mut bytes = fixtures::DATA_SHRED.to_vec();
        bytes.extend_from_slice(&0x0a0b_0c0du32.to_le_bytes());
        let (_, nonce) = parse_repair(Bytes::from(bytes.clone())).unwrap();
        assert_eq!(nonce, 0x0a0b_0c0d);

        // The same bytes are a malformed Turbine packet: only a repair response carries a nonce.
        assert_matches!(
            parse_turbine(Bytes::from(bytes.clone())),
            Err(ParseError::TrailingBytes(4))
        );

        // Anything else trailing the shred is neither a nonce nor padding we should accept.
        bytes.push(0);
        assert_matches!(
            parse_repair(Bytes::from(bytes)),
            Err(ParseError::TrailingBytes(5))
        );

        // A repair response has to carry the nonce it is a response to.
        assert_matches!(
            parse_repair(fixtures::DATA_SHRED),
            Err(ParseError::MissingNonce)
        );
    }

    #[test_case(
        AdmissionPolicy { shred_version: 43, ..fixture_policy() },
        RejectReason::ShredVersionMismatch { expected: 43, found: 42 }
    )]
    #[test_case(
        AdmissionPolicy { max_slot: fixtures::FIXTURE_SLOT.saturating_sub(1), ..fixture_policy() },
        RejectReason::SlotOutOfRange { slot: fixtures::FIXTURE_SLOT }
    )]
    #[test_case(
        AdmissionPolicy { max_data_shreds_per_slot: 64, ..fixture_policy() },
        RejectReason::IndexOutOfBounds { index: 64 }
    )]
    #[test_case(
        AdmissionPolicy { root: fixtures::FIXTURE_SLOT, ..fixture_policy() },
        RejectReason::BadParentOffset { slot: fixtures::FIXTURE_SLOT, parent_offset: 1 }
    )]
    fn check_policy_rejects_out_of_policy(policy: AdmissionPolicy, expected: RejectReason) {
        let parsed = parse_turbine(fixtures::DATA_SHRED).unwrap();
        let shred = parsed
            .into_data()
            .expect("the fixture is a data shred, not a code shred");
        let reason = shred
            .check_policy(&policy)
            .map(|_| ())
            .expect_err("policy must reject");
        assert_eq!(reason, expected);
    }

    #[test]
    fn parsing_as_the_wrong_kind_is_rejected() {
        assert_matches!(
            Shred::<Code, Parsed>::parse_turbine(fixtures::DATA_SHRED),
            Err(ParseError::UnexpectedKind {
                expected: ShredKind::Code,
                found: ShredKind::Data,
            })
        );
    }
}
