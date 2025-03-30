#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]
extern crate test;
use {
    solana_core::{
        cluster_slots_service::cluster_slots::ClusterSlots, replay_stage::DUPLICATE_THRESHOLD,
    },
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
    },
    test::{black_box, Bencher},
};

const LOOKUP_SLOTS_TO_SIMULATE: usize = 200;
const NUM_SLOTS_PER_EPOCH_SLOTS: u64 = 50;
const NUM_NODES: usize = 1000;
//handy for running with profiler like flamegraph
const NUM_INNER_LOOPS: usize = 1;
const STAKE_PER_NODE: u64 = 42;
const REPAIR_THREADS: usize = 4;
fn generate_stakes(num_nodes: usize) -> HashMap<Pubkey, u64> {
    let nodes: Vec<_> = (0..num_nodes).map(|_| Pubkey::new_unique()).collect();
    let stakes = HashMap::from_iter(nodes.iter().map(|e| (*e, STAKE_PER_NODE)));
    stakes
}

#[bench]
fn bench_cluster_slots_update_new_and_fast(bencher: &mut Bencher) {
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    let fav_stake: Vec<_> = stakes.keys().take(1).collect();
    //warmup
    cs.generate_fill_for_tests(&stakes, 0, 1..10);
    let mut cur_slot = 0;
    bencher.iter(|| {
        for _ in 0..NUM_INNER_LOOPS {
            cs.generate_fill_for_tests(
                &stakes,
                cur_slot,
                (cur_slot + 2)..(cur_slot + NUM_SLOTS_PER_EPOCH_SLOTS + 2),
            );
            let rl = cs.lookup(cur_slot + 3).unwrap();
            let rl = rl.read().unwrap();
            assert!(rl.contains_key(fav_stake[0]));
            cur_slot += 1;
        }
    });
    dbg!(cur_slot);
    dbg!(cs.total_writes.load(Ordering::Relaxed));
    dbg!(cs.total_allocations.load(Ordering::Relaxed));
}

#[bench]
fn bench_cluster_slots_lookup_new(bencher: &mut Bencher) {
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    cs.generate_fill_for_tests(&stakes, 0, 1..NUM_SLOTS_PER_EPOCH_SLOTS);
    let mut cur_slot = 0;
    bencher.iter(|| {
        let res = cs.lookup(cur_slot);
        black_box(res);
        cur_slot = (cur_slot + 1) % NUM_SLOTS_PER_EPOCH_SLOTS;
    })
}


#[bench]
fn bench_cluster_slots_contested_lookup(bencher: &mut Bencher) {
    let slot_start = std::sync::Barrier::new(REPAIR_THREADS + 1);
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    println!("Initialization lookup new");
    cs.generate_fill_for_tests(&stakes, 0, 1..NUM_SLOTS_PER_EPOCH_SLOTS);
    println!("Initialization done, this bench takes a while!");
    let cur_slot = AtomicU64::new(0);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                    slot_start.wait();
                    let slot = cur_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    cs.generate_fill_for_tests(
                        &stakes,
                        slot,
                        (slot + 1)..(slot + NUM_SLOTS_PER_EPOCH_SLOTS),
                    );
                }
            });
            for _ in 0..REPAIR_THREADS {
                scope.spawn(|| {
                    let mut sum = 0;
                    for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                        slot_start.wait();
                        let slot = cur_slot.load(std::sync::atomic::Ordering::SeqCst);
                        let lookup_slot = slot + 5;
                        //assume we are repairing 10 shreds per slot
                        for _shred in 0..10 {
                            if let Some(res) = cs.lookup(lookup_slot) {
                                if res.read().unwrap().len() == 0 {
                                    dbg!(lookup_slot);
                                    panic!();
                                }
                                let mut slot_stake = 0;
                                for (_, v) in res.read().unwrap().iter() {
                                    let v = v.load(Ordering::Relaxed);
                                    slot_stake += v;
                                    sum += v;
                                }
                                assert!(
                                    slot_stake as f64
                                        > (STAKE_PER_NODE * NUM_NODES as u64) as f64
                                            * DUPLICATE_THRESHOLD
                                );
                            }
                        }
                    }
                    black_box(sum);
                });
            }
        });
    });
}
