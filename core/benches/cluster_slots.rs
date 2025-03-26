#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]

extern crate test;
use std::{collections::HashMap, sync::atomic::AtomicU64};

use solana_core::cluster_slots_service::cluster_slots::ClusterSlots;
use solana_pubkey::Pubkey;
use test::{black_box, Bencher};

fn generate_stakes(num_nodes: usize) -> HashMap<Pubkey, u64> {
    let nodes: Vec<_> = (0..num_nodes).map(|_| Pubkey::new_unique()).collect();
    let stakes = HashMap::from_iter(nodes.iter().map(|e| (*e, 42)));
    stakes
}
#[bench]
fn bench_cluster_slots_update(bencher: &mut Bencher) {
    let num_slots_per_update = 100;
    let num_nodes = 2000;
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    cs.generate_fill_for_tests(&stakes, 0, 0..0);
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests(
            &stakes,
            cur_slot,
            (cur_slot + 2)..(cur_slot + num_slots_per_update + 2),
        );
        black_box(&cs);
        cur_slot += 1;
    })
}
#[bench]
fn bench_cluster_slots_update_original(bencher: &mut Bencher) {
    let num_slots_per_update = 100;
    let num_nodes = 2000;
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    cs.generate_fill_for_tests_fp(&stakes, 0, 0..0);
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests_fp(
            &stakes,
            cur_slot,
            (cur_slot + 2)..(cur_slot + num_slots_per_update + 2),
        );
        black_box(&cs);
        cur_slot += 1;
    })
}
#[bench]
fn bench_cluster_slots_update_no_fp(bencher: &mut Bencher) {
    let num_slots_per_update = 100;
    let num_nodes = 2000;
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    cs.generate_fill_for_tests(&stakes, 0, 0..0);
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests(
            &stakes,
            cur_slot,
            (cur_slot + 2)..(cur_slot + num_slots_per_update + 2),
        );
        black_box(&cs);
        cur_slot += 1;
    })
}

#[bench]
fn bench_cluster_slots_lookup(bencher: &mut Bencher) {
    let slot_start = std::sync::Barrier::new(2);
    let num_slots_per_update = 10;
    let slots_to_simulate = 10;
    let num_nodes = 20;
    let cs = ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    println!("Initialization");
    cs.generate_fill_for_tests(&stakes, 0, 1..num_slots_per_update);
    println!("Initialization done");
    let cur_slot = AtomicU64::new(0);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..slots_to_simulate {
                    slot_start.wait();
                    let slot = cur_slot.load(std::sync::atomic::Ordering::SeqCst);
                    cs.generate_fill_for_tests(
                        &stakes,
                        slot,
                        (slot + 1)..(slot + num_slots_per_update),
                    );
                    //println!("producer:{slot}");
                }
            });
            scope.spawn(|| {
                let mut sum = 0;
                for _ in 0..slots_to_simulate {
                    let slot = cur_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    slot_start.wait();
                    let lookup_slot = slot + 5;
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
                black_box(sum);
            });
        });
    })
}

#[bench]
fn bench_cluster_slots_lookup_old_fp(bencher: &mut Bencher) {
    let slot_start = std::sync::Barrier::new(2);
    let num_slots_per_update = 10;
    let slots_to_simulate = 10;
    let num_nodes = 20;
    let cs = solana_core::cluster_slots_service::cluster_slots_old::ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    println!("Initialization");
    cs.generate_fill_for_tests_fp(&stakes, 0, 1..num_slots_per_update);
    println!("Initialization done");
    let cur_slot = AtomicU64::new(0);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..slots_to_simulate {
                    slot_start.wait();
                    let slot = cur_slot.load(std::sync::atomic::Ordering::SeqCst);
                    cs.generate_fill_for_tests_fp(
                        &stakes,
                        slot,
                        (slot + 1)..(slot + num_slots_per_update),
                    );
                    //println!("producer:{slot}");
                }
            });
            scope.spawn(|| {
                let mut sum = 0;
                for _ in 0..slots_to_simulate {
                    let slot = cur_slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    slot_start.wait();
                    let lookup_slot = slot + 5;
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
                    //println!("consumer:{slot}");
                }
                black_box(sum);
            });
        });
    })
}
