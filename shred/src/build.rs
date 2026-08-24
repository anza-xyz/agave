//! Building an erasure batch.
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
//! Every byte is written through [`ShredViewMut`], so the writer states no offset of its own, and
//! every finished shred is read back with [`ShredView`](crate::ShredView) before it is handed out.

use {
    crate::{
        error::BuildError,
        headers::{CodeHeader, CommonHeader, DataHeader, ShredFlags},
        kind::{Code, Data, ShredKind},
        merkle,
        policy::DATA_SHREDS_PER_FEC_BLOCK,
        provenance::SelfProduced,
        shred::{CodeShred, DataShred, merkle_tree::MerkleTree},
        shred_variant::ShredVariant,
        state::Verified,
        view::ShredViewMut,
        wire_format::{MERKLE_PROOF_ENTRIES, SIZE_OF_MERKLE_PROOF, SIZE_OF_MERKLE_PROOF_ENTRY},
    },
    bytes::Bytes,
    reed_solomon_erasure::galois_8::ReedSolomon,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::sync::OnceLock,
};

/// Number of data shreds in an erasure batch.
pub const DATA_SHREDS: usize = DATA_SHREDS_PER_FEC_BLOCK as usize;
/// Number of code shreds in an erasure batch.
pub const CODE_SHREDS: usize = DATA_SHREDS;
/// Shards in an erasure batch, which is also the number of leaves in its Merkle tree.
pub const SHARDS: usize = DATA_SHREDS + CODE_SHREDS;

// A proof entry per level of a perfect binary tree over the batch's shards.
static_assertions::const_assert_eq!(SHARDS, 1 << MERKLE_PROOF_ENTRIES);

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
    /// The two kinds are indexed by separate counters on the wire, but with every FEC set holding 32
    /// of each the counters advance together from zero, so they never disagree. A shred whose index
    /// falls outside its own FEC set is rejected by [`admit`](crate::Shred::admit) anyway.
    pub fec_set_index: u32,
    /// Merkle root of the preceding erasure batch.
    pub chained_merkle_root: Hash,
    /// Whether to reserve room for a retransmitter signature, which the last batch of a slot does.
    ///
    /// The room is only reserved, never filled: the retransmitter signature of a shred this node
    /// produced is all zeroes, and stays that way until a node that received the shred signs it.
    pub resigned: bool,
    /// Whether this batch ends the slot.
    pub last_in_slot: bool,
}

impl FecSetSpec {
    /// How much ledger data one batch built to this spec can carry.
    pub const fn capacity(&self) -> usize {
        self.data_capacity_per_shred().saturating_mul(DATA_SHREDS)
    }

    const fn data_capacity_per_shred(&self) -> usize {
        match self.resigned {
            true => Data::SIZE_OF_BODY_RESIGNED,
            false => Data::SIZE_OF_BODY,
        }
    }

    const fn data_variant(&self) -> ShredVariant {
        match self.resigned {
            true => ShredVariant::MerkleDataResigned,
            false => ShredVariant::MerkleData,
        }
    }

    const fn code_variant(&self) -> ShredVariant {
        match self.resigned {
            true => ShredVariant::MerkleCodeResigned,
            false => ShredVariant::MerkleCode,
        }
    }
}

/// One finished erasure batch.
#[derive(Clone, Debug)]
pub struct FecSet {
    /// The batch's data shreds, in index order.
    pub data: Vec<DataShred<Verified, SelfProduced>>,
    /// The batch's code shreds, in index order.
    pub code: Vec<CodeShred<Verified, SelfProduced>>,
    /// The root the leader signed, which the next batch chains to.
    pub merkle_root: Hash,
}

impl FecSet {
    /// Builds and signs the erasure batch carrying `data`.
    ///
    /// `data` is a serialized `&[Entry]`, or the tail of one; it is split across the batch's data
    /// shreds and zero-padded to fill them. Anything longer than [`FecSetSpec::capacity`] belongs
    /// to more than one batch, which is the caller's to split.
    pub fn build(spec: &FecSetSpec, data: &[u8], keypair: &Keypair) -> Result<Self, BuildError> {
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
        let tree = MerkleTree::try_new(leaves.into_iter().map(Ok))?;
        let signature = keypair.sign_message(tree.root().as_ref());
        for (index, payload) in payloads.iter_mut().enumerate() {
            let mut proof = Vec::with_capacity(SIZE_OF_MERKLE_PROOF);
            for entry in tree.make_merkle_proof(index, SHARDS) {
                proof.extend_from_slice(entry?);
            }
            let mut view = view(spec, index, payload)?;
            debug_assert_eq!(
                proof.len(),
                SIZE_OF_MERKLE_PROOF,
                "a tree over {SHARDS} leaves proves each of them in {MERKLE_PROOF_ENTRIES} \
                 entries of {SIZE_OF_MERKLE_PROOF_ENTRY} bytes",
            );
            view.merkle_proof_mut().copy_from_slice(&proof);
            view.signature_mut().copy_from_slice(signature.as_ref());
        }

        // Reading each shred back turns the reader's rules into the writer's test.
        let mut payloads = payloads.into_iter().map(Bytes::from);
        let data = payloads
            .by_ref()
            .take(DATA_SHREDS)
            .map(DataShred::assume_built)
            .collect::<Result<_, _>>()?;
        let code = payloads
            .map(CodeShred::assume_built)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            data,
            code,
            merkle_root: *tree.root(),
        })
    }
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
        // Every batch is a whole FEC set, so its last data shred always completes one; only the
        // last batch of a slot also ends the slot.
        let last = position == DATA_SHREDS.saturating_sub(1);
        let mut flags = spec.reference_tick.min(ShredFlags::REFERENCE_TICK_MASK);
        if last {
            flags |= match spec.last_in_slot {
                true => ShredFlags::LAST_SHRED_IN_SLOT,
                false => ShredFlags::DATA_COMPLETE_SHRED,
            };
        }
        let header = DataHeader {
            parent_offset,
            flags: ShredFlags::from(flags),
            // The size field covers the headers as well as the data itself.
            size: u16::try_from(Data::SIZE_OF_HEADERS.saturating_add(chunk.len()))
                .expect("a data shred is shorter than u16::MAX"),
        };
        let mut payload = vec![0u8; Data::SIZE_OF_PAYLOAD];
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
        let mut payload = vec![0u8; Code::SIZE_OF_PAYLOAD];
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
/// Building its encoding matrix is expensive, which is why it is cached.
fn coder() -> &'static ReedSolomon {
    static CODER: OnceLock<ReedSolomon> = OnceLock::new();
    CODER.get_or_init(|| {
        ReedSolomon::new(DATA_SHREDS, CODE_SHREDS)
            .expect("32 data plus 32 code shards is a valid erasure configuration")
    })
}

/// A view over the shard at `index` of the batch, whose kind its position decides.
///
/// The two kinds have different layouts but the same set of sections, so the batch-wide passes
/// (chaining, hashing, signing) are written once over a view that hides which kind it is looking at.
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

#[cfg(all(test, feature = "dev-context-only-utils"))]
mod tests {
    use {
        super::*,
        crate::{
            policy::AdmissionPolicy,
            shred::{ShredParsed, parse},
        },
        solana_signature::Signature,
        solana_signer::Signer,
    };

    fn keypair() -> Keypair {
        Keypair::new_from_array([7u8; 32])
    }

    fn spec(resigned: bool) -> FecSetSpec {
        FecSetSpec {
            slot: 1_000,
            parent_slot: 999,
            version: 42,
            reference_tick: 5,
            fec_set_index: 64,
            chained_merkle_root: Hash::new_from_array([3u8; 32]),
            resigned,
            last_in_slot: resigned,
        }
    }

    fn policy(spec: &FecSetSpec) -> AdmissionPolicy {
        AdmissionPolicy {
            shred_version: spec.version,
            root: spec.slot.saturating_sub(1),
            max_slot: spec.slot.saturating_add(1_000),
            max_data_shreds_per_slot: 32_768,
            max_code_shreds_per_slot: 32_768,
        }
    }

    /// Everything the read path checks, applied to what the write path produced.
    #[test]
    fn built_batch_passes_the_read_path() {
        let keypair = keypair();
        let data: Vec<u8> = (0..20_000u32).map(|index| index as u8).collect();
        for resigned in [false, true] {
            let spec = spec(resigned);
            let set = FecSet::build(&spec, &data, &keypair).unwrap();
            assert_eq!(set.data.len(), DATA_SHREDS);
            assert_eq!(set.code.len(), CODE_SHREDS);

            let mut reassembled = Vec::new();
            for (position, shred) in set.data.iter().enumerate() {
                let (parsed, nonce) = parse(shred.bytes().clone()).unwrap();
                assert_eq!(nonce, None);
                let ShredParsed::Data(shred) = parsed else {
                    panic!("a data shred parsed as a code shred");
                };
                let shred = shred.admit(&policy(&spec)).unwrap();
                let shred = shred.verify(&keypair.pubkey()).unwrap();
                assert_eq!(shred.merkle_root().unwrap(), set.merkle_root);
                assert_eq!(shred.index(), spec.fec_set_index + position as u32);
                assert_eq!(shred.erasure_shard_index(), Some(position));
                reassembled.extend_from_slice(shred.data().unwrap());
            }
            assert_eq!(reassembled, data);

            for (position, shred) in set.code.iter().enumerate() {
                let (parsed, _) = parse(shred.bytes().clone()).unwrap();
                let ShredParsed::Code(shred) = parsed else {
                    panic!("a code shred parsed as a data shred");
                };
                let shred = shred.admit(&policy(&spec)).unwrap();
                let shred = shred.verify(&keypair.pubkey()).unwrap();
                assert_eq!(shred.merkle_root().unwrap(), set.merkle_root);
                assert_eq!(shred.position(), position as u16);
                assert_eq!(shred.erasure_shard_index(), Some(DATA_SHREDS + position));
            }
        }
    }

    /// A shred this node built carries no retransmitter signature, whether or not its variant
    /// reserves room for one. `resign` is unreachable from here: the shred would have to be
    /// received first.
    #[test]
    fn self_produced_shreds_carry_no_retransmitter_signature() {
        for resigned in [false, true] {
            let spec = spec(resigned);
            let set = FecSet::build(&spec, b"entries", &keypair()).unwrap();
            let zeroes = Signature::default();
            for shred in &set.data {
                assert_eq!(shred.retransmitter_signature(), resigned.then_some(&zeroes));
            }
        }
    }

    #[test]
    fn data_beyond_one_batch_is_rejected() {
        let spec = spec(false);
        let data = vec![0u8; spec.capacity().saturating_add(1)];
        assert_matches::assert_matches!(
            FecSet::build(&spec, &data, &keypair()),
            Err(BuildError::TooMuchData { .. })
        );
    }
}
