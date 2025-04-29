#![cfg(feature = "agave-unstable-api")]
//! Rebuilding the missing shreds of an erasure batch.
//!
//! Recovery is the read path's mirror of `solana-shredder`, and it runs the same
//! passes in the same order for the same reason: Reed-Solomon fills the missing erasure shards,
//! then the batch's Merkle tree is rebuilt over all 64 shards so that each rebuilt shred can be
//! given the proof that witnesses it.
//!
//! What a recovered shred does not get is a fresh signature. The one the leader produced is over
//! the batch's Merkle root, which is a property of the whole set rather than of any one shred, so
//! it is copied out of a survivor. That is only sound if the rebuilt batch really is the batch the
//! survivors came from, which is what the root check at the end establishes: the tree over the
//! rebuilt shards has to hash to the root the survivors' own proofs reconstruct. A shard from
//! another batch, or a corrupted one, changes the root and the whole recovery is rejected.

pub mod error;

use {
    crate::error::RecoverError,
    bytes::Bytes,
    solana_hash::Hash,
    solana_shred_verify::{MerkleError, MerkleTree, merkle},
    solana_shred_wire_format::{
        constants::{
            CODE_SHREDS, DATA_SHREDS, MERKLE_PROOF_ENTRIES, SHARDS, SIZE_OF_MERKLE_PROOF,
            SIZE_OF_MERKLE_PROOF_ENTRY, payload_buffer,
        },
        headers::{CodeHeader, CommonHeader},
        kind::{Code, Data, ShredLayout},
        shred_variant::{ShredKind, ShredVariant},
        view::{self, ShredView, ShredViewMut},
    },
    solana_shredder::coder,
    solana_signature::Signature,
};

/// Rebuilds whatever is missing from the FEC set `survivors` are part of.
///
/// The rebuilt payloads come back in shard order, and only the shards that were actually missing
/// are rebuilt: an input with complete batch returns nothing.
///
/// # The caller vouches for the survivors
///
/// A rebuilt shred is handed no signature of its own. The one the leader produced is over the
/// batch's Merkle root, a property of the whole set rather than of any one shred, so it is copied
/// out of a survivor, and the root check at the end is what makes that sound. What this function
/// cannot check is that the survivors' own signatures were ever verified. `solana-shred` wraps this
/// in `solana_shred::recover::recover`, which takes shreds whose type says they were, and that
/// wrapper is meant to be the only caller.
pub fn recover_payloads(survivors: &[Bytes]) -> Result<Vec<Bytes>, RecoverError> {
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; SHARDS];
    let mut leaves: Vec<Option<Hash>> = vec![None; SHARDS];
    let mut batch = None;
    collect(survivors, &mut shards, &mut leaves, &mut batch)?;
    let batch = batch.ok_or(RecoverError::NoShreds)?;

    let present: Vec<bool> = shards.iter().map(Option::is_some).collect();
    let survivors = present.iter().filter(|present| **present).count();
    if survivors < DATA_SHREDS {
        return Err(RecoverError::NotEnoughShards {
            have: survivors,
            need: DATA_SHREDS,
        });
    }
    if survivors == SHARDS {
        return Ok(Vec::new());
    }
    coder().reconstruct(&mut shards[..])?;

    // Everything before the proof, so that the leaves below are over finished bytes.
    let mut rebuilt = Vec::with_capacity(SHARDS.saturating_sub(survivors));
    for (index, shard) in shards.iter().enumerate() {
        if present.get(index).copied().unwrap_or(false) {
            continue;
        }
        let shard = shard
            .as_deref()
            .expect("reconstruct filled every shard of a recoverable batch");
        let (payload, leaf) = rebuild(&batch, index, shard)?;
        *leaves
            .get_mut(index)
            .expect("the loop stays inside the batch") = Some(leaf);
        rebuilt.push((index, payload));
    }

    let len = leaves.len();
    let tree = MerkleTree::try_new_with_len(
        leaves
            .into_iter()
            .map(|leaf| leaf.ok_or(MerkleError::EmptyIterator)),
        len,
    )?;
    if *tree.root() != batch.merkle_root {
        return Err(RecoverError::RootMismatch);
    }

    let mut recovered = Vec::with_capacity(rebuilt.len());
    for (index, mut payload) in rebuilt {
        let proof = merkle_proof(&tree, index)?;
        match index < DATA_SHREDS {
            true => write_proof::<Data>(&mut payload, batch.data_variant(), &proof)?,
            false => write_proof::<Code>(&mut payload, batch.code_variant(), &proof)?,
        }
        recovered.push(Bytes::from(payload));
    }
    Ok(recovered)
}

/// What every shred of one FEC set carries identically, which is what a rebuilt shred is missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Batch {
    slot: u64,
    version: u16,
    fec_set_index: u32,
    resigned: bool,
    chained_merkle_root: Hash,
    signature: Signature,
    merkle_root: Hash,
}

impl Batch {
    fn of<K: ShredLayout>(view: &ShredView<'_, K>) -> Result<Self, RecoverError> {
        Ok(Self {
            slot: view.common.slot,
            version: view.common.version,
            fec_set_index: view.common.fec_set_index,
            resigned: view.common.variant.resigned(),
            chained_merkle_root: *view.chained_merkle_root,
            signature: *view.signature,
            merkle_root: merkle::root_of(view)?,
        })
    }

    const fn data_variant(&self) -> ShredVariant {
        ShredVariant::data(self.resigned)
    }

    const fn code_variant(&self) -> ShredVariant {
        ShredVariant::code(self.resigned)
    }
}

/// Takes the survivors' shards, their leaves, and what they agree about the batch.
///
/// A survivor's own variant byte says which kind it is, so the two kinds arrive in one slice and
/// are dispatched here rather than by the caller.
fn collect(
    survivors: &[Bytes],
    shards: &mut [Option<Vec<u8>>],
    leaves: &mut [Option<Hash>],
    batch: &mut Option<Batch>,
) -> Result<(), RecoverError> {
    for payload in survivors {
        match view::peek_variant(payload)?.shred_kind() {
            ShredKind::Data => collect_one::<Data>(payload, shards, leaves, batch),
            ShredKind::Code => collect_one::<Code>(payload, shards, leaves, batch),
        }?;
    }
    Ok(())
}

/// Takes one survivor of known kind.
fn collect_one<K: ShredLayout>(
    payload: &[u8],
    shards: &mut [Option<Vec<u8>>],
    leaves: &mut [Option<Hash>],
    batch: &mut Option<Batch>,
) -> Result<(), RecoverError> {
    let view = ShredView::<K>::read_exact(payload)?;
    let described = Batch::of(&view)?;
    match batch {
        Some(batch) if *batch != described => return Err(RecoverError::MixedFecSets),
        Some(_) => {}
        None => *batch = Some(described),
    }
    // Whether the shard index lands inside the batch is checked below; the headers agreeing that
    // it exists at all was checked while the survivor was read.
    let index = K::erasure_shard_index(&view.common, &view.header);
    let shard = shards
        .get_mut(index)
        .ok_or(RecoverError::ShardIndexOutOfRange {
            index,
            shards: SHARDS,
        })?;
    if shard.is_some() {
        return Err(RecoverError::DuplicateShard { index });
    }
    *shard = Some(view.erasure_shard.to_vec());
    *leaves
        .get_mut(index)
        .expect("the shards and the leaves are indexed alike") =
        Some(merkle::leaf(view.merkle_leaf));
    Ok(())
}

/// Rebuilds the shred at `index` from its recovered shard, up to but not including its proof, and
/// returns it with its Merkle leaf.
fn rebuild(batch: &Batch, index: usize, shard: &[u8]) -> Result<(Vec<u8>, Hash), RecoverError> {
    match index < DATA_SHREDS {
        // A data shred's headers are inside its erasure shard, so the shard is everything the
        // rebuilt shred needs except what the batch as a whole carries.
        true => {
            let mut payload = payload_buffer::<Data>();
            let mut view = ShredViewMut::<Data>::new(&mut payload, batch.data_variant())?;
            view.erasure_shard_mut().copy_from_slice(shard);
            let leaf = finish(&mut view, batch);
            Ok((payload, leaf))
        }
        // A code shred's headers are outside its shard, since the codes are generated before the
        // headers that describe them exist. They are not lost with the shred: every one of them is
        // either fixed by the batch's shape or a counter over it.
        false => {
            let position = index.saturating_sub(DATA_SHREDS);
            let common = CommonHeader {
                variant: batch.code_variant(),
                slot: batch.slot,
                index: batch
                    .fec_set_index
                    .saturating_add(u32::try_from(position).expect("a batch has 32 code shreds")),
                version: batch.version,
                fec_set_index: batch.fec_set_index,
            };
            let header = CodeHeader {
                num_data_shreds: u16::try_from(DATA_SHREDS).expect("32 fits in a u16"),
                num_code_shreds: u16::try_from(CODE_SHREDS).expect("32 fits in a u16"),
                position: u16::try_from(position).expect("a batch has 32 code shreds"),
            };
            let mut payload = payload_buffer::<Code>();
            let mut view = ShredViewMut::<Code>::new(&mut payload, common.variant)?;
            view.write_headers(&common, &header)?;
            view.erasure_shard_mut().copy_from_slice(shard);
            let leaf = finish(&mut view, batch);
            Ok((payload, leaf))
        }
    }
}

/// Writes what the batch carries identically into a rebuilt shred, and hashes its leaf.
///
/// The retransmitter signature is left as it was allocated, all zeroes: it belongs to whichever
/// node forwarded the shred, and nothing here forwarded anything.
fn finish<K: ShredLayout>(view: &mut ShredViewMut<'_, K>, batch: &Batch) -> Hash {
    view.chained_merkle_root_mut()
        .copy_from_slice(batch.chained_merkle_root.as_ref());
    view.signature_mut()
        .copy_from_slice(batch.signature.as_ref());
    merkle::leaf(view.merkle_leaf())
}

/// The proof of the leaf at `index`, as the bytes that go into a shred.
fn merkle_proof(tree: &MerkleTree, index: usize) -> Result<Vec<u8>, RecoverError> {
    let mut proof = Vec::with_capacity(SIZE_OF_MERKLE_PROOF);
    for entry in tree.make_merkle_proof(index, SHARDS) {
        proof.extend_from_slice(entry?);
    }
    debug_assert_eq!(
        proof.len(),
        SIZE_OF_MERKLE_PROOF,
        "a tree over {SHARDS} leaves proves each of them in {MERKLE_PROOF_ENTRIES} entries of \
         {SIZE_OF_MERKLE_PROOF_ENTRY} bytes",
    );
    Ok(proof)
}

fn write_proof<K: ShredLayout>(
    payload: &mut [u8],
    variant: ShredVariant,
    proof: &[u8],
) -> Result<(), RecoverError> {
    let mut view = ShredViewMut::<K>::new(payload, variant)?;
    view.merkle_proof_mut().copy_from_slice(proof);
    Ok(())
}
