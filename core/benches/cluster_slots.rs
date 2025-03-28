#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]
use frozen_collections::MapQuery;
use jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

extern crate test;
use std::{collections::HashMap, sync::atomic::AtomicU64, sync::atomic::Ordering};

use solana_core::cluster_slots_service::cluster_slots::{ClusterSlots, ClusterSlots2};
use solana_pubkey::Pubkey;
use test::{black_box, Bencher};

const LOOKUP_SLOTS_TO_SIMULATE: usize = 1000;
const NUM_SLOTS_PER_EPOCH_SLOTS: u64 = 100;
const NUM_NODES: usize = 1000;
//handy for running with profiler
const NUM_INNER_LOOPS: usize = 1;

fn generate_stakes(num_nodes: usize) -> HashMap<Pubkey, u64> {
    let nodes: Vec<_> = (0..num_nodes).map(|_| Pubkey::new_unique()).collect();
    let stakes = HashMap::from_iter(nodes.iter().map(|e| (*e, 42)));
    stakes
}

/*
#[bench]
fn bench_cluster_slots_update_original(bencher: &mut Bencher) {
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    cs.generate_fill_for_tests_fp(&stakes, 0, 0..0);
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests_fp(
            &stakes,
            cur_slot,
            (cur_slot + 2)..(cur_slot + NUM_SLOTS_PER_EPOCH_SLOTS + 2),
        );
        black_box(&cs);
        cur_slot += 1;
    })
}*/

#[bench]
fn bench_cluster_slots_update_dash_map(bencher: &mut Bencher) {
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
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
            assert!(!cs.lookup(cur_slot + 3).unwrap().is_empty());

            cur_slot += 1;
        }
    });
    dbg!(cur_slot);
    //dbg!(cs.total_writes.load(Ordering::Relaxed));
}
#[bench]
fn bench_cluster_slots_update_no_fp(bencher: &mut Bencher) {
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
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
            assert!(!cs.lookup(cur_slot + 3).unwrap().read().unwrap().is_empty());

            cur_slot += 1;
        }
    });
    dbg!(cur_slot);
    dbg!(cs.total_writes.load(Ordering::Relaxed));
}
#[bench]
fn bench_cluster_slots_update_new_and_fast(bencher: &mut Bencher) {
    let cs = ClusterSlots2::default();
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
            //dbg!(&rl);

            assert!(rl.contains_key(fav_stake[0]));
            cur_slot += 1;
        }
    });
    dbg!(cur_slot);
    dbg!(cs.total_writes.load(Ordering::Relaxed));
}
/*
#[bench]
fn bench_cluster_slots_lookup(bencher: &mut Bencher) {
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
fn bench_cluster_slots_lookup_old(bencher: &mut Bencher) {
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
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
    let slot_start = std::sync::Barrier::new(2);
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    println!("Initialization");
    cs.generate_fill_for_tests(&stakes, 0, 1..NUM_SLOTS_PER_EPOCH_SLOTS);
    println!("Initialization done");
    let cur_slot = AtomicU64::new(0);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                    slot_start.wait();
                    let slot = cur_slot.load(std::sync::atomic::Ordering::SeqCst);
                    cs.generate_fill_for_tests(
                        &stakes,
                        slot,
                        (slot + 1)..(slot + NUM_SLOTS_PER_EPOCH_SLOTS),
                    );
                    //println!("producer:{slot}");
                }
            });
            scope.spawn(|| {
                let mut sum = 0;
                for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                    let slot = cur_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    slot_start.wait();
                    let lookup_slot = slot + 5;
                    //assume we are repairing 100 shreds per slot
                    for _shred in 0..100 {
                        if let Some(res) = cs.lookup(lookup_slot) {
                            if res.len() == 0 {
                                dbg!(lookup_slot);
                                panic!();
                            }
                            for v in res.iter() {
                                assert!(*v.value() > 0);
                                sum += v.value();
                            }
                        }
                        //println!("consumer:{slot}");
                    }
                }
                black_box(sum);
            });
        });
    })
}

#[bench]
fn bench_cluster_slots_contested_lookup_old(bencher: &mut Bencher) {
    let slot_start = std::sync::Barrier::new(2);
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(NUM_NODES);
    println!("Initialization");
    cs.generate_fill_for_tests(&stakes, 0, 1..NUM_SLOTS_PER_EPOCH_SLOTS);
    println!("Initialization done");
    let cur_slot = AtomicU64::new(0);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                    slot_start.wait();
                    let slot = cur_slot.load(std::sync::atomic::Ordering::SeqCst);
                    cs.generate_fill_for_tests(
                        &stakes,
                        slot,
                        (slot + 1)..(slot + NUM_SLOTS_PER_EPOCH_SLOTS),
                    );
                    //println!("producer:{slot}");
                }
            });
            scope.spawn(|| {
                let mut sum = 0;
                for _ in 0..LOOKUP_SLOTS_TO_SIMULATE {
                    let slot = cur_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    slot_start.wait();
                    let lookup_slot = slot + 5;
                    //assume we are repairing 100 shreds per slot
                    for _shred in 0..100 {
                        if let Some(res) = cs.lookup(lookup_slot) {
                            if res.read().unwrap().len() == 0 {
                                dbg!(lookup_slot);
                                panic!();
                            }
                            for v in res.read().unwrap().iter() {
                                assert!(*v.1 > 0);
                                sum += v.1;
                            }
                        }
                    }
                    //println!("consumer:{slot}");
                }
                black_box(sum);
            });
        });
    })
}
*/
