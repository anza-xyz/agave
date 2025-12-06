#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]

use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    solana_core::{
        banking_stage::transaction_scheduler::scheduler_controller::SchedulerConfig,
        banking_trace::Channels,
        validator::{BlockProductionMethod, SchedulerPacing},
    },
    solana_vote::vote_transaction::new_tower_sync_transaction,
    solana_vote_program::vote_state::TowerSync,
    tokio::sync::mpsc,
};

extern crate test;

use {
    crossbeam_channel::{unbounded, Receiver},
    log::*,
    rand::{thread_rng, Rng},
    rayon::prelude::*,
    solana_core::{banking_stage::BankingStage, banking_trace::BankingTracer},
    solana_entry::entry::{next_hash, Entry},
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_processor::process_entries_for_tests,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        get_tmp_ledger_path_auto_delete,
    },
    solana_message::Message,
    solana_perf::packet::to_packet_batches,
    solana_poh::poh_recorder::{create_test_recorder, WorkingBankEntry},
    solana_pubkey as pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_system_transaction as system_transaction,
    solana_time_utils::timestamp,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
    std::{
        iter::repeat_with,
        sync::{atomic::Ordering, Arc},
        time::{Duration, Instant},
    },
    test::Bencher,
};

fn check_txs(receiver: &Arc<Receiver<WorkingBankEntry>>, ref_tx_count: usize) {
    let mut total = 0;
    let now = Instant::now();
    loop {
        if let Ok((_bank, (entry, _tick_height))) = receiver.recv_timeout(Duration::new(1, 0)) {
            total += entry.transactions.len();
        }
        if total >= ref_tx_count {
            break;
        }
        if now.elapsed().as_secs() > 60 {
            break;
        }
    }
    assert_eq!(total, ref_tx_count);
}

fn make_accounts_txs(txes: usize, mint_keypair: &Keypair, hash: Hash) -> Vec<Transaction> {
    let to_pubkey = pubkey::new_rand();
    let dummy = system_transaction::transfer(mint_keypair, &to_pubkey, 1, hash);
    (0..txes)
        .into_par_iter()
        .map(|_| {
            let mut new = dummy.clone();
            let sig: [u8; 64] = std::array::from_fn(|_| thread_rng().gen::<u8>());
            new.message.account_keys[0] = pubkey::new_rand();
            new.message.account_keys[1] = pubkey::new_rand();
            new.signatures = vec![Signature::from(sig)];
            new
        })
        .collect()
}

fn make_programs_txs(txes: usize, hash: Hash) -> Vec<Transaction> {
    let progs = 4;
    (0..txes)
        .map(|_| {
            let from_key = Keypair::new();
            let instructions: Vec<_> = repeat_with(|| {
                let to_key = pubkey::new_rand();
                system_instruction::transfer(&from_key.pubkey(), &to_key, 1)
            })
            .take(progs)
            .collect();
            let message = Message::new(&instructions, Some(&from_key.pubkey()));
            Transaction::new(&[&from_key], message, hash)
        })
        .collect()
}

fn make_vote_txs(txes: usize) -> Vec<Transaction> {
    // 1000 voters
    let num_voters = 1000;
    let (keypairs, vote_keypairs): (Vec<_>, Vec<_>) = (0..num_voters)
        .map(|_| (Keypair::new(), Keypair::new()))
        .unzip();
    (0..txes)
        .map(|i| {
            // Quarter of the votes should be filtered out
            let vote = if i % 4 == 0 {
                TowerSync::from(vec![(2, 1)])
            } else {
                TowerSync::from(vec![(i as u64, 1)])
            };
            new_tower_sync_transaction(
                vote,
                Hash::new_unique(),
                &keypairs[i % num_voters],
                &vote_keypairs[i % num_voters],
                &vote_keypairs[i % num_voters],
                None,
            )
        })
        .collect()
}

enum TransactionType {
    Accounts,
    Programs,
    AccountsAndVotes,
    ProgramsAndVotes,
}

fn bench_banking(
    bencher: &mut Bencher,
    tx_type: TransactionType,
    block_production_method: BlockProductionMethod,
) {
    agave_logger::setup();
    let num_threads = BankingStage::default_num_workers();
    //   a multiple of packet chunk duplicates to avoid races
    const CHUNKS: usize = 8;
    const PACKETS_PER_BATCH: usize = 192;
    let txes = PACKETS_PER_BATCH * num_threads.get() * CHUNKS;
    let mint_total = 1_000_000_000_000;
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(mint_total);

    // Set a high ticks_per_slot so we don't run out of ticks
    // during the benchmark
    genesis_config.ticks_per_slot = 10_000;

    let banking_tracer = BankingTracer::new_disabled();
    let Channels {
        non_vote_sender,
        non_vote_receiver,
        tpu_vote_sender,
        tpu_vote_receiver,
        gossip_vote_sender,
        gossip_vote_receiver,
    } = banking_tracer.create_channels();

    let mut bank = Bank::new_for_benches(&genesis_config);
    // Allow arbitrary transaction processing time for the purposes of this bench
    bank.ns_per_slot = u128::MAX;
    let bank_forks = BankForks::new_rw_arc(bank);
    let bank = bank_forks.read().unwrap().get(0).unwrap();

    // set cost tracker limits to MAX so it will not filter out TXs
    bank.write_cost_tracker()
        .unwrap()
        .set_limits(u64::MAX, u64::MAX, u64::MAX);

    debug!("threads: {num_threads} txs: {txes}");

    let transactions = match tx_type {
        TransactionType::Accounts | TransactionType::AccountsAndVotes => {
            make_accounts_txs(txes, &mint_keypair, genesis_config.hash())
        }
        TransactionType::Programs | TransactionType::ProgramsAndVotes => {
            make_programs_txs(txes, genesis_config.hash())
        }
    };
    let vote_txs = match tx_type {
        TransactionType::AccountsAndVotes | TransactionType::ProgramsAndVotes => {
            Some(make_vote_txs(txes))
        }
        _ => None,
    };

    // fund all the accounts
    transactions.iter().for_each(|tx| {
        let fund = system_transaction::transfer(
            &mint_keypair,
            &tx.message.account_keys[0],
            mint_total / txes as u64,
            genesis_config.hash(),
        );
        let x = bank.process_transaction(&fund);
        x.unwrap();
    });
    //sanity check, make sure all the transactions can execute sequentially
    transactions.iter().for_each(|tx| {
        let res = bank.process_transaction(tx);
        assert!(res.is_ok(), "sanity test transactions");
    });
    bank.clear_signatures();
    //sanity check, make sure all the transactions can execute in parallel
    let res = bank.process_transactions(transactions.iter());
    for r in res {
        assert!(r.is_ok(), "sanity parallel execution");
    }
    bank.clear_signatures();
    let verified: Vec<_> = to_packet_batches(&transactions, PACKETS_PER_BATCH);
    let vote_packets = vote_txs.map(|vote_txs| {
        let mut packet_batches = to_packet_batches(&vote_txs, PACKETS_PER_BATCH);
        for batch in packet_batches.iter_mut() {
            for mut packet in batch.iter_mut() {
                packet.meta_mut().set_simple_vote(true);
            }
        }
        packet_batches
    });

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Arc::new(
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger"),
    );
    let (exit, poh_recorder, _poh_controller, transaction_recorder, poh_service, signal_receiver) =
        create_test_recorder(bank.clone(), blockstore, None, None);
    let (s, _r) = unbounded();
    let _banking_stage = BankingStage::new_num_threads(
        block_production_method,
        poh_recorder.clone(),
        transaction_recorder,
        non_vote_receiver,
        tpu_vote_receiver,
        gossip_vote_receiver,
        mpsc::channel(1).1,
        num_threads,
        SchedulerConfig {
            scheduler_pacing: SchedulerPacing::Disabled,
        },
        None,
        s,
        None,
        bank_forks,
        None,
    );

    let chunk_len = verified.len() / CHUNKS;
    let mut start = 0;

    // This is so that the signal_receiver does not go out of scope after the closure.
    // If it is dropped before poh_service, then poh_service will error when
    // calling send() on the channel.
    let signal_receiver = Arc::new(signal_receiver);
    let signal_receiver2 = signal_receiver;
    bencher.iter(move || {
        let now = Instant::now();
        let mut sent = 0;
        if let Some(vote_packets) = &vote_packets {
            tpu_vote_sender
                .send(BankingPacketBatch::new(
                    vote_packets[start..start + chunk_len].to_vec(),
                ))
                .unwrap();
            gossip_vote_sender
                .send(BankingPacketBatch::new(
                    vote_packets[start..start + chunk_len].to_vec(),
                ))
                .unwrap();
        }
        for v in verified[start..start + chunk_len].chunks(chunk_len / num_threads) {
            debug!(
                "sending... {}..{} {} v.len: {}",
                start,
                start + chunk_len,
                timestamp(),
                v.len(),
            );
            for xv in v {
                sent += xv.len();
            }
            non_vote_sender
                .send(BankingPacketBatch::new(v.to_vec()))
                .unwrap();
        }

        check_txs(&signal_receiver2, txes / CHUNKS);

        // This signature clear may not actually clear the signatures
        // in this chunk, but since we rotate between CHUNKS then
        // we should clear them by the time we come around again to reuse that chunk.
        bank.clear_signatures();
        trace!(
            "time: {} checked: {} sent: {}",
            now.elapsed().as_micros(),
            txes / CHUNKS,
            sent,
        );
        start += chunk_len;
        start %= verified.len();
    });
    exit.store(true, Ordering::Relaxed);
    poh_service.join().unwrap();
}

#[bench]
fn bench_banking_stage_multi_accounts(bencher: &mut Bencher) {
    bench_banking(
        bencher,
        TransactionType::Accounts,
        BlockProductionMethod::CentralScheduler,
    );
}

#[bench]
fn bench_banking_stage_multi_programs(bencher: &mut Bencher) {
    bench_banking(
        bencher,
        TransactionType::Programs,
        BlockProductionMethod::CentralScheduler,
    );
}

#[bench]
fn bench_banking_stage_multi_accounts_with_voting(bencher: &mut Bencher) {
    bench_banking(
        bencher,
        TransactionType::AccountsAndVotes,
        BlockProductionMethod::CentralScheduler,
    );
}

#[bench]
fn bench_banking_stage_multi_programs_with_voting(bencher: &mut Bencher) {
    bench_banking(
        bencher,
        TransactionType::ProgramsAndVotes,
        BlockProductionMethod::CentralScheduler,
    );
}

fn simulate_process_entries(
    mint_keypair: &Keypair,
    mut tx_vector: Vec<VersionedTransaction>,
    genesis_config: &GenesisConfig,
    keypairs: &[Keypair],
    initial_lamports: u64,
    num_accounts: usize,
) {
    let bank = Bank::new_for_benches(genesis_config);
    let slot = bank.slot();
    let bank_fork = BankForks::new_rw_arc(bank);
    let bank = bank_fork.read().unwrap().get_with_scheduler(slot).unwrap();
    bank.clone_without_scheduler()
        .set_fork_graph_in_program_cache(Arc::downgrade(&bank_fork));

    for i in 0..(num_accounts / 2) {
        bank.transfer(initial_lamports, mint_keypair, &keypairs[i * 2].pubkey())
            .unwrap();
    }

    for i in (0..num_accounts).step_by(2) {
        tx_vector.push(
            system_transaction::transfer(
                &keypairs[i],
                &keypairs[i + 1].pubkey(),
                initial_lamports,
                bank.last_blockhash(),
            )
            .into(),
        );
    }

    // Transfer lamports to each other
    let entry = Entry {
        num_hashes: 1,
        hash: next_hash(&bank.last_blockhash(), 1, &tx_vector),
        transactions: tx_vector,
    };
    process_entries_for_tests(&bank, vec![entry], None, None).unwrap();
}

#[bench]
fn bench_process_entries(bencher: &mut Bencher) {
    // entropy multiplier should be big enough to provide sufficient entropy
    // but small enough to not take too much time while executing the test.
    let entropy_multiplier: usize = 25;
    let initial_lamports = 100;

    // number of accounts need to be in multiple of 4 for correct
    // execution of the test.
    let num_accounts = entropy_multiplier * 4;
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config((num_accounts + 1) as u64 * initial_lamports);

    let keypairs: Vec<Keypair> = repeat_with(Keypair::new).take(num_accounts).collect();
    let tx_vector: Vec<VersionedTransaction> = Vec::with_capacity(num_accounts / 2);

    bencher.iter(|| {
        simulate_process_entries(
            &mint_keypair,
            tx_vector.clone(),
            &genesis_config,
            &keypairs,
            initial_lamports,
            num_accounts,
        );
    });
}
