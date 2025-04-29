#![cfg(feature = "agave-unstable-api")]
//! Building an erasure batch/FEC set (shredding process).
//!
//! A shred cannot be built alone. Its Merkle proof comes from the tree over its whole FEC set, and
//! the signature it carries is the leader's over that tree's root, so the unit of construction is
//! the batch: 32 data shreds plus the 32 code shreds that erasure-code them.
//!
//! The order the format forces:
//!
//! ```text
//! headers + bodies      the data shreds' headers are inside their erasure shards, so they go first
//! erasure coding        the 32 data shards produce the 32 code shards, and so the code bodies
//! chained Merkle root   written into all 64; outside the shards, hence after coding
//! Merkle tree           64 leaves, one per shard, hashed over each shred's Merkle leaf region
//! signature             one signature over the root, copied into all 64 shreds
//! Merkle proofs         6 entries per shred, from the tree
//! ```
//!
//! # Why the write path is shaped around erasure batches
//!
//! Single shred cannot be constructed due to the Merkle tree. Making the batch the unit of
//! construction ensures that all constructed shreds are valid.
//!
//! What comes out is payload bytes, in shard order, not shreds: this crate has no shred type to
//! hand them back as. `solana-shred` wraps [`build_payloads`] into
//! `solana_shred::shredder::FecSet::build`, which reads each payload back through the parser before
//! handing it out, and that is the only place the batch becomes typed shreds.

pub mod error;

pub use solana_shred_wire_format::constants::{CODE_SHREDS, DATA_SHREDS, SHARDS};
use {
    crate::error::BuildError,
    bytes::Bytes,
    reed_solomon_erasure::galois_8::ReedSolomon,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_shred_verify::merkle,
    solana_shred_wire_format::{
        constants::{
            MERKLE_PROOF_ENTRIES, SIZE_OF_MERKLE_PROOF, SIZE_OF_MERKLE_PROOF_ENTRY, payload_buffer,
        },
        headers::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
        kind::{Code, Data, ShredLayout},
        shred_variant::ShredVariant,
        view::ShredViewMut,
    },
    std::sync::OnceLock,
};

/// Everything about an erasure batch that is not its ledger data.
///
/// Deliberately plain data: which slot is being built, where its shreds are indexed and what the
/// previous batch's root was are all the caller's to decide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FecSetSpec {
    /// Slot these shreds belong to.
    pub slot: Slot,
    /// Slot this one chains to.
    pub parent_slot: Slot,
    /// Cluster shred version.
    pub version: u16,
    /// Tick this batch was produced at, saturated at [`ShredFlags::REFERENCE_TICK_MASK`].
    pub reference_tick: u8,
    /// Index of this batch's first data shred, which is also the FEC set index and the index of
    /// this batch's first code shred.
    ///
    /// The two kinds are indexed by separate counters on the wire, but with every FEC set holding
    /// 32 of each the counters advance together from zero, so they never disagree. A shred whose
    /// index falls outside its own FEC set is rejected by
    /// `solana_shred::shred::Shred::check_policy` anyway.
    pub fec_set_index: u32,
    /// Merkle root of the preceding erasure batch.
    pub chained_merkle_root: Hash,
    /// What this batch ends, if anything: a run of entries, the slot, or nothing.
    pub batch_position: BatchPosition,
}

/// Where the erasure batch ends up in the slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchPosition {
    /// Nothing: the data continues into the next batch, and no completion marker is written.
    Interior,
    /// Terminates the entry batch, last data shred will carry `DATA_COMPLETE_SHRED`.
    DataComplete,
    /// The last batch in the slot, every shred of the batch reserves room for a retransmitter
    /// signature and `LAST_IN_SLOT` flag is set. Implies DataComplete.
    LastInSlot,
}

impl BatchPosition {
    /// Whether shreds of this batch reserve room for a retransmitter signature, which is what the
    /// variant byte's resigned bit says.
    #[inline]
    pub const fn resigned(self) -> bool {
        matches!(self, Self::LastInSlot)
    }

    /// The completion bits this batch's last data shred carries, if any.
    const fn completion_flags(self) -> u8 {
        match self {
            Self::Interior => 0,
            Self::DataComplete => ShredFlags::DATA_COMPLETE_SHRED,
            Self::LastInSlot => ShredFlags::LAST_SHRED_IN_SLOT,
        }
    }
}

impl FecSetSpec {
    /// How much ledger data one batch built to this spec can carry.
    pub const fn capacity(&self) -> usize {
        self.data_capacity_per_shred().saturating_mul(DATA_SHREDS)
    }

    const fn data_capacity_per_shred(&self) -> usize {
        match self.batch_position.resigned() {
            true => Data::SIZE_OF_BODY_RESIGNED,
            false => Data::SIZE_OF_BODY,
        }
    }

    const fn data_variant(&self) -> ShredVariant {
        ShredVariant::data(self.batch_position.resigned())
    }

    const fn code_variant(&self) -> ShredVariant {
        ShredVariant::code(self.batch_position.resigned())
    }
}

/// Builds and signs the erasure batch carrying `data`.
///
/// `data` is a serialized `&[Entry]`, or the tail of one; it is split across the batch's data shreds
/// and zero-padded to fill them. Anything longer than [`FecSetSpec::capacity`] belongs to more than
/// one batch, which is the caller's to split.
///
/// The payloads come back in shard order: the first [`DATA_SHREDS`] are the batch's data shreds in
/// index order, the rest its code shreds. The [`Hash`] is the root the leader signed, which the next
/// batch chains to.
pub fn build_payloads(
    spec: &FecSetSpec,
    data: &[u8],
    keypair: &Keypair,
) -> Result<(Vec<Bytes>, Hash), BuildError> {
    if data.len() > spec.capacity() {
        return Err(BuildError::TooMuchData {
            len: data.len(),
            capacity: spec.capacity(),
        });
    }
    let mut payloads = Vec::with_capacity(SHARDS);
    write_data_shreds(spec, data, &mut payloads)?;
    write_code_shreds(spec, &mut payloads)?;
    encode_erasure_batch(spec, &mut payloads)?;
    for (index, payload) in payloads.iter_mut().enumerate() {
        view(spec, index, payload)?
            .chained_merkle_root_mut()
            .copy_from_slice(spec.chained_merkle_root.as_ref());
    }

    // The leaves have to be hashed before any proof is written, since a leaf covers everything
    // that precedes the proof.
    let leaves: Vec<Hash> = payloads
        .iter_mut()
        .enumerate()
        .map(|(index, payload)| Ok(merkle::leaf(view(spec, index, payload)?.merkle_leaf())))
        .collect::<Result<_, BuildError>>()?;
    let tree = merkle::tree(leaves.into_iter())?;
    let signature = solana_shred_verify::sign(keypair, tree.root());
    for (index, payload) in payloads.iter_mut().enumerate() {
        let mut proof = Vec::with_capacity(SIZE_OF_MERKLE_PROOF);
        for entry in tree.make_merkle_proof(index, SHARDS) {
            proof.extend_from_slice(entry?);
        }
        let mut view = view(spec, index, payload)?;
        debug_assert_eq!(
            proof.len(),
            SIZE_OF_MERKLE_PROOF,
            "a tree over {SHARDS} leaves proves each of them in {MERKLE_PROOF_ENTRIES} entries of \
             {SIZE_OF_MERKLE_PROOF_ENTRY} bytes",
        );
        view.merkle_proof_mut().copy_from_slice(&proof);
        view.signature_mut().copy_from_slice(signature.as_ref());
    }

    let payloads = payloads.into_iter().map(Bytes::from).collect();
    Ok((payloads, *tree.root()))
}

/// Writes the batch's data shreds: headers, and `data` split across their bodies.
fn write_data_shreds(
    spec: &FecSetSpec,
    data: &[u8],
    payloads: &mut Vec<Vec<u8>>,
) -> Result<(), BuildError> {
    let parent_offset = spec
        .slot
        .checked_sub(spec.parent_slot)
        .and_then(|offset| u16::try_from(offset).ok())
        .ok_or(BuildError::BadParentSlot {
            slot: spec.slot,
            parent_slot: spec.parent_slot,
        })?;
    let mut common = CommonHeader {
        variant: spec.data_variant(),
        slot: spec.slot,
        index: spec.fec_set_index,
        version: spec.version,
        fec_set_index: spec.fec_set_index,
    };
    let chunks = data
        .chunks(spec.data_capacity_per_shred())
        .chain(std::iter::repeat(&[][..]))
        .take(DATA_SHREDS);
    for (position, chunk) in chunks.enumerate() {
        // A batch's last data shred is the only one that can end anything, and what it ends is the
        // caller's to say: a full FEC set is not evidence that the data stops there.
        let last = position == DATA_SHREDS.saturating_sub(1);
        let mut flags = spec.reference_tick.min(ShredFlags::REFERENCE_TICK_MASK);
        if last {
            flags |= spec.batch_position.completion_flags();
        }
        let header = DataHeader {
            parent_offset,
            flags: ShredFlags::from(flags),
            // The size field covers the headers as well as the data itself.
            size: u16::try_from(Data::SIZE_OF_HEADERS.saturating_add(chunk.len()))
                .expect("a data shred is shorter than u16::MAX"),
        };
        let mut payload = payload_buffer::<Data>();
        let mut view = ShredViewMut::<Data>::new(&mut payload, common.variant)?;
        view.write_headers(&common, &header)?;
        view.body_mut()
            .get_mut(..chunk.len())
            .expect("the data was chunked to the body's capacity")
            .copy_from_slice(chunk);
        payloads.push(payload);
        common.index = common
            .index
            .checked_add(1)
            .ok_or(BuildError::IndexOverflow)?;
    }
    Ok(())
}

/// Writes the batch's code shreds: headers only, since their bodies are the erasure codes.
fn write_code_shreds(spec: &FecSetSpec, payloads: &mut Vec<Vec<u8>>) -> Result<(), BuildError> {
    let mut common = CommonHeader {
        variant: spec.code_variant(),
        slot: spec.slot,
        index: spec.fec_set_index,
        version: spec.version,
        fec_set_index: spec.fec_set_index,
    };
    for position in 0..CODE_SHREDS {
        let header = CodeHeader {
            num_data_shreds: u16::try_from(DATA_SHREDS).expect("32 fits in a u16"),
            num_code_shreds: u16::try_from(CODE_SHREDS).expect("32 fits in a u16"),
            position: u16::try_from(position).expect("a batch has fewer than u16::MAX shards"),
        };
        let mut payload = payload_buffer::<Code>();
        ShredViewMut::<Code>::new(&mut payload, common.variant)?.write_headers(&common, &header)?;
        payloads.push(payload);
        common.index = common
            .index
            .checked_add(1)
            .ok_or(BuildError::IndexOverflow)?;
    }
    Ok(())
}

/// Fills the code shreds' erasure shards from the data shreds'.
fn encode_erasure_batch(spec: &FecSetSpec, payloads: &mut [Vec<u8>]) -> Result<(), BuildError> {
    let mut shards = Vec::with_capacity(SHARDS);
    for (index, payload) in payloads.iter_mut().enumerate() {
        shards.push(view(spec, index, payload)?.into_erasure_shard());
    }
    coder().encode(&mut shards[..])?;
    Ok(())
}

/// Singleton Reed-Solomon coder.
///
/// Building its encoding matrix is expensive, which is why it is cached. Shared with
/// `solana-fec-set-recovery`, which reconstructs with the same matrix this encodes with.
pub fn coder() -> &'static ReedSolomon {
    static CODER: OnceLock<ReedSolomon> = OnceLock::new();
    CODER.get_or_init(|| {
        ReedSolomon::new(DATA_SHREDS, CODE_SHREDS)
            .expect("32 data plus 32 code shards is a valid erasure configuration")
    })
}

/// A view over the shard at `index` of the batch, whose kind its position decides.
///
/// The two kinds have different layouts but the same set of sections, so the batch-wide passes
/// (chaining, hashing, signing) are written once over a view that hides which kind it is looking
/// at.
fn view<'a>(
    spec: &FecSetSpec,
    index: usize,
    payload: &'a mut [u8],
) -> Result<AnyShredViewMut<'a>, BuildError> {
    Ok(match index < DATA_SHREDS {
        true => AnyShredViewMut::Data(ShredViewMut::new(payload, spec.data_variant())?),
        false => AnyShredViewMut::Code(ShredViewMut::new(payload, spec.code_variant())?),
    })
}

/// A [`ShredViewMut`] of either kind.
enum AnyShredViewMut<'a> {
    Data(ShredViewMut<'a, Data>),
    Code(ShredViewMut<'a, Code>),
}

impl<'a> AnyShredViewMut<'a> {
    fn into_erasure_shard(self) -> &'a mut [u8] {
        match self {
            Self::Data(view) => view.into_erasure_shard(),
            Self::Code(view) => view.into_erasure_shard(),
        }
    }

    fn merkle_leaf(&self) -> &[u8] {
        match self {
            Self::Data(view) => view.merkle_leaf(),
            Self::Code(view) => view.merkle_leaf(),
        }
    }

    fn chained_merkle_root_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Data(view) => view.chained_merkle_root_mut(),
            Self::Code(view) => view.chained_merkle_root_mut(),
        }
    }

    fn merkle_proof_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Data(view) => view.merkle_proof_mut(),
            Self::Code(view) => view.merkle_proof_mut(),
        }
    }

    fn signature_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Data(view) => view.signature_mut(),
            Self::Code(view) => view.signature_mut(),
        }
    }
}
