//! The bytes this crate writes are the bytes the cluster already runs on.
//!
//! A wire-format writer can only be judged against the implementation it replaces. The vectors in
//! `golden/` are the payloads `solana-ledger`'s shredder produced for these two batches, captured
//! before that shredder was deleted, and this builds the same batches here and compares all 64
//! payloads of each byte for byte.
//!
//! # Regenerating
//!
//! Don't, unless the wire format itself is meant to change: a mismatch here is a compatibility
//! break, not a stale fixture. The vectors were produced by `Shredder::make_shreds_from_data_slice`
//! on the commit before this crate replaced it, with the parameters below, data shreds first.

use {
    agave_shred::shredder::{BatchPosition, FecSet, FecSetSpec, SHARDS},
    solana_hash::Hash,
    solana_keypair::Keypair,
};

const SLOT: u64 = 1_000;
const PARENT_SLOT: u64 = 998;
const VERSION: u16 = 42;
const REFERENCE_TICK: u8 = 9;
const FEC_SET_INDEX: u32 = 96;

/// The batch that ends a run of entries, and the one that ends the slot.
const BATCH: &[u8] = include_bytes!("golden/batch.bin");
const BATCH_LAST_IN_SLOT: &[u8] = include_bytes!("golden/batch_last_in_slot.bin");

fn keypair() -> Keypair {
    Keypair::new_from_array([7u8; 32])
}

fn spec(batch_position: BatchPosition) -> FecSetSpec {
    FecSetSpec {
        slot: SLOT,
        parent_slot: PARENT_SLOT,
        version: VERSION,
        reference_tick: REFERENCE_TICK,
        fec_set_index: FEC_SET_INDEX,
        chained_merkle_root: Hash::new_from_array([5u8; 32]),
        batch_position,
    }
}

/// Splits a vector file into its payloads, each stored behind its `u32` length.
fn payloads(mut vectors: &[u8]) -> Vec<&[u8]> {
    let mut payloads = Vec::with_capacity(SHARDS);
    while !vectors.is_empty() {
        let (len, rest) = vectors
            .split_first_chunk::<4>()
            .expect("a vector file is a sequence of length-prefixed payloads");
        let len = u32::from_le_bytes(*len) as usize;
        let (payload, rest) = rest
            .split_at_checked(len)
            .expect("each length prefix is followed by that many bytes");
        payloads.push(payload);
        vectors = rest;
    }
    payloads
}

fn run(batch_position: BatchPosition, vectors: &[u8]) {
    // One batch's worth of data, ending mid-shred so the padding is exercised too.
    let data: Vec<u8> = (0..20_000u32).map(|index| index as u8).collect();
    let built = FecSet::build(&spec(batch_position), &data, &keypair()).expect("the spec is legal");
    let built: Vec<&[u8]> = built
        .data
        .iter()
        .map(|shred| shred.bytes().as_ref())
        .chain(built.code.iter().map(|shred| shred.bytes().as_ref()))
        .collect();

    let expected = payloads(vectors);
    assert_eq!(expected.len(), SHARDS, "a batch is {SHARDS} shreds");
    assert_eq!(built.len(), expected.len());
    for (index, (built, expected)) in built.iter().zip(&expected).enumerate() {
        assert_eq!(
            built, expected,
            "shard {index} of a {batch_position:?} batch differs from what the cluster's shredder \
             produced",
        );
    }
}

#[test]
fn data_complete_batch_matches_the_golden_payloads() {
    run(BatchPosition::DataComplete, BATCH);
}

#[test]
fn last_in_slot_batch_matches_the_golden_payloads() {
    run(BatchPosition::LastInSlot, BATCH_LAST_IN_SLOT);
}
