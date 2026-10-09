#![cfg(feature = "agave-unstable-api")]
//! Rebuilding an erasure batch in full from any 32 of its shreds.
//!
//! Recovery is the read path's mirror of `agave-shredder`, and it runs the same
//! passes in the same order for the same reason: Reed-Solomon fills the erasure shards, then the
//! batch's Merkle tree is rebuilt over all 64 shards so that each rebuilt shred can be given the
//! proof that witnesses it.
//!
//! The whole set is rebuilt every time, not just the shreds that are missing. Exactly 32 survivors
//! go into Reed-Solomon and the other 32 shards come out of it, so the root check at the end covers
//! the set as a whole: the tree over those leaves only hashes to the signed root if the leader's
//! set is a valid codeword, which is what makes every node rebuild the same set from whichever 32
//! shreds reached it.
//!
//! What a rebuilt shred does not get is a fresh signature. The one the leader produced is over
//! the batch's Merkle root, which is a property of the whole set rather than of any one shred, so
//! it is copied out of a survivor. That is only sound if the rebuilt batch really is the batch the
//! survivors came from, which is what the root check establishes: a shard from another batch, or a
//! corrupted one, changes the root and the whole rebuild is rejected.

pub mod error;

use {
    crate::error::RecoverError,
    agave_shred_verify::{MerkleTree, merkle},
    agave_shred_wire_format::{
        constants::{
            CODE_SHREDS, DATA_SHREDS, MERKLE_PROOF_ENTRIES, SHARDS, SIZE_OF_MERKLE_PROOF,
            SIZE_OF_MERKLE_PROOF_ENTRY, payload_buffer,
        },
        headers::{CodeHeader, CommonHeader},
        kind::{Code, Data, ShredLayout},
        shred_variant::{ShredKind, ShredVariant},
        view::{self, ShredView, ShredViewMut},
    },
    agave_shredder::coder,
    bytes::Bytes,
    solana_hash::Hash,
    solana_signature::Signature,
};

/// One erasure batch, rebuilt in full.
#[derive(Clone, Debug)]
pub struct RebuiltFecSet {
    /// All [`SHARDS`] payloads of the set in shard order: the data shreds, then the code shreds.
    /// A survivor is handed back as the bytes it was given as.
    pub payloads: Vec<Bytes>,
    /// The root the leader signed, which the next batch chains to.
    pub merkle_root: Hash,
}

/// Rebuilds the whole FEC set `survivors` are part of.
///
/// Needs at least [`DATA_SHREDS`] distinct survivors. Data shards are fed to Reed-Solomon first,
/// so when every data shred survived the coder only encodes.
///
/// # The caller vouches for the survivors
///
/// A rebuilt shred is handed no signature of its own. The one the leader produced is over the
/// batch's Merkle root, a property of the whole set rather than of any one shred, so it is copied
/// out of a survivor, and the root check at the end is what makes that sound. What this function
/// cannot check is that the survivors' own signatures were ever verified. `agave-shred` wraps this
/// in `agave_shred::recover::rebuild_fec_set`, which takes shreds whose type says they were, and
/// that wrapper is meant to be the only caller.
pub fn rebuild_fec_set(survivors: &[Bytes]) -> Result<RebuiltFecSet, RecoverError> {
    let mut slots = [None; SHARDS];
    let reference = collect(survivors, &mut slots)?;
    let have = slots.iter().flatten().count();
    let Some((batch, merkle_root)) = reference.filter(|_| have >= DATA_SHREDS) else {
        return Err(RecoverError::NotEnoughShards {
            have,
            need: DATA_SHREDS,
        });
    };

    // The first DATA_SHREDS survivors in shard order are the coder's input; every other shard,
    // survivor or not, is rebuilt from them.
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; SHARDS];
    let mut leaves: Vec<Option<Hash>> = vec![None; SHARDS];
    let inputs = slots
        .iter()
        .zip(shards.iter_mut().zip(leaves.iter_mut()))
        .filter_map(|(survivor, slot)| Some((survivor.as_ref()?, slot)))
        .take(DATA_SHREDS);
    for (survivor, (shard, leaf)) in inputs {
        *shard = Some(survivor.erasure_shard.to_vec());
        *leaf = Some(merkle::leaf(survivor.merkle_leaf));
    }
    coder().reconstruct(&mut shards[..])?;

    // Everything before the proof, so that the leaves below are over finished bytes.
    let mut rebuilt = Vec::with_capacity(SHARDS.saturating_sub(DATA_SHREDS));
    for (index, (shard, leaf)) in shards.iter().zip(leaves.iter_mut()).enumerate() {
        if leaf.is_some() {
            continue;
        }
        let shard = shard
            .as_deref()
            .expect("reconstruct filled every shard of a recoverable batch");
        let (payload, rebuilt_leaf) = rebuild(&batch, index, shard)?;
        *leaf = Some(rebuilt_leaf);
        rebuilt.push((index, payload, rebuilt_leaf));
    }

    let tree = merkle::tree(
        leaves
            .into_iter()
            .map(|leaf| leaf.expect("every shard is either a coder input or rebuilt")),
    )?;
    if *tree.root() != merkle_root {
        return Err(RecoverError::RootMismatch);
    }

    let mut payloads: Vec<Option<Bytes>> = slots
        .iter()
        .map(|survivor| survivor.map(|survivor| survivor.payload.clone()))
        .collect();
    for (index, mut payload, leaf) in rebuilt {
        let survivor = slots
            .get(index)
            .expect("rebuilt shards are inside the batch");
        if let Some(survivor) = survivor {
            // A survivor that was not a coder input proves a leaf of the same signed tree at the
            // same index as its rebuilt twin, so the two are the same shred.
            debug_assert_eq!(
                merkle::leaf(survivor.merkle_leaf),
                leaf,
                "survivor at shard {index} differs from the shred rebuilt in its place",
            );
            continue;
        }
        let proof = merkle_proof(&tree, index)?;
        match index < DATA_SHREDS {
            true => write_proof::<Data>(&mut payload, batch.data_variant(), &proof)?,
            false => write_proof::<Code>(&mut payload, batch.code_variant(), &proof)?,
        }
        *payloads
            .get_mut(index)
            .expect("rebuilt shards are inside the batch") = Some(Bytes::from(payload));
    }
    Ok(RebuiltFecSet {
        payloads: payloads
            .into_iter()
            .map(|payload| payload.expect("every shard is either a survivor or rebuilt"))
            .collect(),
        merkle_root,
    })
}

/// What every shred of one FEC set carries identically, which is what a rebuilt shred is missing.
///
/// The Merkle root is deliberately not part of it. Survivors are verified, and equal signatures
/// verified against the same leader are over the same root, so the root is computed once from the
/// first survivor's proof rather than from every one of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Batch {
    slot: u64,
    version: u16,
    fec_set_index: u32,
    resigned: bool,
    chained_merkle_root: Hash,
    signature: Signature,
}

impl Batch {
    fn of<K: ShredLayout>(view: &ShredView<'_, K>) -> Self {
        Self {
            slot: view.common.slot,
            version: view.common.version,
            fec_set_index: view.common.fec_set_index,
            resigned: view.common.variant.resigned(),
            chained_merkle_root: *view.chained_merkle_root,
            signature: *view.signature,
        }
    }

    const fn data_variant(&self) -> ShredVariant {
        ShredVariant::data(self.resigned)
    }

    const fn code_variant(&self) -> ShredVariant {
        ShredVariant::code(self.resigned)
    }

    /// The common header of the shred at shard `index` of this batch, or `None` if the batch has
    /// no such shard.
    fn common_header(&self, index: usize) -> Option<CommonHeader> {
        if index >= SHARDS {
            return None;
        }
        let (variant, offset) = match index.checked_sub(DATA_SHREDS) {
            None => (self.data_variant(), index),
            Some(position) => (self.code_variant(), position),
        };
        Some(CommonHeader {
            variant,
            slot: self.slot,
            index: self
                .fec_set_index
                .checked_add(u32::try_from(offset).ok()?)?,
            version: self.version,
            fec_set_index: self.fec_set_index,
        })
    }
}

/// One survivor, as the sections recovery needs from it.
#[derive(Clone, Copy)]
struct Survivor<'a> {
    payload: &'a Bytes,
    erasure_shard: &'a [u8],
    merkle_leaf: &'a [u8],
}

/// Places the survivors in `slots` by shard index, and returns what they agree about the batch
/// along with the root the first of them proves.
///
/// A survivor's own variant byte says which kind it is, so the two kinds arrive in one slice and
/// are dispatched here rather than by the caller.
fn collect<'a>(
    survivors: &'a [Bytes],
    slots: &mut [Option<Survivor<'a>>; SHARDS],
) -> Result<Option<(Batch, Hash)>, RecoverError> {
    let mut reference = None;
    for payload in survivors {
        match view::peek_variant(payload)?.shred_kind() {
            ShredKind::Data => {
                let view = ShredView::<Data>::read_exact(payload)?;
                collect_one(payload, &view, slots, &mut reference)
            }
            ShredKind::Code => {
                let view = ShredView::<Code>::read_exact(payload)?;
                // The shard index is derived from `num_data_shreds`, so the configuration has to
                // be the fixed one before that index means anything.
                let CodeHeader {
                    num_data_shreds,
                    num_code_shreds,
                    position: _,
                } = view.header;
                if usize::from(num_data_shreds) != DATA_SHREDS
                    || usize::from(num_code_shreds) != CODE_SHREDS
                {
                    return Err(RecoverError::MisplacedShred {
                        index: view.common.index,
                        fec_set_index: view.common.fec_set_index,
                    });
                }
                collect_one(payload, &view, slots, &mut reference)
            }
        }?;
    }
    Ok(reference)
}

/// Places one survivor of known kind.
fn collect_one<'a, K: ShredLayout>(
    payload: &'a Bytes,
    view: &ShredView<'a, K>,
    slots: &mut [Option<Survivor<'a>>; SHARDS],
    reference: &mut Option<(Batch, Hash)>,
) -> Result<(), RecoverError> {
    let described = Batch::of(view);
    let batch = match reference {
        Some((batch, _)) if *batch != described => return Err(RecoverError::MixedFecSets),
        Some((batch, _)) => *batch,
        None => {
            *reference = Some((described, merkle::root_of(view)?));
            described
        }
    };
    // The headers have to put the shred where the batch says a shred with its index goes: a data
    // shred among the data shards, a code shred at `fec_set_index + position`.
    let index = K::erasure_shard_index(&view.common, &view.header);
    if batch.common_header(index) != Some(view.common) {
        return Err(RecoverError::MisplacedShred {
            index: view.common.index,
            fec_set_index: view.common.fec_set_index,
        });
    }
    let slot = slots
        .get_mut(index)
        .expect("common_header only describes shards inside the batch");
    if slot.is_some() {
        return Err(RecoverError::DuplicateShard { index });
    }
    *slot = Some(Survivor {
        payload,
        erasure_shard: view.erasure_shard,
        merkle_leaf: view.merkle_leaf,
    });
    Ok(())
}

/// Rebuilds the shred at `index` from its recovered shard, up to but not including its proof, and
/// returns it with its Merkle leaf.
fn rebuild(batch: &Batch, index: usize, shard: &[u8]) -> Result<(Vec<u8>, Hash), RecoverError> {
    let common = batch
        .common_header(index)
        .ok_or(RecoverError::InvalidRebuiltShred { index })?;
    match index < DATA_SHREDS {
        // A data shred's headers are inside its erasure shard, so the shard is everything the
        // rebuilt shred needs except what the batch as a whole carries.
        true => {
            let mut payload = payload_buffer::<Data>();
            ShredViewMut::<Data>::new(&mut payload, common.variant)?
                .erasure_shard_mut()
                .copy_from_slice(shard);
            // Those headers came out of the coder, so nothing has checked them yet. The root check
            // would only prove the leader signed them, and a leader can sign a data shred that
            // claims another slot or another index.
            let rebuilt_common = ShredView::<Data>::read_exact(&payload)
                .ok()
                .map(|view| view.common);
            if rebuilt_common != Some(common) {
                return Err(RecoverError::InvalidRebuiltShred { index });
            }
            let mut view = ShredViewMut::<Data>::new(&mut payload, common.variant)?;
            let leaf = finish(&mut view, batch);
            Ok((payload, leaf))
        }
        // A code shred's headers are outside its shard, since the codes are generated before the
        // headers that describe them exist. They are not lost with the shred: every one of them is
        // either fixed by the batch's shape or a counter over it.
        false => {
            let position = index.saturating_sub(DATA_SHREDS);
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
/// The retransmitter signature is left as it was allocated, all zeroes.
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_shredder::{BatchPosition, FecSetSpec, build_payloads},
        assert_matches::assert_matches,
        rand::seq::SliceRandom,
        solana_keypair::Keypair,
        test_case::test_case,
    };

    const FEC_SET_INDEX: u32 = 64;

    fn spec(batch_position: BatchPosition) -> FecSetSpec {
        FecSetSpec {
            slot: 1_000,
            parent_slot: 999,
            version: 42,
            reference_tick: 5,
            fec_set_index: FEC_SET_INDEX,
            chained_merkle_root: Hash::new_from_array([3u8; 32]),
            batch_position,
        }
    }

    /// A full, correctly signed batch, in shard order, and the root it was signed over.
    fn build(batch_position: BatchPosition) -> (Vec<Bytes>, Hash) {
        let spec = spec(batch_position);
        let data: Vec<u8> = (0..spec.capacity()).map(|index| index as u8).collect();
        build_payloads(&spec, &data, &Keypair::new()).expect("the spec is valid")
    }

    /// Rewrites the headers of one shred, leaving everything else as it was.
    fn edit_headers<K: ShredLayout>(
        payload: &Bytes,
        edit: impl FnOnce(&mut CommonHeader, &mut K::Header),
    ) -> Bytes {
        let view = ShredView::<K>::read_exact(payload).expect("built shreds parse");
        let (mut common, mut header) = (view.common, view.header);
        edit(&mut common, &mut header);
        let mut payload = payload.to_vec();
        ShredViewMut::<K>::new(&mut payload, common.variant)
            .expect("the payload has the kind's length")
            .write_headers(&common, &header)
            .expect("headers fit their section");
        Bytes::from(payload)
    }

    /// Recomputes the code shards from the data shards, the way a leader that signs whatever data
    /// shreds it built would.
    fn reencode(payloads: &[Bytes], resigned: bool) -> Vec<Bytes> {
        let mut payloads: Vec<Vec<u8>> = payloads.iter().map(|payload| payload.to_vec()).collect();
        let mut shards: Vec<&mut [u8]> = payloads
            .iter_mut()
            .enumerate()
            .map(|(index, payload)| match index < DATA_SHREDS {
                true => ShredViewMut::<Data>::new(payload, ShredVariant::data(resigned))
                    .expect("data payload")
                    .into_erasure_shard(),
                false => ShredViewMut::<Code>::new(payload, ShredVariant::code(resigned))
                    .expect("code payload")
                    .into_erasure_shard(),
            })
            .collect();
        coder()
            .encode(&mut shards[..])
            .expect("a full batch encodes");
        payloads.into_iter().map(Bytes::from).collect()
    }

    #[test_case(BatchPosition::Interior)]
    #[test_case(BatchPosition::LastInSlot)]
    fn rebuilds_whole_set_from_any_subset(batch_position: BatchPosition) {
        let (payloads, merkle_root) = build(batch_position);
        let rebuilt = rebuild_fec_set(&payloads[..DATA_SHREDS]).unwrap();
        assert_eq!(
            rebuilt.payloads, payloads,
            "rebuilt from the data shreds alone"
        );
        assert_eq!(rebuilt.merkle_root, merkle_root);
        let rebuilt = rebuild_fec_set(&payloads[DATA_SHREDS..]).unwrap();
        assert_eq!(
            rebuilt.payloads, payloads,
            "rebuilt from the code shreds alone"
        );

        let mut rng = rand::rng();
        for size in DATA_SHREDS..=SHARDS {
            let mut subset = payloads.clone();
            subset.shuffle(&mut rng);
            subset.truncate(size);
            let rebuilt = rebuild_fec_set(&subset).unwrap();
            assert_eq!(rebuilt.payloads, payloads, "rebuilt from {size} shreds");
            assert_eq!(rebuilt.merkle_root, merkle_root);
        }
    }

    #[test]
    fn too_few_shreds() {
        let (payloads, _) = build(BatchPosition::Interior);
        let have = DATA_SHREDS.saturating_sub(1);
        assert_matches!(
            rebuild_fec_set(&payloads[DATA_SHREDS..][..have]),
            Err(RecoverError::NotEnoughShards { have: 31, need: 32 })
        );
        assert_matches!(
            rebuild_fec_set(&[]),
            Err(RecoverError::NotEnoughShards { have: 0, need: 32 })
        );
    }

    #[test]
    fn duplicate_shred() {
        let (payloads, _) = build(BatchPosition::Interior);
        let mut survivors = payloads[..DATA_SHREDS].to_vec();
        survivors.push(payloads[3].clone());
        assert_matches!(
            rebuild_fec_set(&survivors),
            Err(RecoverError::DuplicateShard { index: 3 })
        );
    }

    #[test]
    fn shreds_from_two_sets() {
        // Same spec and data, different leader: only the signature tells the two sets apart.
        let (payloads, _) = build(BatchPosition::Interior);
        let (other, _) = build(BatchPosition::Interior);
        let mut survivors = payloads[..DATA_SHREDS].to_vec();
        survivors[7] = other[7].clone();
        assert_matches!(rebuild_fec_set(&survivors), Err(RecoverError::MixedFecSets));
    }

    #[test_case(5; "data shred")]
    #[test_case(DATA_SHREDS + 1; "code shred")]
    fn corrupted_survivor(index: usize) {
        // With the first two data shreds missing, the coder's inputs are data shreds 2..32 and the
        // first two code shreds, so both test cases corrupt an input. The last body byte is
        // flipped so that a corrupted code shard only reaches the rebuilt data shreds' padding,
        // past the headers that would be rejected first.
        let missing = 2;
        let (mut payloads, _) = build(BatchPosition::Interior);
        let mut corrupted = payloads[index].to_vec();
        let last_body_byte = match index < DATA_SHREDS {
            true => ShredViewMut::<Data>::new(&mut corrupted, ShredVariant::data(false))
                .unwrap()
                .body_mut()
                .last_mut()
                .map(|byte| *byte ^= 0xFF),
            false => ShredViewMut::<Code>::new(&mut corrupted, ShredVariant::code(false))
                .unwrap()
                .body_mut()
                .last_mut()
                .map(|byte| *byte ^= 0xFF),
        };
        last_body_byte.expect("the body is not empty");
        payloads[index] = Bytes::from(corrupted);
        assert_matches!(
            rebuild_fec_set(&payloads[missing..]),
            Err(RecoverError::RootMismatch)
        );
    }

    #[test_case(|header| header.num_data_shreds = 0; "data count")]
    #[test_case(|header| header.num_code_shreds = 33; "code count")]
    #[test_case(|header| header.position = header.position.wrapping_add(1); "position")]
    #[test_case(|header| header.position = 40; "position past the batch")]
    fn misplaced_code_survivor(edit: fn(&mut CodeHeader)) {
        let (payloads, _) = build(BatchPosition::Interior);
        let mut survivors = payloads[DATA_SHREDS..].to_vec();
        survivors[2] = edit_headers::<Code>(&survivors[2], |_, header| edit(header));
        assert_matches!(
            rebuild_fec_set(&survivors),
            Err(RecoverError::MisplacedShred { .. })
        );
    }

    #[test]
    fn data_survivor_among_code_shards() {
        let (payloads, _) = build(BatchPosition::Interior);
        let mut survivors = payloads[..DATA_SHREDS].to_vec();
        survivors[4] = edit_headers::<Data>(&survivors[4], |common, _| {
            common.index = FEC_SET_INDEX.wrapping_add(40);
        });
        assert_matches!(
            rebuild_fec_set(&survivors),
            Err(RecoverError::MisplacedShred {
                index: 104,
                fec_set_index: FEC_SET_INDEX
            })
        );
    }

    #[test_case(|common| common.index = common.index.wrapping_add(1); "index")]
    #[test_case(|common| common.slot = common.slot.wrapping_add(1); "slot")]
    #[test_case(|common| common.version = common.version.wrapping_add(1); "version")]
    #[test_case(|common| common.fec_set_index = 0; "fec set index")]
    #[test_case(|common| common.variant = ShredVariant::data(true); "resigned bit")]
    fn leader_signed_data_shred_in_the_wrong_place(edit: fn(&mut CommonHeader)) {
        // The leader builds the codes over a data shred whose headers disagree with its place in
        // the batch, and that data shred is the one that has to be rebuilt.
        let missing = 5;
        let (mut payloads, _) = build(BatchPosition::Interior);
        payloads[missing] = edit_headers::<Data>(&payloads[missing], |common, _| edit(common));
        let payloads = reencode(&payloads, false);
        let survivors: Vec<Bytes> = payloads
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != missing)
            .map(|(_, payload)| payload.clone())
            .collect();
        assert_matches!(
            rebuild_fec_set(&survivors),
            Err(RecoverError::InvalidRebuiltShred { index: 5 })
        );
    }
}
