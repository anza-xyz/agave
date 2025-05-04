extern crate solana_core;

use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_core::{
        consensus::{tower_storage::FileTowerStorage, Tower},
        vote_simulator::VoteSimulator,
    },
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk::{
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    },
    tempfile::TempDir,
    trees::tr,
};

fn bench_save_tower(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();

    let vote_account_pubkey = &Pubkey::default();
    let node_keypair = Arc::new(Keypair::new());
    let heaviest_bank = BankForks::new_rw_arc(Bank::default_for_tests())
        .read()
        .unwrap()
        .working_bank();
    let tower_storage = FileTowerStorage::new(dir.path().to_path_buf());
    let tower = Tower::new(
        &node_keypair.pubkey(),
        vote_account_pubkey,
        0,
        &heaviest_bank,
    );

    c.bench_function("save_tower", |b| {
        b.iter(|| {
            tower.save(&tower_storage, &node_keypair).unwrap();
        });
    });
}

fn bench_generate_ancestors_descendants(c: &mut Criterion) {
    let vote_account_pubkey = &Pubkey::default();
    let node_keypair = Arc::new(Keypair::new());
    let heaviest_bank = BankForks::new_rw_arc(Bank::default_for_tests())
        .read()
        .unwrap()
        .working_bank();
    let mut tower = Tower::new(
        &node_keypair.pubkey(),
        vote_account_pubkey,
        0,
        &heaviest_bank,
    );

    let num_banks = 500;
    let forks = tr(0);
    let mut vote_simulator = VoteSimulator::new(2);
    vote_simulator.fill_bank_forks(forks, &HashMap::new(), true);
    vote_simulator.create_and_vote_new_branch(
        0,
        num_banks,
        &HashMap::new(),
        &HashSet::new(),
        &Pubkey::new_unique(),
        &mut tower,
    );

    c.bench_function("generate_ancestors_descendants", |b| {
        b.iter(|| {
            for _ in 0..num_banks {
                let _ancestors = vote_simulator.bank_forks.read().unwrap().ancestors();
                let _descendants = vote_simulator.bank_forks.read().unwrap().descendants();
            }
        })
    });
}

criterion_group!(benches, bench_save_tower, bench_generate_ancestors_descendants);
criterion_main!(benches);
