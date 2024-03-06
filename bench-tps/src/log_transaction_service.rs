//! `LogTransactionService` requests confirmed blocks, analyses transactions submitted by bench-tps,
//! and saves log files in csv format.

use {
    crate::bench_tps_client::{BenchTpsClient, Result},
    chrono::{DateTime, TimeZone, Utc},
    crossbeam_channel::{select, tick, unbounded, Receiver, Sender},
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
        thread::{self, sleep, Builder, JoinHandle},
        time::Duration,
    },
};

// Data to establish communication between sender thread and
// LogTransactionService.
#[derive(Clone)]
pub(crate) struct TransactionInfoBatch {
    pub signatures: Vec<Signature>,
    pub sent_at: DateTime<Utc>,
    pub compute_unit_prices: Vec<Option<u64>>,
}

pub(crate) type SignatureBatchSender = Sender<TransactionInfoBatch>;

pub(crate) struct LogTransactionService {
    thread_handler: JoinHandle<()>,
}

pub(crate) fn create_log_transactions_service_and_sender<Client>(
    client: &Arc<Client>,
    block_data_file: Option<&str>,
    transaction_data_file: Option<&str>,
) -> (Option<LogTransactionService>, Option<SignatureBatchSender>)
where
    Client: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    if verify_data_files(block_data_file, transaction_data_file) {
        let (sender, receiver) = unbounded();
        let log_tx_service =
            LogTransactionService::new(client, receiver, block_data_file, transaction_data_file);
        (Some(log_tx_service), Some(sender))
    } else {
        (None, None)
    }
}

// How often process blocks.
const PROCESS_BLOCKS_EVERY_MS: u64 = 16 * DEFAULT_MS_PER_SLOT;
// Max age for transaction in the transaction map.
const REMOVE_TIMEOUT_TX_EVERY_SEC: i64 = (MAX_PROCESSING_AGE as f64 * DEFAULT_S_PER_SLOT) as i64;

// Map used to filter submitted transactions.
#[derive(Clone)]
struct TransactionSendInfo {
    pub sent_at: DateTime<Utc>,
    pub compute_unit_price: Option<u64>,
}
type MapSignatureToTxInfo = HashMap<Signature, TransactionSendInfo>;

type SignatureBatchReceiver = Receiver<TransactionInfoBatch>;

impl LogTransactionService {
    fn new<Client>(
        client: &Arc<Client>,
        signature_receiver: SignatureBatchReceiver,
        block_data_file: Option<&str>,
        transaction_data_file: Option<&str>,
    ) -> Self
    where
        Client: 'static + BenchTpsClient + Send + Sync + ?Sized,
    {
        if !verify_data_files(block_data_file, transaction_data_file) {
            panic!("Expect block-data-file or transaction-data-file is specified, must have been verified by callee.");
        }

        let client = client.clone();
        let log_writer = LogWriter::new(block_data_file, transaction_data_file);

        let thread_handler = Builder::new()
            .name("LogTransactionService".to_string())
            .spawn(move || {
                Self::run(client, signature_receiver, log_writer);
            })
            .expect("LogTransactionService is up.");
        Self { thread_handler }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_handler.join()
    }

    fn run<Client>(
        client: Arc<Client>,
        signature_receiver: SignatureBatchReceiver,
        mut log_writer: LogWriter,
    ) where
        Client: 'static + BenchTpsClient + Send + Sync + ?Sized,
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

        let mut start_block = get_slot_with_retry(&client)
            .expect("get_slot_with_retry succeed, cannot proceed without having slot. Must be a problem with RPC.");

        let mut signature_to_tx_info = MapSignatureToTxInfo::new();
        loop {
            select! {
                recv(signature_receiver) -> msg => {
                    match msg {
                        Ok(TransactionInfoBatch {
                            signatures,
                            sent_at,
                            compute_unit_prices
                        }) => {
                            signatures.iter().zip(compute_unit_prices).for_each( |(sign, compute_unit_price)| {signature_to_tx_info.insert(*sign, TransactionSendInfo {
                                sent_at,
                                compute_unit_price
                            });});
                        }
                        Err(e) => {
                            info!("Stop LogTransactionService, message received: {e}");
                            log_writer.flush();
                            break;
                        }
                    }
                },
                recv(block_processing_timer_receiver) -> _ => {
                    let mut measure_process_blocks = Measure::start("measure_process_blocks");
                    info!("sign_receiver queue len: {}", signature_receiver.len());
                    let block_slots = get_blocks_with_retry(&client, start_block);
                    let Ok(block_slots) = block_slots else {
                        error!("Failed to get blocks, stop LogWriterService.");
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
                    let num_blocks = blocks.len();
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
                    measure_process_blocks.stop();

                    let time_send_us = measure_process_blocks.as_us();
                    info!("Time to process {num_blocks} blocks: {time_send_us}");
                    log_writer.flush();
                },
            }
        }
    }

    fn process_block(
        block: UiConfirmedBlock,
        signature_to_tx_info: &mut MapSignatureToTxInfo,
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
                    OptionSerializer::Some(cu_consumed) => cu_consumed,
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
                    block.block_time,
                    sent_at,
                    meta.as_ref(),
                    Some(block.blockhash.clone()),
                    Some(slot_leader.clone()),
                    compute_unit_price,
                    false,
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
        signature_to_tx_info: &mut MapSignatureToTxInfo,
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

fn verify_data_files(block_data_file: Option<&str>, transaction_data_file: Option<&str>) -> bool {
    block_data_file.is_some() || transaction_data_file.is_some()
}

#[derive(Clone, Serialize)]
struct BlockData {
    pub block_hash: String,
    pub block_slot: Slot,
    pub block_leader: String,
    pub block_time: Option<DateTime<Utc>>,
    pub total_num_transactions: usize,
    pub num_bench_tps_transactions: usize,
    pub total_cu_consumed: u64,
    pub bench_tps_cu_consumed: u64,
}

#[derive(Clone, Serialize)]
struct TransactionData {
    pub signature: String,
    pub sent_at: Option<DateTime<Utc>>,
    pub confirmed_slot: Option<Slot>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub successful: bool,
    pub slot_leader: Option<String>,
    pub error: Option<String>,
    pub blockhash: Option<String>,
    pub timed_out: bool,
    pub compute_unit_price: u64,
}

type CsvFileWriter = csv::Writer<File>;
struct LogWriter {
    block_log_writer: Option<CsvFileWriter>,
    transaction_log_writer: Option<CsvFileWriter>,
}

impl LogWriter {
    fn new(block_data_file: Option<&str>, transaction_data_file: Option<&str>) -> Self {
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

    #[allow(clippy::too_many_arguments)]
    fn write_to_transaction_log(
        &mut self,
        signature: &Signature,
        confirmed_slot: Option<Slot>,
        block_time: Option<i64>,
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
                confirmed_at: block_time.map(|time| {
                    Utc.timestamp_opt(time, 0)
                        .latest()
                        .expect("valid timestamp")
                }),
                sent_at: Some(sent_at),
                successful: meta.as_ref().map_or(false, |m| m.status.is_ok()),
                error: meta
                    .as_ref()
                    .and_then(|m| m.err.as_ref().map(|x| x.to_string())),
                blockhash,
                slot_leader,
                timed_out,
                compute_unit_price: compute_unit_price.unwrap_or(0),
            };
            let _ = transaction_log_writer.serialize(tx_data);
        }
    }

    #[allow(clippy::too_many_arguments)]
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
                block_time: block_time.map(|time| {
                    Utc.timestamp_opt(time, 0)
                        .latest()
                        .expect("valid timestamp")
                }),
                num_bench_tps_transactions,
                total_num_transactions,
                bench_tps_cu_consumed,
                total_cu_consumed,
            };
            let _ = block_log_writer.serialize(block_data);
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
const RETRY_EVERY_MS: u64 = 4 * DEFAULT_MS_PER_SLOT;

fn call_rpc_with_retry<Func, Data>(f: Func, retry_warning: &str) -> Result<Data>
where
    Func: Fn() -> Result<Data>,
{
    let mut iretry = 0;
    loop {
        match f() {
            Ok(slot) => {
                return Ok(slot);
            }
            Err(error) => {
                if iretry == NUM_RETRY {
                    return Err(error);
                }
                warn!("{retry_warning}: {error}, retry.");
                sleep(Duration::from_millis(RETRY_EVERY_MS));
            }
        }
        iretry += 1;
    }
}

fn get_slot_with_retry<Client>(client: &Arc<Client>) -> Result<Slot>
where
    Client: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    call_rpc_with_retry(|| client.get_slot(), "Failed to get slot")
}

fn get_blocks_with_retry<Client>(client: &Arc<Client>, start_block: u64) -> Result<Vec<Slot>>
where
    Client: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    call_rpc_with_retry(
        || client.get_blocks(start_block, None),
        "Failed to download blocks",
    )
}
