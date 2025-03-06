use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::Sender,
    rand::prelude::*,
    rand_distr::{Distribution, Normal, Uniform, WeightedIndex},
    solana_keypair::Keypair,
    solana_perf::packet::{to_packet_batches, PacketBatch, NUM_PACKETS},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::instruction::AccountMeta,
    solana_sdk::{
        account::AccountSharedData,
        compute_budget::ComputeBudgetInstruction,
        genesis_config::GenesisConfig,
        hash::Hash,
        instruction::Instruction,
        message::{Message, VersionedMessage},
        signer::Signer,
        system_instruction,
        transaction::{Transaction, VersionedTransaction},
    },
    std::sync::Arc,
};

pub fn create_accounts(num_txs: usize, genesis_config: &mut GenesisConfig) -> Vec<Keypair> {
    let owner = Keypair::new();

    let account_keypairs: Vec<Keypair> = (0..num_txs).map(|_| Keypair::new()).collect();
    for keypair in account_keypairs.iter() {
        genesis_config.add_account(
            keypair.pubkey(),
            AccountSharedData::new(1000, 0, &owner.pubkey()),
        );
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

pub struct TransactionConfig {
    pub compute_unit_price: Box<dyn DistributionSource>,
    pub transaction_cu_budget: u64,
    /// This parameter specifies the size of consequent dependent
    /// transaction clique. Assume that we generate a list of txs tx_1,
    /// tx_2,.... For value 1 we have all txs to be independent, for value 2
    /// tx_1 and tx_2 will share the same source account, but tx_3 will be
    /// independent from them. For value 3, tx_1, tx_2, tx_3 will share the
    /// same source, but tx4 will be independent from these 3, etc.
    pub num_account_conflicts: usize,
    pub probability_invalid_blockhash: f64,
    pub probability_invalid_account: f64,
    pub data_size_limit: u32,
}

// TODO If this functionality generally sounds good, we can redo it with Iterator later.
pub fn generate_transactions(
    num_txs: usize,
    account_keypairs: &[Keypair],
    bank: Arc<Bank>,
    sender: Sender<Arc<Vec<PacketBatch>>>,
    TransactionConfig {
        compute_unit_price,
        transaction_cu_budget,
        num_account_conflicts,
        probability_invalid_blockhash,
        probability_invalid_account,
        data_size_limit,
    }: TransactionConfig,
) {
    // We need to split accounts into two parts: from and to. To avoid
    // undesired conflicts, the from part will be from 0 to
    // `1/(num_account_conflicts+1)*num_txs`. And the
    // requirement that `num_account_conflicts*num_txs <=
    // (num_account_conflicts+1)*num_accounts`.
    let num_accounts = account_keypairs.len();
    assert!(num_account_conflicts * num_txs <= (num_account_conflicts + 1) * num_accounts);

    let from_keypairs = account_keypairs
        .iter()
        .flat_map(|x| std::iter::repeat_with(move || x).take(num_account_conflicts));

    let split_point = num_accounts / (num_account_conflicts + 1);
    let to_keypairs = account_keypairs
        .iter()
        .skip(split_point as usize)
        .map(|keypair| keypair.pubkey());

    let mut rng: ThreadRng = thread_rng();
    let blockhash = FaultyBlockhash::new(bank.last_blockhash(), probability_invalid_blockhash);

    let mut txs: Vec<Transaction> = Vec::with_capacity(num_txs);
    for (from, to) in from_keypairs.zip(to_keypairs) {
        let from = if rng.gen_bool(probability_invalid_account) {
            &Keypair::new()
        } else {
            from
        };
        let transfer = system_instruction::transfer(&from.pubkey(), &to, 1);
        let set_cu_instruction =
            ComputeBudgetInstruction::set_compute_unit_limit(transaction_cu_budget as u32);
        let set_data_size_limit =
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_size_limit);
        let set_fee_instruction =
            ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price.sample(&mut rng));
        let message = Message::new(
            &[
                set_cu_instruction,
                set_data_size_limit,
                set_fee_instruction,
                transfer,
            ],
            Some(&from.pubkey()),
        );
        txs.push(Transaction::new(
            &vec![&from],
            message,
            blockhash.get(&mut rng),
        ));
    }

    let packets_batches = BankingPacketBatch::new(to_packet_batches(&txs, NUM_PACKETS));
    // Send 2 times to touch more code in receive_until packet_deserializer.rs
    for _ in 0..2 {
        if sender.send(packets_batches.clone()).is_err() {
            panic!("Unexpectedly dropped receiver!");
        }
    }
}

pub fn generate_transactions_simple(
    num_txs: usize,
    bank: Arc<Bank>,
    sender: Sender<Arc<Vec<PacketBatch>>>,
    TransactionConfig {
        compute_unit_price,
        probability_invalid_blockhash,
        ..
    }: TransactionConfig,
) {
    let mut rng: ThreadRng = thread_rng();
    let blockhash = FaultyBlockhash::new(bank.last_blockhash(), probability_invalid_blockhash);

    const MAX_INSTRUCTIONS_PER_TRANSACTION: usize = 205;

    let txs: Vec<VersionedTransaction> = (0..num_txs)
        .map(|_| {
            let keypair = Keypair::new();
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
                        pubkey: keypair.pubkey(),
                        is_signer: true,
                        is_writable: true,
                    }],
                ));
            }
            VersionedTransaction::try_new(
                VersionedMessage::Legacy(Message::new_with_blockhash(
                    &instructions,
                    Some(&keypair.pubkey()),
                    &blockhash.get(&mut rng),
                )),
                &[&keypair],
            )
            .unwrap()
        })
        .collect();

    //let x = to_packet_batches(&txs, NUM_PACKETS);

    let packets_batches = BankingPacketBatch::new(to_packet_batches(&txs, NUM_PACKETS));
    //TODO Does it matter?
    // Send 2 times to touch more code in receive_until packet_deserializer.rs
    //for _ in 0..2 {
    if sender.send(packets_batches.clone()).is_err() {
        panic!("Unexpectedly dropped receiver!");
    }
    //}
}
