use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    crossbeam_channel::{unbounded, Receiver},
    rand::prelude::*,
    solana_core::banking_stage::{
        decision_maker::BufferedPacketsDecision,
        packet_deserializer::PacketDeserializer,
        transaction_scheduler::{
            receive_and_buffer::{
                ReceiveAndBuffer, SanitizedTransactionReceiveAndBuffer,
                TransactionViewReceiveAndBuffer,
            },
            scheduler_metrics::{SchedulerCountMetrics, SchedulerTimingMetrics},
            transaction_state_container::StateContainer,
        },
    },
    solana_keypair::Keypair,
    solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
    solana_perf::packet::{to_packet_batches, PacketBatch, NUM_PACKETS},
    solana_poh::poh_recorder::BankStart,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk::{
        account::AccountSharedData,
        compute_budget::ComputeBudgetInstruction,
        genesis_config::GenesisConfig,
        hash::Hash,
        instruction::{AccountMeta, Instruction},
        message::{Message, VersionedMessage},
        signer::Signer,
        transaction::VersionedTransaction,
    },
    solana_sdk_ids::system_program,
    std::{
        sync::{Arc, RwLock},
        time::Instant,
    },
};

// the max number of instructions of given type that we can put into packet.
const MAX_INSTRUCTIONS_PER_TRANSACTION: usize = 204;

fn create_accounts(num_accounts: usize, genesis_config: &mut GenesisConfig) -> Vec<Keypair> {
    let owner = &system_program::id();

    let account_keypairs: Vec<Keypair> = (0..num_accounts).map(|_| Keypair::new()).collect();
    for keypair in account_keypairs.iter() {
        genesis_config.add_account(keypair.pubkey(), AccountSharedData::new(10000, 0, &owner));
    }
    account_keypairs
}

/// Structure that returns correct provided blockhash or some incorrect hash
/// with given probability.
pub struct FaultyBlockhash {
    blockhash: Hash,
    probability_invalid_blockhash: f64,
}

impl FaultyBlockhash {
    /// Create a new faulty hash generator
    pub fn new(blockhash: Hash, probability_invalid_blockhash: f64) -> Self {
        Self {
            blockhash,
            probability_invalid_blockhash,
        }
    }

    pub fn get<R: Rng>(&self, rng: &mut R) -> Hash {
        if rng.gen::<f64>() < self.probability_invalid_blockhash {
            Hash::default()
        } else {
            self.blockhash
        }
    }
}

fn generate_transactions(
    num_txs: usize,
    bank: Arc<Bank>,
    fee_payers: &[Keypair],
    num_instructions_per_tx: usize,
    probability_invalid_blockhash: f64,
) -> BankingPacketBatch {
    assert!(num_instructions_per_tx <= MAX_INSTRUCTIONS_PER_TRANSACTION);
    let blockhash = FaultyBlockhash::new(bank.last_blockhash(), probability_invalid_blockhash);

    let mut rng = rand::thread_rng();

    let mut fee_payers = fee_payers.iter().cycle();

    let txs: Vec<VersionedTransaction> = (0..num_txs)
        .map(|_| {
            let fee_payer = fee_payers.next().unwrap();
            let program_id = Pubkey::new_unique();

            let mut instructions = Vec::with_capacity(num_instructions_per_tx);
            // Experiments with different distributions didn't show much of the effect on the performance.
            let compute_unit_price = rng.gen_range(0..1000);
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                compute_unit_price,
            ));
            for _ in 0..num_instructions_per_tx - 1 {
                instructions.push(Instruction::new_with_bytes(
                    program_id,
                    &[0],
                    vec![AccountMeta {
                        pubkey: fee_payer.pubkey(),
                        is_signer: true,
                        is_writable: true,
                    }],
                ));
            }
            VersionedTransaction::try_new(
                VersionedMessage::Legacy(Message::new_with_blockhash(
                    &instructions,
                    Some(&fee_payer.pubkey()),
                    &blockhash.get(&mut rng),
                )),
                &[&fee_payer],
            )
            .unwrap()
        })
        .collect();

    let packets_batches = BankingPacketBatch::new(to_packet_batches(&txs, NUM_PACKETS));
    return packets_batches;
}

// TODO(klykov): maybe not needed anylonger if add new to the TransactionViewReceiveAndBuffer?
trait ReceiveAndBufferCreator {
    fn create(
        receiver: Receiver<Arc<Vec<PacketBatch>>>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self;
}

impl ReceiveAndBufferCreator for TransactionViewReceiveAndBuffer {
    fn create(
        receiver: Receiver<Arc<Vec<PacketBatch>>>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        TransactionViewReceiveAndBuffer {
            receiver,
            bank_forks,
        }
    }
}

impl ReceiveAndBufferCreator for SanitizedTransactionReceiveAndBuffer {
    fn create(
        receiver: Receiver<Arc<Vec<PacketBatch>>>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        SanitizedTransactionReceiveAndBuffer::new(PacketDeserializer::new(receiver), bank_forks)
    }
}

fn bench_receive_and_buffer<T: ReceiveAndBuffer + ReceiveAndBufferCreator>(
    c: &mut Criterion,
    bench_name: &str,
    num_instructions_per_tx: usize,
    probability_invalid_blockhash: f64,
) {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(100_000);
    let num_txs = 16 * 1024;
    // This doesn't change the time to execute
    let num_fee_payers = 1;
    // fee payers will be verified, so have to create them properly
    let fee_payers = create_accounts(num_fee_payers, &mut genesis_config);

    let (bank, bank_forks) =
        Bank::new_for_benches(&genesis_config).wrap_with_bank_forks_for_tests();
    let bank_start = BankStart {
        working_bank: bank.clone(),
        bank_creation_time: Arc::new(Instant::now()),
    };

    let (sender, receiver) = unbounded();

    let mut rb = T::create(receiver, bank_forks);

    const TOTAL_BUFFERED_PACKETS: usize = 100_000;
    let mut count_metrics = SchedulerCountMetrics::default();
    let mut timing_metrics = SchedulerTimingMetrics::default();
    let decision = BufferedPacketsDecision::Consume(bank_start);

    let txs = generate_transactions(
        num_txs,
        bank.clone(),
        &fee_payers,
        num_instructions_per_tx,
        probability_invalid_blockhash,
    );
    let mut container = <T as ReceiveAndBuffer>::Container::with_capacity(TOTAL_BUFFERED_PACKETS);
    c.bench_function(bench_name, |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                // Setup
                {
                    if sender.send(txs.clone()).is_err() {
                        panic!("Unexpectedly dropped receiver!");
                    }

                    // make sure container is empty.
                    container.clear();
                }

                let start = Instant::now();
                {
                    let res = rb.receive_and_buffer_packets(
                        &mut container,
                        &mut timing_metrics,
                        &mut count_metrics,
                        &decision,
                    );
                    assert!(res.unwrap() == num_txs && !container.is_empty());
                    black_box(&container);
                }
                total += start.elapsed();
            }
            total
        })
    });
}

fn bench_sanitized_transaction_receive_and_buffer(c: &mut Criterion) {
    bench_receive_and_buffer::<SanitizedTransactionReceiveAndBuffer>(
        c,
        "sanitized_transaction_receive_and_buffer_max_instructions",
        MAX_INSTRUCTIONS_PER_TRANSACTION,
        0.0,
    );
    bench_receive_and_buffer::<SanitizedTransactionReceiveAndBuffer>(
        c,
        "sanitized_transaction_receive_and_buffer_max_instructions_10p_invalid_blockhash",
        MAX_INSTRUCTIONS_PER_TRANSACTION,
        0.1,
    );
    bench_receive_and_buffer::<SanitizedTransactionReceiveAndBuffer>(
        c,
        "sanitized_transaction_receive_and_buffer_min_instructions",
        0,
        0.0,
    );
}

fn bench_transaction_view_receive_and_buffer(c: &mut Criterion) {
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_receive_and_buffer_max_instructions",
        MAX_INSTRUCTIONS_PER_TRANSACTION,
        0.0,
    );
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_receive_and_buffer_max_instructions_10p_invalid_blockhash",
        MAX_INSTRUCTIONS_PER_TRANSACTION,
        0.1,
    );
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_receive_and_buffer_min_instructions",
        0,
        0.0,
    );
}

criterion_group!(
    benches,
    bench_sanitized_transaction_receive_and_buffer,
    bench_transaction_view_receive_and_buffer
);
criterion_main!(benches);
