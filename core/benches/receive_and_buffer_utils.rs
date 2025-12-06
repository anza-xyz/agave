use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{unbounded, Receiver, Sender},
    rand::prelude::*,
    solana_account::AccountSharedData,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_core::banking_stage::{
        decision_maker::BufferedPacketsDecision,
        transaction_scheduler::{
            receive_and_buffer::{ReceiveAndBuffer, TransactionViewReceiveAndBuffer},
            transaction_state_container::StateContainer,
        },
        TOTAL_BUFFERED_PACKETS,
    },
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
    solana_message::{Message, VersionedMessage},
    solana_perf::packet::{to_packet_batches, PacketBatch, NUM_PACKETS},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk_ids::system_program,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    std::sync::{Arc, RwLock},
};

// the max number of instructions of given type that we can put into packet.
pub const MAX_INSTRUCTIONS_PER_TRANSACTION: usize = 204;

fn create_accounts(num_accounts: usize, genesis_config: &mut GenesisConfig) -> Vec<Keypair> {
    let owner = &system_program::id();

    let account_keypairs: Vec<Keypair> = (0..num_accounts).map(|_| Keypair::new()).collect();
    for keypair in account_keypairs.iter() {
        genesis_config.add_account(keypair.pubkey(), AccountSharedData::new(10000, 0, owner));
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
    set_rand_cu_price: bool,
) -> BankingPacketBatch {
    assert!(num_instructions_per_tx <= MAX_INSTRUCTIONS_PER_TRANSACTION);
    if set_rand_cu_price {
        assert!(
            num_instructions_per_tx > 0,
            "`num_instructions_per_tx` must be at least 1 when `set_rand_cu_price` flag is set to \
             count the set_compute_unit_price instruction."
        );
    }
    let blockhash = FaultyBlockhash::new(bank.last_blockhash(), probability_invalid_blockhash);

    let mut rng = rand::thread_rng();

    let mut fee_payers = fee_payers.iter().cycle();

    let txs: Vec<VersionedTransaction> = (0..num_txs)
        .map(|_| {
            let fee_payer = fee_payers.next().unwrap();
            let program_id = Pubkey::new_unique();

            let mut instructions = Vec::with_capacity(num_instructions_per_tx);
            if set_rand_cu_price {
                // Experiments with different distributions didn't show much of the effect on the performance.
                let compute_unit_price = rng.gen_range(0..1000);
                instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                    compute_unit_price,
                ));
            }
            for _ in 0..num_instructions_per_tx.saturating_sub(1) {
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

    BankingPacketBatch::new(to_packet_batches(&txs, NUM_PACKETS))
}

pub trait ReceiveAndBufferCreator {
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
            sharable_banks: bank_forks.read().unwrap().sharable_banks(),
        }
    }
}

pub struct ReceiveAndBufferSetup<T: ReceiveAndBuffer> {
    // prepared transaction batches
    pub txs: BankingPacketBatch,
    // to send prepared transaction batches
    pub sender: Sender<Arc<Vec<PacketBatch>>>,
    // received transactions will be inserted into container
    pub container: <T as ReceiveAndBuffer>::Container,
    // receive_and_buffer for sdk or transaction_view
    pub receive_and_buffer: T,
    // hardcoded for bench to always Consume
    pub decision: BufferedPacketsDecision,
}

pub fn setup_receive_and_buffer<T: ReceiveAndBuffer + ReceiveAndBufferCreator>(
    num_txs: usize,
    num_instructions_per_tx: usize,
    probability_invalid_blockhash: f64,
    set_rand_cu_price: bool,
    use_single_payer: bool,
) -> ReceiveAndBufferSetup<T> {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(100_000);
    let num_fee_payers = if use_single_payer { 1 } else { num_txs };
    // fee payers will be verified, so have to create them properly
    let fee_payers = create_accounts(num_fee_payers, &mut genesis_config);

    let (bank, bank_forks) =
        Bank::new_for_benches(&genesis_config).wrap_with_bank_forks_for_tests();

    let (sender, receiver) = unbounded();

    let receive_and_buffer = T::create(receiver, bank_forks);

    let decision = BufferedPacketsDecision::Consume(bank.clone());

    let txs = generate_transactions(
        num_txs,
        bank.clone(),
        &fee_payers,
        num_instructions_per_tx,
        probability_invalid_blockhash,
        set_rand_cu_price,
    );
    let container = <T as ReceiveAndBuffer>::Container::with_capacity(TOTAL_BUFFERED_PACKETS);

    ReceiveAndBufferSetup {
        txs,
        sender,
        container,
        receive_and_buffer,
        decision,
    }
}
