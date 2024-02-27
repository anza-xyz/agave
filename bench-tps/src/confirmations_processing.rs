use {
    crate::bench_tps_client::BenchTpsClient,
    chrono::{DateTime, Utc},
    crossbeam_channel::{select, tick, Receiver, Sender},
    log::*,
    serde::Serialize,
    solana_client::rpc_config::RpcBlockConfig,
    solana_measure::measure::Measure,
    solana_rpc_client::rpc_client::RpcClient,
    solana_sdk::{
        commitment_config::CommitmentConfig, commitment_config::CommitmentLevel,
        signature::Signature, slot_history::Slot,
    },
    solana_transaction_status::{
        option_serializer::OptionSerializer, RewardType, TransactionDetails, UiConfirmedBlock,
        UiTransactionEncoding,
    },
    std::{
        collections::HashMap,
        sync::Arc,
        thread::{Builder, JoinHandle},
        time::Duration,
    },
};

const BLOCK_PROCESSING_PERIOD_MS: u64 = 400;

//TODO(klykov) extract some method retry
fn get_blocks_with_retry<T>(client: &Arc<T>, start_block: u64) -> Result<Vec<Slot>, ()>
where
    T: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    const N_TRY_REQUEST_BLOCKS: u64 = 4;
    for _ in 0..N_TRY_REQUEST_BLOCKS {
        let block_slots = client.get_blocks(start_block, None);

        match block_slots {
            Ok(slots) => {
                return Ok(slots);
            }
            Err(error) => {
                warn!("Failed to download blocks: {}, retry", error);
            }
        }
    }
    Err(())
}

#[derive(Clone)]
pub struct SignatureBatch {
    pub signatures: Vec<Signature>,
    pub sent_at: DateTime<Utc>,
    // pub sent_slot: Slot, I think it can be calculated from time
}

//TODO(klykov) If there will be no other data, rename to transaction time or something lile that
#[derive(Clone)]
pub struct TransactionSendInfo {
    pub sent_at: DateTime<Utc>,
    //pub sent_slot: Slot,
    //TODO add priority fee
    //pub priority_fee: u64,
}

#[derive(Clone, Serialize)]
pub struct BlockData {
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
pub struct TransactionData {
    pub signature: String,
    //pub sent_slot: Slot,
    pub sent_at: String,
    pub confirmed_slot: Option<Slot>,
    //pub confirmed_at: Option<String>,
    pub successful: bool,
    pub slot_leader: Option<String>,
    pub error: Option<String>,
    pub block_hash: Option<String>,
    pub slot_processed: Option<Slot>,
    pub timed_out: bool,
    //TODO add priority fee
    //pub priority_fee: u64,
}

pub type SignatureBatchReceiver = Receiver<SignatureBatch>;
pub type SignatureBatchSender = Sender<SignatureBatch>;

// TODO(klykov): extract to TxConfirmationService
fn create_confirmation_handler_thread<T>(
    client: &Arc<T>,
    sign_receiver: SignatureBatchReceiver,
) -> JoinHandle<()>
where
    T: 'static + BenchTpsClient + Send + Sync + ?Sized,
{
    let block_processing_timer_receiver = tick(Duration::from_millis(BLOCK_PROCESSING_PERIOD_MS));

    // TODO(klykov): wrap with retry
    let from_slot = client.get_slot().expect("get_slot succeed");

    let mut start_block = from_slot;
    //TODO(klykov) use commitment setup globally
    let commitment = CommitmentConfig {
        commitment: CommitmentLevel::Confirmed,
    };
    let rpc_block_config = RpcBlockConfig {
        encoding: Some(UiTransactionEncoding::Base64),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(true),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(0),
    };
    let client = client.clone();

    Builder::new().name("ConfirmationHandler".to_string()).spawn(move || {
        let mut signature_to_tx_info = HashMap::<Signature, TransactionSendInfo>::new();
        loop {
            select! {
                recv(sign_receiver) -> msg => {
                    match msg {
                        Ok(SignatureBatch {
                            signatures,
                            sent_at,
                            //sent_slot
                        }) => {
                            let mut measure_send_txs = Measure::start("measure_send_txs");

                            signatures.iter().for_each( |sign| {signature_to_tx_info.insert(*sign, TransactionSendInfo {
                                sent_at, //sent_slot
                            });});

                            measure_send_txs.stop();
                            let time_send_ns = measure_send_txs.as_ns();
                            info!("TIME: {time_send_ns}")
                        }
                        _ => panic!("Sender panics"),
                    }
                },
                recv(block_processing_timer_receiver) -> _ => {
                    info!("sign_receiver queue len: {}", sign_receiver.len());
                    // TODO(klykov) Move to process_blocks();
                    let block_slots = get_blocks_with_retry(&client, start_block);
                    let Ok(block_slots) = block_slots else {
                        error!("Failed to get blocks");
                        //TODO(klykov) shall I drop receiver?
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
                    for block_slot in blocks.zip(&block_slots) {
                        let block = match block_slot.0 {
                            Ok(x) => x,
                            Err(_) => continue,
                        };
                        process_blocks(
                            block,
                            &mut signature_to_tx_info,
                            *block_slot.1,
                        )
                    }

                },
            }
        }
    }).unwrap()
}

fn process_blocks(
    block: UiConfirmedBlock,
    signature_to_tx_info: &mut HashMap<Signature, TransactionSendInfo>,
    slot: u64,
) {
    let rewards = block.rewards.as_ref().unwrap();
    let slot_leader = match rewards
        .iter()
        .find(|r| r.reward_type == Some(RewardType::Fee))
    {
        Some(x) => x.pubkey.clone(),
        None => "".to_string(),
    };

    let Some(transactions) = &block.transactions else {
        warn!("Empty block: {slot}");
        return;
    };

    let mut num_bench_tps_transactions: usize = 0;
    let mut total_cu_consumed: u64 = 0;
    let mut bench_tps_cu_consumed: u64 = 0;
    for solana_transaction_status::EncodedTransactionWithStatusMeta {
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
        // TODO(klykov): rename variable
        if let Some(transaction_record) = signature_to_tx_info.remove(signature) {
            num_bench_tps_transactions = num_bench_tps_transactions.saturating_add(1);
            bench_tps_cu_consumed = bench_tps_cu_consumed.saturating_add(cu_consumed);

            let tx_confirm = TransactionData {
                signature: signature.to_string(),
                confirmed_slot: Some(slot),
                //confirmed_at: Some(Utc::now().to_string()),
                // TODO use sent_slot instead of sent_at by using map
                sent_at: transaction_record.sent_at.to_string(),
                //sent_slot: transaction_record.sent_slot,
                successful: if let Some(meta) = &meta {
                    meta.status.is_ok()
                } else {
                    false
                },
                error: if let Some(meta) = &meta {
                    meta.err.as_ref().map(|x| x.to_string())
                } else {
                    None
                },
                block_hash: Some(block.blockhash.clone()),
                slot_processed: Some(slot),
                slot_leader: Some(slot_leader.clone()),
                timed_out: false,
                //priority_fees: transaction_record.priority_fees,
            };
            let ss = serde_json::to_value(&tx_confirm).unwrap();
            info!("TransactionData: {}", ss.to_string());
        }
    }
    // push block data
    {
        let blockData = BlockData {
            block_hash: block.blockhash.clone(),
            block_leader: slot_leader,
            block_slot: slot,
            block_time: if let Some(time) = block.block_time {
                time as u64
            } else {
                0
            },
            num_bench_tps_transactions,
            total_num_transactions: transactions.len(),
            bench_tps_cu_consumed,
            total_cu_consumed,
        };
        let ss = serde_json::to_value(&blockData).unwrap();
        info!("BlockData: {}", ss.to_string());
        //writer.serialize(record).await.unwrap();
    }
}

/* writing to csv
let mut writer = csv_async::AsyncSerializer::from_writer(
    File::create(block_data_save_file).await.unwrap(),
);
let mut block_data = block_data;
while let Ok(record) = block_data.recv().await {
    writer.serialize(record).await.unwrap();
}
writer.flush().await.unwrap();
*/
