use {
    crate::{
        send_transaction_service_stats::{
            SendTransactionServiceStats, SendTransactionServiceStatsReport,
        },
        transaction_client::TransactionClient,
    },
    crossbeam_channel::{Receiver, RecvTimeoutError},
    itertools::Itertools,
    log::*,
    solana_hash::Hash,
    solana_nonce_account as nonce_account,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, BankPair},
    },
    solana_signature::Signature,
    std::{
        collections::hash_map::{Entry, HashMap},
        net::SocketAddr,
        num::Saturating,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Maximum size of the transaction retry pool
const MAX_TRANSACTION_RETRY_POOL_SIZE: usize = 10_000; // This seems like a lot but maybe it needs to be bigger one day

/// Default retry interval
const DEFAULT_RETRY_RATE_MS: u64 = 2_000;

/// Default number of leaders to forward transactions to
const DEFAULT_LEADER_FORWARD_COUNT: u64 = 2;
/// Default max number of time the service will retry broadcast
const DEFAULT_SERVICE_MAX_RETRIES: usize = usize::MAX;

/// Default batch size for sending transaction in batch
/// When this size is reached, send out the transactions.
const DEFAULT_TRANSACTION_BATCH_SIZE: usize = 1;

// The maximum transaction batch size
pub const MAX_TRANSACTION_BATCH_SIZE: usize = 10_000;

/// Maximum transaction sends per second
pub const MAX_TRANSACTION_SENDS_PER_SECOND: u64 = 1_000;

/// Default maximum batch waiting time in ms. If this time is reached,
/// whatever transactions are cached will be sent.
const DEFAULT_BATCH_SEND_RATE_MS: u64 = 1;

// The maximum transaction batch send rate in MS
pub const MAX_BATCH_SEND_RATE_MS: usize = 100_000;

pub struct SendTransactionService {
    receive_txn_thread: JoinHandle<()>,
    retry_thread: JoinHandle<()>,
    exit: Arc<AtomicBool>,
}

pub struct TransactionInfo {
    pub message_hash: Hash,
    pub signature: Signature,
    pub blockhash: Hash,
    pub wire_transaction: Vec<u8>,
    pub last_valid_block_height: u64,
    pub durable_nonce_info: Option<(Pubkey, Hash)>,
    pub max_retries: Option<usize>,
    retries: usize,
    /// Last time the transaction was sent
    last_sent_time: Option<Instant>,
}

impl TransactionInfo {
    pub fn new(
        message_hash: Hash,
        signature: Signature,
        blockhash: Hash,
        wire_transaction: Vec<u8>,
        last_valid_block_height: u64,
        durable_nonce_info: Option<(Pubkey, Hash)>,
        max_retries: Option<usize>,
        last_sent_time: Option<Instant>,
    ) -> Self {
        Self {
            message_hash,
            signature,
            blockhash,
            wire_transaction,
            last_valid_block_height,
            durable_nonce_info,
            max_retries,
            retries: 0,
            last_sent_time,
        }
    }

    fn get_max_retries(
        &self,
        default_max_retries: Option<usize>,
        service_max_retries: usize,
    ) -> Option<usize> {
        self.max_retries
            .or(default_max_retries)
            .map(|max_retries| max_retries.min(service_max_retries))
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct ProcessTransactionsResult {
    rooted: u64,
    expired: u64,
    retried: u64,
    max_retries_elapsed: u64,
    failed: u64,
    retained: u64,
    last_sent_time: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub retry_rate_ms: u64,
    pub leader_forward_count: u64,
    pub default_max_retries: Option<usize>,
    pub service_max_retries: usize,
    /// The batch size for sending transactions in batches
    pub batch_size: usize,
    /// How frequently batches are sent
    pub batch_send_rate_ms: u64,
    /// When the retry pool exceeds this max size, new transactions are dropped after their first broadcast attempt
    pub retry_pool_max_size: usize,
    pub tpu_peers: Option<Vec<SocketAddr>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retry_rate_ms: DEFAULT_RETRY_RATE_MS,
            leader_forward_count: DEFAULT_LEADER_FORWARD_COUNT,
            default_max_retries: None,
            service_max_retries: DEFAULT_SERVICE_MAX_RETRIES,
            batch_size: DEFAULT_TRANSACTION_BATCH_SIZE,
            batch_send_rate_ms: DEFAULT_BATCH_SEND_RATE_MS,
            retry_pool_max_size: MAX_TRANSACTION_RETRY_POOL_SIZE,
            tpu_peers: None,
        }
    }
}

/// The maximum duration the retry thread may be configured to sleep before
/// processing the transactions that need to be retried.
pub const MAX_RETRY_SLEEP_MS: u64 = 1000;

impl SendTransactionService {
    pub fn new_with_client<Client: TransactionClient + Clone + std::marker::Send + 'static>(
        bank_forks: &Arc<RwLock<BankForks>>,
        receiver: Receiver<TransactionInfo>,
        client: Client,
        config: Config,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let stats_report = Arc::new(SendTransactionServiceStatsReport::default());

        let retry_transactions = Arc::new(Mutex::new(HashMap::new()));

        let receive_txn_thread = Self::receive_txn_thread(
            receiver,
            client.clone(),
            retry_transactions.clone(),
            config.clone(),
            stats_report.clone(),
            exit.clone(),
        );

        let retry_thread = Self::retry_thread(
            bank_forks.clone(),
            client,
            retry_transactions,
            config,
            stats_report,
            exit.clone(),
        );
        Self {
            receive_txn_thread,
            retry_thread,
            exit,
        }
    }

    /// Thread responsible for receiving transactions from RPC clients.
    fn receive_txn_thread<Client: TransactionClient + std::marker::Send + 'static>(
        receiver: Receiver<TransactionInfo>,
        client: Client,
        retry_transactions: Arc<Mutex<HashMap<Signature, TransactionInfo>>>,
        Config {
            batch_send_rate_ms,
            batch_size,
            retry_pool_max_size,
            default_max_retries,
            service_max_retries,
            ..
        }: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let mut last_batch_sent = Instant::now();
        let mut transactions = HashMap::new();

        debug!("Starting send-transaction-service::receive_txn_thread");
        Builder::new()
            .name("solStxReceive".to_string())
            .spawn(move || loop {
                let stats = &stats_report.stats;
                let recv_result = receiver.recv_timeout(Duration::from_millis(batch_send_rate_ms));
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                match recv_result {
                    Err(RecvTimeoutError::Disconnected) => {
                        info!("Terminating send-transaction-service.");
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(transaction_info) => {
                        stats.received_transactions.fetch_add(1, Ordering::Relaxed);
                        let entry = transactions.entry(transaction_info.signature);
                        let mut new_transaction = false;
                        if let Entry::Vacant(_) = entry {
                            if !retry_transactions
                                .lock()
                                .unwrap()
                                .contains_key(&transaction_info.signature)
                            {
                                entry.or_insert(transaction_info);
                                new_transaction = true;
                            }
                        }
                        if !new_transaction {
                            stats
                                .received_duplicate_transactions
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                if (!transactions.is_empty()
                    && last_batch_sent.elapsed().as_millis() as u64 >= batch_send_rate_ms)
                    || transactions.len() >= batch_size
                {
                    stats
                        .sent_transactions
                        .fetch_add(transactions.len() as u64, Ordering::Relaxed);
                    let wire_transactions = transactions
                        .values()
                        .map(|transaction_info| transaction_info.wire_transaction.clone())
                        .collect::<Vec<Vec<u8>>>();
                    client.send_transactions_in_batch(wire_transactions, stats);
                    let last_sent_time = Instant::now();
                    {
                        // take a lock of retry_transactions and move the batch to the retry set.
                        let mut retry_transactions = retry_transactions.lock().unwrap();
                        let mut transactions_to_retry: usize = 0;
                        let mut transactions_added_to_retry = Saturating::<usize>(0);
                        for (signature, mut transaction_info) in transactions.drain() {
                            // drop transactions with 0 max retries
                            let max_retries = transaction_info
                                .get_max_retries(default_max_retries, service_max_retries);
                            if max_retries == Some(0) {
                                continue;
                            }
                            transactions_to_retry += 1;

                            let retry_len = retry_transactions.len();
                            let entry = retry_transactions.entry(signature);
                            if let Entry::Vacant(_) = entry {
                                if retry_len >= retry_pool_max_size {
                                    break;
                                } else {
                                    transaction_info.last_sent_time = Some(last_sent_time);
                                    transactions_added_to_retry += 1;
                                    entry.or_insert(transaction_info);
                                }
                            }
                        }
                        let Saturating(retry_queue_overflow) =
                            Saturating(transactions_to_retry) - transactions_added_to_retry;
                        stats
                            .retry_queue_overflow
                            .fetch_add(retry_queue_overflow as u64, Ordering::Relaxed);
                        stats
                            .retry_queue_size
                            .store(retry_transactions.len() as u64, Ordering::Relaxed);
                    }
                    last_batch_sent = Instant::now();
                }
                stats_report.report();
            })
            .unwrap()
    }

    /// Thread responsible for retrying transactions
    fn retry_thread<Client: TransactionClient + std::marker::Send + 'static>(
        bank_forks: Arc<RwLock<BankForks>>,
        client: Client,
        retry_transactions: Arc<Mutex<HashMap<Signature, TransactionInfo>>>,
        config: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        debug!("Starting send-transaction-service::retry_thread.");
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let retry_interval_ms_default = MAX_RETRY_SLEEP_MS.min(config.retry_rate_ms);
        let mut retry_interval_ms = retry_interval_ms_default;
        Builder::new()
            .name("solStxRetry".to_string())
            .spawn(move || loop {
                sleep(Duration::from_millis(retry_interval_ms));
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                let mut transactions = retry_transactions.lock().unwrap();
                if transactions.is_empty() {
                    retry_interval_ms = retry_interval_ms_default;
                } else {
                    let stats = &stats_report.stats;
                    stats
                        .retry_queue_size
                        .store(transactions.len() as u64, Ordering::Relaxed);

                    let BankPair {
                        root_bank,
                        working_bank,
                    } = sharable_banks.load();
                    let result = Self::process_transactions(
                        &working_bank,
                        &root_bank,
                        &mut transactions,
                        &client,
                        &config,
                        stats,
                    );
                    stats_report.report();

                    // Adjust retry interval taking into account the time since the last send.
                    retry_interval_ms = retry_interval_ms_default
                        .checked_sub(
                            result
                                .last_sent_time
                                .and_then(|last| Instant::now().checked_duration_since(last))
                                .and_then(|interval| interval.as_millis().try_into().ok())
                                .unwrap_or(0),
                        )
                        .unwrap_or(retry_interval_ms_default);
                }
            })
            .unwrap()
    }

    /// Retry transactions sent before.
    fn process_transactions<Client: TransactionClient + std::marker::Send + 'static>(
        working_bank: &Bank,
        root_bank: &Bank,
        transactions: &mut HashMap<Signature, TransactionInfo>,
        client: &Client,
        &Config {
            retry_rate_ms,
            service_max_retries,
            default_max_retries,
            batch_size,
            ..
        }: &Config,
        stats: &SendTransactionServiceStats,
    ) -> ProcessTransactionsResult {
        let mut result = ProcessTransactionsResult::default();

        let mut batched_transactions = Vec::new();
        let mut exceeded_retries_transactions = Vec::new();
        let retry_rate = Duration::from_millis(retry_rate_ms);

        transactions.retain(|signature, transaction_info| {
            if transaction_info.durable_nonce_info.is_some() {
                stats.nonced_transactions.fetch_add(1, Ordering::Relaxed);
            }
            if root_bank
                .get_committed_transaction_status_and_slot(
                    &transaction_info.message_hash,
                    &transaction_info.blockhash,
                )
                .is_some()
            {
                info!("Transaction is rooted: {signature}");
                result.rooted += 1;
                stats.rooted_transactions.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let signature_status = working_bank.get_committed_transaction_status_and_slot(
                &transaction_info.message_hash,
                &transaction_info.blockhash,
            );
            if let Some((nonce_pubkey, durable_nonce)) = transaction_info.durable_nonce_info {
                let nonce_account = working_bank.get_account(&nonce_pubkey).unwrap_or_default();
                let now = Instant::now();
                let expired = transaction_info
                    .last_sent_time
                    .and_then(|last| now.checked_duration_since(last))
                    .map(|elapsed| elapsed >= retry_rate)
                    .unwrap_or(false);
                let verify_nonce_account =
                    nonce_account::verify_nonce_account(&nonce_account, &durable_nonce);
                if verify_nonce_account.is_none() && signature_status.is_none() && expired {
                    info!("Dropping expired durable-nonce transaction: {signature}");
                    result.expired += 1;
                    stats.expired_transactions.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
            if transaction_info.last_valid_block_height < root_bank.block_height() {
                info!("Dropping expired transaction: {signature}");
                result.expired += 1;
                stats.expired_transactions.fetch_add(1, Ordering::Relaxed);
                return false;
            }

            let max_retries =
                transaction_info.get_max_retries(default_max_retries, service_max_retries);

            if let Some(max_retries) = max_retries {
                if transaction_info.retries >= max_retries {
                    info!("Dropping transaction due to max retries: {signature}");
                    result.max_retries_elapsed += 1;
                    stats
                        .transactions_exceeding_max_retries
                        .fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }

            match signature_status {
                None => {
                    let now = Instant::now();
                    let need_send = transaction_info
                        .last_sent_time
                        .and_then(|last| now.checked_duration_since(last))
                        .map(|elapsed| elapsed >= retry_rate)
                        .unwrap_or(true);
                    if need_send {
                        if transaction_info.last_sent_time.is_some() {
                            // Transaction sent before is unknown to the working bank, it might have been
                            // dropped or landed in another fork. Re-send it.

                            info!("Retrying transaction: {signature}");
                            result.retried += 1;
                            transaction_info.retries += 1;
                        }

                        batched_transactions.push(*signature);
                        transaction_info.last_sent_time = Some(now);

                        let max_retries = transaction_info
                            .get_max_retries(default_max_retries, service_max_retries);
                        if let Some(max_retries) = max_retries {
                            if transaction_info.retries >= max_retries {
                                exceeded_retries_transactions.push(*signature);
                            }
                        }
                    } else if let Some(last) = transaction_info.last_sent_time {
                        result.last_sent_time = Some(
                            result
                                .last_sent_time
                                .map(|result_last| result_last.min(last))
                                .unwrap_or(last),
                        );
                    }
                    true
                }
                Some((_slot, status)) => {
                    if !status {
                        info!("Dropping failed transaction: {signature}");
                        result.failed += 1;
                        stats.failed_transactions.fetch_add(1, Ordering::Relaxed);
                        false
                    } else {
                        result.retained += 1;
                        true
                    }
                }
            }
        });

        stats.retries.fetch_add(result.retried, Ordering::Relaxed);

        if !batched_transactions.is_empty() {
            // Processing the transactions in batch
            let wire_transactions = batched_transactions
                .iter()
                .filter_map(|signature| transactions.get(signature))
                .map(|transaction_info| transaction_info.wire_transaction.clone());

            let iter = wire_transactions.chunks(batch_size);
            for chunk in &iter {
                let chunk = chunk.collect();
                client.send_transactions_in_batch(chunk, stats);
            }
        }

        result.max_retries_elapsed += exceeded_retries_transactions.len() as u64;
        stats
            .transactions_exceeding_max_retries
            .fetch_add(result.max_retries_elapsed, Ordering::Relaxed);
        for signature in exceeded_retries_transactions {
            info!("Dropping transaction due to max retries: {signature}");
            transactions.remove(&signature);
        }

        result
    }

    pub fn join(self) -> thread::Result<()> {
        self.receive_txn_thread.join()?;
        self.exit.store(true, Ordering::Relaxed);
        self.retry_thread.join()
    }
}
#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            test_utils::ClientWithCreator,
            tpu_info::NullTpuInfo,
            transaction_client::{ConnectionCacheClient, TpuClientNextClient},
        },
        crossbeam_channel::{bounded, unbounded},
        solana_account::AccountSharedData,
        solana_genesis_config::create_genesis_config,
        solana_nonce::{self as nonce, state::DurableNonce},
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_system_interface::program as system_program,
        solana_system_transaction as system_transaction,
        std::ops::Sub,
        tokio::runtime::Handle,
    };

    fn service_exit<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = unbounded();

        let client = C::create_client(maybe_runtime, "127.0.0.1:0".parse().unwrap(), None, 1);

        let send_transaction_service = SendTransactionService::new_with_client(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 1000,
                ..Config::default()
            },
            Arc::new(AtomicBool::new(false)),
        );

        drop(sender);
        send_transaction_service.join().unwrap();
        client.stop();
    }

    #[test]
    fn service_exit_with_connection_cache() {
        service_exit::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn service_exit_with_tpu_client_next() {
        service_exit::<TpuClientNextClient>(Some(Handle::current()));
    }

    fn validator_exit<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = bounded(0);

        let dummy_tx_info = || TransactionInfo {
            message_hash: Hash::default(),
            signature: Signature::default(),
            blockhash: Hash::default(),
            wire_transaction: vec![0; 128],
            last_valid_block_height: 0,
            durable_nonce_info: None,
            max_retries: None,
            retries: 0,
            last_sent_time: None,
        };

        let exit = Arc::new(AtomicBool::new(false));
        let client = C::create_client(maybe_runtime, "127.0.0.1:0".parse().unwrap(), None, 1);
        let _send_transaction_service = SendTransactionService::new_with_client(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 1000,
                ..Config::default()
            },
            exit.clone(),
        );

        sender.send(dummy_tx_info()).unwrap();

        thread::spawn(move || {
            exit.store(true, Ordering::Relaxed);
            client.stop();
        });

        let mut option = Ok(());
        while option.is_ok() {
            option = sender.send(dummy_tx_info());
        }
    }

    #[test]
    fn validator_exit_with_connection_cache() {
        validator_exit::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validator_exit_with_tpu_client_next() {
        validator_exit::<TpuClientNextClient>(Some(Handle::current()));
    }

    fn process_transactions<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        agave_logger::setup();

        let (mut genesis_config, mint_keypair) = create_genesis_config(4);
        genesis_config.fee_rate_governor = solana_fee_calculator::FeeRateGovernor::new(0, 0);
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let leader_forward_count = 1;
        let config = Config::default();

        let root_bank = Bank::new_from_parent(
            bank_forks.read().unwrap().working_bank(),
            &Pubkey::default(),
            1,
        );
        let root_bank = bank_forks
            .write()
            .unwrap()
            .insert(root_bank)
            .clone_without_scheduler();

        let (rooted_transaction, rooted_signature) = {
            let transaction = system_transaction::transfer(
                &mint_keypair,
                &mint_keypair.pubkey(),
                1,
                root_bank.last_blockhash(),
            );
            root_bank.process_transaction(&transaction).unwrap();
            let signature = transaction.signatures[0];
            (transaction, signature)
        };

        let working_bank = bank_forks
            .write()
            .unwrap()
            .insert(Bank::new_from_parent(
                root_bank.clone(),
                &Pubkey::default(),
                2,
            ))
            .clone_without_scheduler();

        let (non_rooted_transaction, non_rooted_signature) = {
            let transaction = system_transaction::transfer(
                &mint_keypair,
                &mint_keypair.pubkey(),
                2,
                working_bank.last_blockhash(),
            );
            working_bank.process_transaction(&transaction).unwrap();
            let signature = transaction.signatures[0];
            (transaction, signature)
        };

        let (failed_transaction, failed_signature) = {
            let transaction = system_transaction::transfer(
                &mint_keypair,
                &Pubkey::default(),
                1,
                working_bank.last_blockhash(),
            );
            working_bank.process_transaction(&transaction).unwrap_err();
            let signature = transaction.signatures[0];
            (transaction, signature)
        };

        let mut transactions = HashMap::new();

        info!("Expired transactions are dropped...");
        let stats = SendTransactionServiceStats::default();
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                root_bank.block_height() - 1,
                None,
                None,
                Some(Instant::now()),
            ),
        );

        let client = C::create_client(
            maybe_runtime,
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Rooted transactions are dropped...");
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_transaction.message.hash(),
                rooted_signature,
                rooted_transaction.message.recent_blockhash,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Failed transactions are dropped...");
        transactions.insert(
            failed_signature,
            TransactionInfo::new(
                failed_transaction.message.hash(),
                failed_signature,
                failed_transaction.message.recent_blockhash,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                failed: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Non-rooted transactions are kept...");
        transactions.insert(
            non_rooted_signature,
            TransactionInfo::new(
                non_rooted_transaction.message.hash(),
                non_rooted_signature,
                non_rooted_transaction.message.recent_blockhash,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retained: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Unknown transactions are retried...");
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );

        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Transactions are only retried until max_retries");
        transactions.insert(
            Signature::from([1; 64]),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                working_bank.block_height(),
                None,
                Some(0),
                Some(Instant::now()),
            ),
        );
        transactions.insert(
            Signature::from([2; 64]),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                working_bank.block_height(),
                None,
                Some(1),
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                max_retries_elapsed: 2,
                ..ProcessTransactionsResult::default()
            }
        );
        client.stop();
    }

    #[test]
    fn process_transactions_with_connection_cache() {
        process_transactions::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn process_transactions_with_tpu_client_next() {
        process_transactions::<TpuClientNextClient>(Some(Handle::current()));
    }

    fn retry_durable_nonce_transactions<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        agave_logger::setup();

        let (mut genesis_config, mint_keypair) = create_genesis_config(4);
        genesis_config.fee_rate_governor = solana_fee_calculator::FeeRateGovernor::new(0, 0);
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let leader_forward_count = 1;
        let config = Config::default();

        let root_bank = Bank::new_from_parent(
            bank_forks.read().unwrap().working_bank(),
            &Pubkey::default(),
            1,
        );
        let root_bank = bank_forks
            .write()
            .unwrap()
            .insert(root_bank)
            .clone_without_scheduler();

        let (rooted_transaction, rooted_signature) = {
            let transaction = system_transaction::transfer(
                &mint_keypair,
                &mint_keypair.pubkey(),
                1,
                root_bank.last_blockhash(),
            );
            root_bank.process_transaction(&transaction).unwrap();
            let signature = transaction.signatures[0];
            (transaction, signature)
        };

        let nonce_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::new(Pubkey::default(), durable_nonce, 42),
        ));
        let nonce_account =
            AccountSharedData::new_data(43, &nonce_state, &system_program::id()).unwrap();
        root_bank.store_account(&nonce_address, &nonce_account);

        let working_bank = bank_forks
            .write()
            .unwrap()
            .insert(Bank::new_from_parent(
                root_bank.clone(),
                &Pubkey::default(),
                2,
            ))
            .clone_without_scheduler();

        let (non_rooted_transaction, non_rooted_signature) = {
            let transaction = system_transaction::transfer(
                &mint_keypair,
                &mint_keypair.pubkey(),
                2,
                working_bank.last_blockhash(),
            );
            let signature = transaction.signatures[0];
            working_bank.process_transaction(&transaction).unwrap();
            (transaction, signature)
        };

        let last_valid_block_height = working_bank.block_height() + 300;

        let (failed_transaction, failed_signature) = {
            let blockhash = working_bank.last_blockhash();
            let transaction =
                system_transaction::transfer(&mint_keypair, &Pubkey::default(), 1, blockhash);
            let signature = transaction.signatures[0];
            working_bank.process_transaction(&transaction).unwrap_err();
            (transaction, signature)
        };

        let mut transactions = HashMap::new();

        info!("Rooted durable-nonce transactions are dropped...");
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_transaction.message.hash(),
                rooted_signature,
                rooted_transaction.message.recent_blockhash,
                vec![],
                last_valid_block_height,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now()),
            ),
        );
        let stats = SendTransactionServiceStats::default();
        let client = C::create_client(
            maybe_runtime,
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // Nonce expired case
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_transaction.message.hash(),
                rooted_signature,
                rooted_transaction.message.recent_blockhash,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        // Expired durable-nonce transactions are dropped; nonce has advanced...
        info!("Expired durable-nonce transactions are dropped...");
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // ... or last_valid_block_height timeout has passed
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                root_bank.block_height() - 1,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Failed durable-nonce transactions are dropped...");
        transactions.insert(
            failed_signature,
            TransactionInfo::new(
                failed_transaction.message.hash(),
                failed_signature,
                failed_transaction.message.recent_blockhash,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())), // runtime should advance nonce on failed transactions
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                failed: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Non-rooted durable-nonce transactions are kept...");
        transactions.insert(
            non_rooted_signature,
            TransactionInfo::new(
                non_rooted_transaction.message.hash(),
                non_rooted_signature,
                non_rooted_transaction.message.recent_blockhash,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())), // runtime advances nonce when transaction lands
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retained: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Unknown durable-nonce transactions are retried until nonce advances...");
        // simulate there was a nonce transaction sent 4 seconds ago (> the retry rate which is 2 seconds)
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Hash::default(),
                Signature::default(),
                Hash::default(),
                vec![],
                last_valid_block_height,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // Advance nonce, simulate the transaction was again last sent 4 seconds ago.
        // This time the transaction should have been dropped.
        for transaction in transactions.values_mut() {
            transaction.last_sent_time = Some(Instant::now().sub(Duration::from_millis(4000)));
        }
        let new_durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let new_nonce_state = nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::new(Pubkey::default(), new_durable_nonce, 42),
        ));
        let nonce_account =
            AccountSharedData::new_data(43, &new_nonce_state, &system_program::id()).unwrap();
        working_bank.store_account(&nonce_address, &nonce_account);
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 0);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        client.stop();
    }

    #[test]
    fn retry_durable_nonce_transactions_with_connection_cache() {
        retry_durable_nonce_transactions::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_durable_nonce_transactions_with_tpu_client_next() {
        retry_durable_nonce_transactions::<TpuClientNextClient>(Some(Handle::current()));
    }
}
