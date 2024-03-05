use {
    crate::bench_tps_client::{BenchTpsClient, Result},
    chrono::{DateTime, Utc},
    crossbeam_channel::{select, tick, unbounded, Receiver, Sender},
    csv,
    log::*,
    serde::Serialize,
    solana_client::rpc_config::RpcBlockConfig,
    solana_measure::measure::Measure,
    solana_sdk::{
        clock::{DEFAULT_MS_PER_SLOT, DEFAULT_S_PER_SLOT, MAX_PROCESSING_AGE},
        commitment_config::{CommitmentConfig, CommitmentLevel},
        signature::Signature,
        slot_history::Slot,
    },
    solana_transaction_status::{
        option_serializer::OptionSerializer, EncodedTransactionWithStatusMeta, RewardType,
        TransactionDetails, UiConfirmedBlock, UiTransactionEncoding, UiTransactionStatusMeta,
    },
    std::{
        collections::HashMap,
        fs::File,
        sync::Arc,
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

const PROCESS_BLOCKS_EVERY_MS: u64 = 16 * DEFAULT_MS_PER_SLOT;
const REMOVE_TIMEOUT_TX_EVERY_SEC: i64 = (MAX_PROCESSING_AGE as f64 * DEFAULT_S_PER_SLOT) as i64;

#[derive(Clone)]
pub(crate) struct SignatureBatch {
    pub signatures: Vec<Signature>,
    pub sent_at: DateTime<Utc>,
    pub compute_unit_prices: Vec<Option<u64>>, // TODO(klykov) pub sent_slot: Slot, I think it can be calculated from time
}

#[derive(Clone)]
pub(crate) struct TransactionSendInfo {
    pub sent_at: DateTime<Utc>,
    pub compute_unit_price: Option<u64>,
}

#[derive(Clone, Serialize)]
struct BlockData {
    pub block_hash: String,
    pub block_slot: Slot,
    pub block_leader: String,
    pub block_time: u64,
    pub total_num_transactions: usize,
    pub num_bench_tps_transactions: usize,
    pub total_cu_consumed: u64,
    pub bench_tps_cu_consumed: u64,
}

#[derive(Clone, Serialize)]
struct TransactionData {
    pub signature: String,
    //pub sent_slot: Slot,
    pub sent_at: String,
    pub confirmed_slot: Option<Slot>,
    //pub confirmed_at: Option<String>,
    pub successful: bool,
    pub slot_leader: Option<String>,
    pub error: Option<String>,
    pub blockhash: Option<String>,
    pub timed_out: bool,
    pub compute_unit_price: u64,
}

fn check_confirmations(
    block_data_file: Option<&String>,
    transaction_data_file: Option<&String>,
) -> bool {
    block_data_file.is_some() || transaction_data_file.is_some()
}

pub(crate) type SignatureBatchSender = Sender<SignatureBatch>;
type SignatureBatchReceiver = Receiver<SignatureBatch>;

pub(crate) fn create_log_transactions_service_and_sender<T>(
    client: &Arc<T>,
    block_data_file: Option<&String>,
    transaction_data_file: Option<&String>,
) -> (Option<LogTransactionService>, Option<SignatureBatchSender>)
where
    T: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    if check_confirmations(block_data_file, transaction_data_file) {
        let (sender, receiver) = unbounded();
        let log_tx_service =
            LogTransactionService::new(client, receiver, block_data_file, transaction_data_file);
        (Some(log_tx_service), Some(sender))
    } else {
        (None, None)
    }
}

pub(crate) struct LogTransactionService {
    thread_handler: JoinHandle<()>,
}

impl LogTransactionService {
    pub fn join(self) -> thread::Result<()> {
        self.thread_handler.join()
    }

    fn new<T>(
        client: &Arc<T>,
        signature_receiver: SignatureBatchReceiver,
        block_data_file: Option<&String>,
        transaction_data_file: Option<&String>,
    ) -> Self
    where
        T: 'static + BenchTpsClient + Send + Sync + ?Sized,
    {
        if !check_confirmations(block_data_file, transaction_data_file) {
            panic!("Expect block-data-file or transaction-data-file is specified.");
        }

        let client = client.clone();
        let mut log_writer = LogWriter::new(block_data_file, transaction_data_file);

        let thread_handler = Builder::new()
            .name("LogTransactionService".to_string())
            .spawn(move || {
                Self::run(client, signature_receiver, log_writer);
            })
            .expect("LogTransactionService is up.");
        return Self { thread_handler };
    }

    fn run<T>(client: Arc<T>, signature_receiver: SignatureBatchReceiver, mut log_writer: LogWriter)
    where
        T: 'static + BenchTpsClient + Send + Sync + ?Sized,
    {
        let commitment: CommitmentConfig = CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        };
        let rpc_block_config = RpcBlockConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            transaction_details: Some(TransactionDetails::Full),
            rewards: Some(true),
            commitment: Some(commitment),
            max_supported_transaction_version: Some(0),
        };
        let block_processing_timer_receiver = tick(Duration::from_millis(PROCESS_BLOCKS_EVERY_MS));

        let mut start_block = get_slot_with_retry(&client).expect("get_slot_with_retry succeed");

        let mut signature_to_tx_info = HashMap::<Signature, TransactionSendInfo>::new();
        loop {
            select! {
                recv(signature_receiver) -> msg => {
                    match msg {
                        Ok(SignatureBatch {
                            signatures,
                            sent_at,
                            compute_unit_prices
                        }) => {
                            let mut measure_send_txs = Measure::start("measure_update_map");
                            signatures.iter().zip(compute_unit_prices).for_each( |(sign, compute_unit_price)| {signature_to_tx_info.insert(*sign, TransactionSendInfo {
                                sent_at,
                                compute_unit_price
                            });});

                            measure_send_txs.stop();
                            let time_send_ns = measure_send_txs.as_ns();
                            info!("@@@ Time to add signatures to map: {time_send_ns}")
                        }
                        Err(e) => {
                            info!("Stop LogTransactionService, error message received {e}");
                            log_writer.flush();
                            break;
                        }
                    }
                },
                recv(block_processing_timer_receiver) -> _ => {
                            let mut measure_send_txs = Measure::start("measure_update_map");
                    info!("sign_receiver queue len: {}", signature_receiver.len());
                    let block_slots = get_blocks_with_retry(&client, start_block);
                    let Ok(block_slots) = block_slots else {
                        error!("Failed to get blocks");
                        drop(signature_receiver);
                        break;
                    };
                    if block_slots.is_empty() {
                        continue;
                    }
                    start_block = *block_slots.last().unwrap() + 1;
                    let blocks = block_slots.iter().map(|slot| {
                    client.get_block_with_config(
                        *slot,
                        rpc_block_config
                    )
                    });
                    for (block, slot) in blocks.zip(&block_slots) {
                        let Ok(block) = block else {
                            continue;
                        };
                        Self::process_block(
                            block,
                            &mut signature_to_tx_info,
                            *slot,
                            &mut log_writer,
                        )
                    }
                    Self::clean_transaction_map(&mut log_writer, &mut signature_to_tx_info);

                    // maybe ok to write every time here? Or create a separate timer
                    log_writer.flush();
                    measure_send_txs.stop();
                    let time_send_ns = measure_send_txs.as_ns();
                    info!("@@@ Time to process blocks: {time_send_ns}")

                },
            }
        }
    }

    fn process_block(
        block: UiConfirmedBlock,
        signature_to_tx_info: &mut HashMap<Signature, TransactionSendInfo>,
        slot: u64,
        log_writer: &mut LogWriter,
    ) {
        let rewards = block.rewards.as_ref().expect("Rewards are present.");
        let slot_leader = rewards
            .iter()
            .find(|r| r.reward_type == Some(RewardType::Fee))
            .map_or("".to_string(), |x| x.pubkey.clone());

        let Some(transactions) = &block.transactions else {
            warn!("Empty block: {slot}");
            return;
        };

        let mut num_bench_tps_transactions: usize = 0;
        let mut total_cu_consumed: u64 = 0;
        let mut bench_tps_cu_consumed: u64 = 0;
        for EncodedTransactionWithStatusMeta {
            transaction, meta, ..
        } in transactions
        {
            let Some(transaction) = transaction.decode() else {
                continue;
            };
            let cu_consumed = meta
                .as_ref()
                .map_or(0, |meta| match meta.compute_units_consumed {
                    OptionSerializer::Some(cu_consumed) => cu_consumed, //TODO(klykov): consider adding error info as well
                    _ => 0,
                });
            let signature = &transaction.signatures[0];

            total_cu_consumed = total_cu_consumed.saturating_add(cu_consumed);
            if let Some(TransactionSendInfo {
                sent_at,
                compute_unit_price,
            }) = signature_to_tx_info.remove(signature)
            {
                num_bench_tps_transactions = num_bench_tps_transactions.saturating_add(1);
                bench_tps_cu_consumed = bench_tps_cu_consumed.saturating_add(cu_consumed);

                log_writer.write_to_transaction_log(
                    signature,
                    Some(slot),
                    sent_at,
                    meta.as_ref(),
                    Some(block.blockhash.clone()),
                    Some(slot_leader.clone()),
                    compute_unit_price,
                    true,
                );
            }
        }
        log_writer.write_to_block_log(
            block.blockhash.clone(),
            slot_leader,
            slot,
            block.block_time,
            num_bench_tps_transactions,
            transactions.len(),
            bench_tps_cu_consumed,
            total_cu_consumed,
        )
    }

    fn clean_transaction_map(
        log_writer: &mut LogWriter,
        signature_to_tx_info: &mut HashMap<Signature, TransactionSendInfo>,
    ) {
        let now: DateTime<Utc> = Utc::now();
        signature_to_tx_info.retain(|signature, tx_info| {
            let duration_since_past_time = now.signed_duration_since(tx_info.sent_at);
            let is_not_timeout_tx =
                duration_since_past_time.num_seconds() < REMOVE_TIMEOUT_TX_EVERY_SEC;
            if !is_not_timeout_tx {
                log_writer.write_to_transaction_log(
                    signature,
                    None,
                    tx_info.sent_at,
                    None,
                    None,
                    None,
                    tx_info.compute_unit_price,
                    true,
                );
            }
            is_not_timeout_tx
        });
    }
}

type CsvFileWriter = csv::Writer<File>;
struct LogWriter {
    block_log_writer: Option<CsvFileWriter>,
    transaction_log_writer: Option<CsvFileWriter>,
}

impl LogWriter {
    fn new(block_data_file: Option<&String>, transaction_data_file: Option<&String>) -> Self {
        let block_log_writer = block_data_file.map(|block_data_file| {
            CsvFileWriter::from_writer(File::create(block_data_file).expect("File can be created."))
        });
        let transaction_log_writer = transaction_data_file.map(|transaction_data_file| {
            CsvFileWriter::from_writer(
                File::create(transaction_data_file).expect("File can be created."),
            )
        });
        Self {
            block_log_writer,
            transaction_log_writer,
        }
    }

    fn write_to_transaction_log(
        &mut self,
        signature: &Signature,
        confirmed_slot: Option<Slot>,
        sent_at: DateTime<Utc>,
        meta: Option<&UiTransactionStatusMeta>,
        blockhash: Option<String>,
        slot_leader: Option<String>,
        compute_unit_price: Option<u64>,
        timed_out: bool,
    ) {
        if let Some(transaction_log_writer) = &mut self.transaction_log_writer {
            let tx_data = TransactionData {
                signature: signature.to_string(),
                confirmed_slot,
                //confirmed_at: Some(Utc::now().to_string()),
                // TODO use sent_slot instead of sent_at by using map
                sent_at: sent_at.to_string(),
                //sent_slot: transaction_record.sent_slot,
                successful: meta.as_ref().map_or(false, |m| m.status.is_ok()),
                error: meta
                    .as_ref()
                    .and_then(|m| m.err.as_ref().map(|x| x.to_string())),
                blockhash,
                slot_leader: slot_leader,
                timed_out,
                compute_unit_price: compute_unit_price.unwrap_or(0),
            };
            transaction_log_writer.serialize(tx_data);
        }
    }

    fn write_to_block_log(
        &mut self,
        blockhash: String,
        slot_leader: String,
        slot: Slot,
        block_time: Option<i64>,
        num_bench_tps_transactions: usize,
        total_num_transactions: usize,
        bench_tps_cu_consumed: u64,
        total_cu_consumed: u64,
    ) {
        if let Some(block_log_writer) = &mut self.block_log_writer {
            let block_data = BlockData {
                block_hash: blockhash,
                block_leader: slot_leader,
                block_slot: slot,
                block_time: block_time.map_or(0, |time| time as u64),
                num_bench_tps_transactions,
                total_num_transactions,
                bench_tps_cu_consumed,
                total_cu_consumed,
            };
            block_log_writer.serialize(block_data);
        }
    }

    fn flush(&mut self) {
        if let Some(block_log_writer) = &mut self.block_log_writer {
            let _ = block_log_writer.flush();
        }
        if let Some(transaction_log_writer) = &mut self.transaction_log_writer {
            let _ = transaction_log_writer.flush();
        }
    }
}

const NUM_RETRY: u64 = 5;

fn get_slot_with_retry<T>(client: &Arc<T>) -> Result<Slot>
where
    T: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    for _ in 1..NUM_RETRY {
        let current_slot = client.get_slot();

        match current_slot {
            Ok(slot) => {
                return Ok(slot);
            }
            Err(error) => {
                warn!("Failed to get slot: {error}, retry.");
            }
        }
    }
    client.get_slot()
}

fn get_blocks_with_retry<T>(client: &Arc<T>, start_block: u64) -> Result<Vec<Slot>>
where
    T: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    for _ in 1..NUM_RETRY {
        let block_slots = client.get_blocks(start_block, None);

        match block_slots {
            Ok(slots) => {
                return Ok(slots);
            }
            Err(error) => {
                warn!("Failed to download blocks: {error}, retry.");
            }
        }
    }
    client.get_blocks(start_block, None)
}
