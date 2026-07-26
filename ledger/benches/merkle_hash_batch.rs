//! Isolates the cost of Merkle hashing for one 32:32 erasure batch.
//!
//! Every row runs in the same binary so the comparison is apples-to-apples.
//! The `upstream_*` rows reimplement the pre-`hash_batch` code path locally, so
//! the baseline does not require checking out a different revision.

use {
    criterion::{criterion_group, criterion_main, BatchSize, Criterion},
    rand::Rng,
    solana_hash::Hash,
    solana_ledger::shred::{
        hash_batch,
        merkle_tree::{MerkleTree, SIZE_OF_MERKLE_PROOF_ENTRY},
    },
    solana_sha256_hasher::hashv,
    std::{hint::black_box, iter::repeat_with},
};

// Mirrors `shred::merkle_tree::{MERKLE_HASH_PREFIX_LEAF, MERKLE_HASH_PREFIX_NODE}`,
// which are crate-private.
const MERKLE_HASH_PREFIX_LEAF: &[u8] = b"\x00SOLANA_MERKLE_SHREDS_LEAF";
const MERKLE_HASH_PREFIX_NODE: &[u8] = b"\x01SOLANA_MERKLE_SHREDS_NODE";

// One 32:32 erasure batch.
const SHREDS_PER_FEC_BLOCK: usize = 64;
// Merkle leaf bytes per shred: erasure shard plus the chained merkle root.
const MERKLE_LEAF_SIZE: usize = 987 + 32;

/// The interior-level loop exactly as it was before `hash_batch`: every join
/// hashed one at a time through `hashv`.
fn upstream_tree(leaves: &[Hash]) -> Hash {
    let mut nodes: Vec<Hash> = leaves.to_vec();
    let len = leaves.len();
    let init = (len > 1).then_some(len);
    for size in std::iter::successors(init, |&k| (k > 2).then_some((k + 1) >> 1)) {
        let offset = nodes.len() - size;
        for index in (offset..offset + size).step_by(2) {
            let node = &nodes[index];
            let other = &nodes[(index + 1).min(offset + size - 1)];
            let parent = hashv(&[
                MERKLE_HASH_PREFIX_NODE,
                &node.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY],
                &other.as_ref()[..SIZE_OF_MERKLE_PROOF_ENTRY],
            ]);
            nodes.push(parent);
        }
    }
    *nodes.last().unwrap()
}

fn bench_merkle(c: &mut Criterion) {
    let mut rng = rand::rng();
    let leaves: Vec<Vec<u8>> = (0..SHREDS_PER_FEC_BLOCK)
        .map(|_| repeat_with(|| rng.random()).take(MERKLE_LEAF_SIZE).collect())
        .collect();
    let leaf_refs: Vec<&[u8]> = leaves.iter().map(Vec::as_slice).collect();

    let mut group = c.benchmark_group("merkle_hash_batch");

    // --- Leaf level: 64 independent ~1KB hashes. ---
    group.bench_function("leaves_upstream", |b| {
        b.iter(|| {
            let nodes: Vec<Hash> = leaf_refs
                .iter()
                .map(|leaf| hashv(&[MERKLE_HASH_PREFIX_LEAF, leaf]))
                .collect();
            black_box(nodes);
        })
    });
    group.bench_function("leaves_seam", |b| {
        b.iter(|| {
            let mut nodes = vec![[0u8; 32]; leaf_refs.len()];
            hash_batch::hash_many_prefixed(MERKLE_HASH_PREFIX_LEAF, &leaf_refs, &mut nodes);
            black_box(nodes);
        })
    });

    // --- Interior levels only, leaves hashed once outside the timed region. ---
    let prehashed: Vec<Hash> = leaf_refs
        .iter()
        .map(|leaf| hashv(&[MERKLE_HASH_PREFIX_LEAF, leaf]))
        .collect();
    group.bench_function("joins_upstream", |b| {
        b.iter_batched(
            || prehashed.clone(),
            |leaves| black_box(upstream_tree(&leaves)),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("joins_seam", |b| {
        b.iter(|| {
            let nodes = prehashed.iter().copied().map(Ok);
            let tree = MerkleTree::try_new_with_len(nodes, SHREDS_PER_FEC_BLOCK).unwrap();
            black_box(*tree.root());
        })
    });

    // --- Whole tree: what shredding and recovery actually pay. ---
    group.bench_function("tree_upstream", |b| {
        b.iter(|| {
            let leaves: Vec<Hash> = leaf_refs
                .iter()
                .map(|leaf| hashv(&[MERKLE_HASH_PREFIX_LEAF, leaf]))
                .collect();
            black_box(upstream_tree(&leaves));
        })
    });
    group.bench_function("tree_seam", |b| {
        b.iter(|| {
            let mut digests = vec![[0u8; 32]; leaf_refs.len()];
            hash_batch::hash_many_prefixed(MERKLE_HASH_PREFIX_LEAF, &leaf_refs, &mut digests);
            let nodes = digests.into_iter().map(Hash::new_from_array).map(Ok);
            let tree = MerkleTree::try_new_with_len(nodes, SHREDS_PER_FEC_BLOCK).unwrap();
            black_box(*tree.root());
        })
    });

    group.finish();
}

criterion_group!(benches, bench_merkle);
criterion_main!(benches);
