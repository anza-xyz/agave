use {
    criterion::{Criterion, criterion_group, criterion_main},
    solana_core::{
        consensus::{Tower, tower_storage::FileTowerStorage},
        vote_simulator::VoteSimulator,
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::Duration,
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

    c.bench_function("bench_save_tower", |b| {
        b.iter(|| {
            tower.save(&tower_storage, &node_keypair).unwrap();
        })
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

    // One pass per iteration. Repeating it `num_banks` times over an unchanged
    // fork tree only multiplied the cost; the harness picks the iteration count.
    c.bench_function("bench_generate_ancestors_descendants", |b| {
        b.iter(|| {
            let bank_forks = vote_simulator.bank_forks.read().unwrap();
            (bank_forks.ancestors(), bank_forks.descendants())
        })
    });
}

criterion_group! {
    name = benches;
    // Trim criterion's defaults: both benches are dominated by a fixture that
    // is built once, and neither needs a long window to settle.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750))
        .sample_size(10)
        .without_plots();
    targets = bench_save_tower, bench_generate_ancestors_descendants
}
criterion_main!(benches);
