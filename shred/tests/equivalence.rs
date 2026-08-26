//! The bytes this crate writes are the bytes `solana-ledger`'s shredder writes.
//!
//! A wire-format writer can only be judged against the implementation the cluster already runs, so
//! this builds the same erasure batch both ways (same keypair, same data, same chained root) and
//! compares all 64 payloads byte for byte.

use {
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    solana_shred::shredder::{BatchPosition, FecSet, FecSetSpec},
};

const SLOT: u64 = 1_000;
const PARENT_SLOT: u64 = 998;
const VERSION: u16 = 42;
const REFERENCE_TICK: u8 = 9;
const FEC_SET_INDEX: u32 = 96;

fn spec(last_in_slot: bool) -> FecSetSpec {
    FecSetSpec {
        slot: SLOT,
        parent_slot: PARENT_SLOT,
        version: VERSION,
        reference_tick: REFERENCE_TICK,
        fec_set_index: FEC_SET_INDEX,
        chained_merkle_root: Hash::new_from_array([5u8; 32]),
        // The incumbent shredder marks the last data shred of the data it was handed, so the one
        // batch built here is the batch that ends: the slot, or just this run of entries.
        batch_position: match last_in_slot {
            true => BatchPosition::LastInSlot,
            false => BatchPosition::DataComplete,
        },
    }
}

/// What the incumbent shredder produces for the same batch, data shreds first.
fn incumbent(data: &[u8], last_in_slot: bool) -> Vec<Shred> {
    let shredder = Shredder::new(SLOT, PARENT_SLOT, REFERENCE_TICK, VERSION).unwrap();
    let (mut data_shreds, code_shreds): (Vec<_>, Vec<_>) = shredder
        .make_shreds_from_data_slice(
            &keypair(),
            data,
            last_in_slot,
            spec(last_in_slot).chained_merkle_root,
            FEC_SET_INDEX,
            FEC_SET_INDEX,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .unwrap()
        .partition(Shred::is_data);
    data_shreds.extend(code_shreds);
    data_shreds
}

fn keypair() -> Keypair {
    Keypair::new_from_array([7u8; 32])
}

#[test]
fn payloads_match_the_incumbent_shredder() {
    // One batch's worth of data, ending mid-shred so the padding is exercised too.
    let data: Vec<u8> = (0..20_000u32).map(|index| index as u8).collect();
    for last_in_slot in [false, true] {
        let spec = spec(last_in_slot);
        let ours = FecSet::build(&spec, &data, &keypair()).unwrap();
        let theirs = incumbent(&data, last_in_slot);

        let ours: Vec<&[u8]> = ours
            .data
            .iter()
            .map(|shred| shred.bytes().as_ref())
            .chain(ours.code.iter().map(|shred| shred.bytes().as_ref()))
            .collect();
        assert_eq!(ours.len(), theirs.len());
        for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
            assert_eq!(
                *ours,
                theirs.payload().as_ref(),
                "shard {index} of a batch with last_in_slot={last_in_slot} differs",
            );
        }
    }
}
