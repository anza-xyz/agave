use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    crossbeam_channel::{unbounded, Receiver},
    rand::prelude::*,
    rand_distr::{Distribution, Normal, Uniform, WeightedIndex},
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

fn create_accounts(num_accounts: usize, genesis_config: &mut GenesisConfig) -> Vec<Keypair> {
    let owner = &system_program::id();

    let account_keypairs: Vec<Keypair> = (0..num_accounts).map(|_| Keypair::new()).collect();
    for keypair in account_keypairs.iter() {
        genesis_config.add_account(keypair.pubkey(), AccountSharedData::new(10000, 0, &owner));
    }
    account_keypairs
}
pub trait DistributionSource {
    fn sample(&self, rng: &mut ThreadRng) -> u64;
}

pub struct UniformDist {
    dist: Uniform<u64>,
}

impl UniformDist {
    pub fn new(min: u64, max: u64) -> Self {
        Self {
            dist: Uniform::new(min, max),
        }
    }
}

impl DistributionSource for UniformDist {
    fn sample(&self, rng: &mut ThreadRng) -> u64 {
        self.dist.sample(rng)
    }
}

pub struct NormalDist {
    dist: Normal<f64>,
}

impl NormalDist {
    pub fn new(mean: f64, std_dev: f64) -> Self {
        Self {
            dist: Normal::new(mean, std_dev).unwrap(),
        }
    }
}

impl DistributionSource for NormalDist {
    fn sample(&self, rng: &mut ThreadRng) -> u64 {
        self.dist.sample(rng) as u64
    }
}

pub struct ConstantValue {
    value: u64,
}

impl ConstantValue {
    pub fn new(value: u64) -> Self {
        Self { value }
    }
}

impl DistributionSource for ConstantValue {
    fn sample(&self, _rng: &mut ThreadRng) -> u64 {
        self.value
    }
}

/// Implementation of a discrete probability distribution
pub struct DiscreteDistribution {
    values: Vec<u64>,
    weights: WeightedIndex<f64>,
}

impl DiscreteDistribution {
    pub fn new(values: Vec<u64>, probabilities: Vec<f64>) -> Self {
        assert_eq!(
            values.len(),
            probabilities.len(),
            "Values and probabilities must have the same length"
        );
        assert!(
            probabilities.iter().all(|&p| p >= 0.0),
            "Probabilities must be non-negative"
        );

        let weights =
            WeightedIndex::new(&probabilities).expect("Invalid probabilities (must sum to > 0)");

        Self { values, weights }
    }
}

impl DistributionSource for DiscreteDistribution {
    fn sample(&self, rng: &mut ThreadRng) -> u64 {
        let index = self.weights.sample(rng);
        self.values[index]
    }
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
    compute_unit_price: Box<dyn DistributionSource>,
    probability_invalid_blockhash: f64,
) -> BankingPacketBatch {
    let mut rng: ThreadRng = thread_rng();
    let blockhash = FaultyBlockhash::new(bank.last_blockhash(), probability_invalid_blockhash);

    const MAX_INSTRUCTIONS_PER_TRANSACTION: usize = 205;
    let mut fee_payers = fee_payers.iter().cycle();

    let txs: Vec<VersionedTransaction> = (0..num_txs)
        .map(|_| {
            let fee_payer = fee_payers.next().unwrap();
            let program_id = Pubkey::new_unique();

            let mut instructions = Vec::with_capacity(MAX_INSTRUCTIONS_PER_TRANSACTION);
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                compute_unit_price.sample(&mut rng),
            ));
            for _ in 0..MAX_INSTRUCTIONS_PER_TRANSACTION - 1 {
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
) {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(100_000);
    let num_txs = 1024;
    let num_fee_payers = 128;
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
        Box::new(UniformDist::new(1, 100)),
        0.0,
    );
    c.bench_function(bench_name, |bencher| {
        bencher.iter_with_setup(
            || {
                if sender.send(txs.clone()).is_err() {
                    panic!("Unexpectedly dropped receiver!");
                }

                let container =
                    <T as ReceiveAndBuffer>::Container::with_capacity(TOTAL_BUFFERED_PACKETS);
                container
            },
            |mut container| {
                let res = rb.receive_and_buffer_packets(
                    &mut container,
                    &mut timing_metrics,
                    &mut count_metrics,
                    &decision,
                );
                assert!(res.unwrap() == num_txs && !container.is_empty());
                black_box(container);
            },
        )
    });
}

fn bench_sanitized_transaction_receive_and_buffer(c: &mut Criterion) {
    bench_receive_and_buffer::<SanitizedTransactionReceiveAndBuffer>(
        c,
        "sanitized_transaction_receive_and_buffer",
    );
}

fn bench_transaction_view_receive_and_buffer(c: &mut Criterion) {
    bench_receive_and_buffer::<TransactionViewReceiveAndBuffer>(
        c,
        "transaction_view_receive_and_buffer",
    );
}

criterion_group!(
    benches,
    bench_sanitized_transaction_receive_and_buffer,
    bench_transaction_view_receive_and_buffer
);
criterion_main!(benches);
