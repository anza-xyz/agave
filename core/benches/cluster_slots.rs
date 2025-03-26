#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]

extern crate test;
use std::collections::HashMap;

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
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests(&stakes, cur_slot, cur_slot + 2..num_slots_per_update + 2);
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
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests_fp(&stakes, cur_slot, cur_slot + 2..num_slots_per_update + 2);
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
    let mut cur_slot = 0;
    bencher.iter(|| {
        cs.generate_fill_for_tests(&stakes, cur_slot, cur_slot + 2..num_slots_per_update + 2);
        black_box(&cs);
        cur_slot += 1;
    })
}
/*
#[bench]
fn bench_cluster_slots_lookup(bencher: &mut Bencher) {
    let slot_start = std::sync::Barrier::new(2);
    let num_slots_per_update = 1000;
    let slots_to_simulate = 100;
    let num_nodes = 2000;
    let mut cs = ClusterSlots::default();
    let stakes = generate_stakes(num_nodes);
    bencher.iter(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for cur_slot in 0..slots_to_simulate {
                    slot_start.wait();
                    cs.generate_fill_for_tests(&stakes, cur_slot..num_slots_per_update);
                }
            });
            scope.spawn(|| {
                let mut sum = 0;
                for cur_slot in 0..slots_to_simulate {
                    slot_start.wait();
                    let lookup_slot = (cur_slot + 5);
                    if let Some(res) = cs.lookup(lookup_slot) {
                        for v in res.iter() {
                            sum += v.value();
                        }
                    }
                }
                dbg!(sum);
            });
        });
    })
}
*/
