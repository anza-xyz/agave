extern crate solana_core;

use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    solana_core::{
        consensus::{tower_storage::FileTowerStorage, Tower},
        vote_simulator::VoteSimulator,
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    },
    tempfile::TempDir,
    trees::tr,
};

fn bench_save_tower(b: &mut Bencher) {
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

    b.iter(|| {
        tower.save(&tower_storage, &node_keypair).unwrap();
    });
}

fn bench_generate_ancestors_descendants(b: &mut Bencher) {
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

    b.iter(|| {
        for _ in 0..num_banks {
            let _ancestors = vote_simulator.bank_forks.read().unwrap().ancestors();
            let _descendants = vote_simulator.bank_forks.read().unwrap().descendants();
        }
    })
}

benchmark_group!(
    benches,
    bench_save_tower,
    bench_generate_ancestors_descendants
);
benchmark_main!(benches);
