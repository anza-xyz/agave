use {
    crate::{
        send_transaction_service_stats::{
            SendTransactionServiceStats, SendTransactionServiceStatsReport,
        },
        transaction_client::TransactionClient,
    },
    crossbeam_channel::Receiver as CrossbeamReceiver,
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
        collections::HashMap,
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
    tokio::{sync::mpsc, task::JoinHandle, time::sleep},
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

/// The maximum duration the retry task may be configured to sleep before
/// processing the transactions that need to be retried.
pub const MAX_RETRY_SLEEP_MS: u64 = 1000;

pub struct SendTransactionServiceNext {
    receive_txn_task: JoinHandle<()>,
    retry_task: JoinHandle<()>,
    exit: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
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

impl SendTransactionServiceNext {
    pub fn new<Client: TransactionClient + Clone + Send + 'static>(
        bank_forks: &Arc<RwLock<BankForks>>,
        receiver: mpsc::Receiver<TransactionInfo>,
        client: Client,
        config: Config,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let stats_report = Arc::new(SendTransactionServiceStatsReport::default());

        // Channel for sending transactions from receive task to retry task
        // Use unbounded to avoid blocking the receive task
        let (retry_sender, retry_receiver) = mpsc::unbounded_channel();

        let receive_txn_task = Self::receive_txn_task(
            receiver,
            client.clone(),
            retry_sender,
            config.clone(),
            stats_report.clone(),
            exit.clone(),
        );

        let retry_task = Self::retry_task(
            bank_forks.clone(),
            client,
            retry_receiver,
            config,
            stats_report,
            exit.clone(),
        );

        Self {
            receive_txn_task,
            retry_task,
            exit,
        }
    }

    /// Task responsible for receiving transactions from RPC clients.
    fn receive_txn_task<Client: TransactionClient + Send + 'static>(
        mut receiver: mpsc::Receiver<TransactionInfo>,
        client: Client,
        retry_sender: mpsc::UnboundedSender<TransactionInfo>,
        Config {
            batch_send_rate_ms,
            batch_size,
            default_max_retries,
            service_max_retries,
            ..
        }: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        debug!("Starting send-transaction-service-next::receive_txn_task");

        tokio::spawn(async move {
            let mut transactions = HashMap::new();
            let mut last_batch_sent = Instant::now();
            let batch_send_rate = Duration::from_millis(batch_send_rate_ms);

            loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                let stats = &stats_report.stats;

                // Calculate timeout until next batch send
                let elapsed = last_batch_sent.elapsed();
                let timeout = batch_send_rate.saturating_sub(elapsed);

                // Wait for transaction or timeout
                let should_send = if timeout.is_zero() {
                    // Time to send batch
                    true
                } else {
                    match tokio::time::timeout(timeout, receiver.recv()).await {
                        Ok(Some(transaction_info)) => {
                            stats.received_transactions.fetch_add(1, Ordering::Relaxed);

                            // Check for duplicates
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                transactions.entry(transaction_info.signature)
                            {
                                e.insert(transaction_info);
                            } else {
                                stats
                                    .received_duplicate_transactions
                                    .fetch_add(1, Ordering::Relaxed);
                            }

                            // Send if batch is full
                            transactions.len() >= batch_size
                        }
                        Ok(None) => {
                            // Channel closed, send remaining and exit
                            info!("Terminating send-transaction-service-next.");
                            exit.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(_) => {
                            // Timeout, send batch
                            true
                        }
                    }
                };

                if should_send && !transactions.is_empty() {
                    stats
                        .sent_transactions
                        .fetch_add(transactions.len() as u64, Ordering::Relaxed);

                    let wire_transactions = transactions
                        .values()
                        .map(|transaction_info| transaction_info.wire_transaction.clone())
                        .collect::<Vec<Vec<u8>>>();

                    client.send_transactions_in_batch(wire_transactions, stats);

                    let last_sent_time = Instant::now();

                    // Send transactions to retry task
                    for (_signature, mut transaction_info) in transactions.drain() {
                        // Drop transactions with 0 max retries
                        let max_retries = transaction_info
                            .get_max_retries(default_max_retries, service_max_retries);
                        if max_retries == Some(0) {
                            continue;
                        }

                        transaction_info.last_sent_time = Some(last_sent_time);

                        // Send to retry task - unbounded channel so this won't block
                        if retry_sender.send(transaction_info).is_err() {
                            // Retry task has shut down
                            break;
                        }
                    }

                    last_batch_sent = Instant::now();
                    stats_report.report();
                }
            }

            debug!("Exiting send-transaction-service-next::receive_txn_task");
        })
    }

    /// Async task responsible for retrying transactions
    fn retry_task<Client: TransactionClient + Send + 'static>(
        bank_forks: Arc<RwLock<BankForks>>,
        client: Client,
        mut retry_receiver: mpsc::UnboundedReceiver<TransactionInfo>,
        config: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        debug!("Starting send-transaction-service-next::retry_task.");

        tokio::spawn(async move {
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            let retry_interval_ms_default = MAX_RETRY_SLEEP_MS.min(config.retry_rate_ms);

            // Local state - no sharing!
            let mut retry_transactions: HashMap<Signature, TransactionInfo> = HashMap::new();
            let mut next_retry_time =
                Instant::now() + Duration::from_millis(retry_interval_ms_default);

            loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                let now = Instant::now();
                let sleep_duration = if now >= next_retry_time {
                    // Time to process retries
                    Duration::ZERO
                } else {
                    next_retry_time.duration_since(now)
                };

                // Wait for new transactions or retry timeout
                tokio::select! {
                    biased;  // Check messages first

                    msg = retry_receiver.recv() => {
                        match msg {
                            Some(transaction_info) => {
                                let signature = transaction_info.signature;

                                let stats = &stats_report.stats;

                                // Only add if retry pool not full
                                if retry_transactions.len() < config.retry_pool_max_size {
                                    retry_transactions.insert(signature, transaction_info);
                                    stats.retry_queue_size.store(
                                    retry_transactions.len() as u64,
                                    Ordering::Relaxed
                                    );
                                } else {
                                    stats.retry_queue_overflow.fetch_add(1, Ordering::Relaxed);

                                }
                                continue;
                            }
                            None => {
                                // Channel closed, exit
                                break;
                            }
                        }
                    }

                    _ = sleep(sleep_duration) => {
                        // Time to process retries
                    }
                }

                // Drain any remaining messages before processing
                while let Ok(transaction_info) = retry_receiver.try_recv() {
                    let signature = transaction_info.signature;
                    let stats = &stats_report.stats;

                    if retry_transactions.len() < config.retry_pool_max_size {
                        retry_transactions.insert(signature, transaction_info);
                        stats
                            .retry_queue_size
                            .store(retry_transactions.len() as u64, Ordering::Relaxed);
                    } else {
                        // Track overflow when pool is full
                        stats.retry_queue_overflow.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if retry_transactions.is_empty() {
                    next_retry_time =
                        Instant::now() + Duration::from_millis(retry_interval_ms_default);
                } else {
                    let stats = &stats_report.stats;
                    stats
                        .retry_queue_size
                        .store(retry_transactions.len() as u64, Ordering::Relaxed);

                    let BankPair {
                        root_bank,
                        working_bank,
                    } = sharable_banks.load();

                    let result = Self::process_transactions(
                        &working_bank,
                        &root_bank,
                        &mut retry_transactions,
                        &client,
                        &config,
                        stats,
                    );
                    stats_report.report();

                    // Adjust retry interval taking into account the time since the last send
                    let retry_interval_ms = retry_interval_ms_default
                        .checked_sub(
                            result
                                .last_sent_time
                                .and_then(|last| Instant::now().checked_duration_since(last))
                                .and_then(|interval| interval.as_millis().try_into().ok())
                                .unwrap_or(0),
                        )
                        .unwrap_or(retry_interval_ms_default)
                        .max(100);

                    next_retry_time = Instant::now() + Duration::from_millis(retry_interval_ms);
                }
            }
        })
    }

    /// Retry transactions sent before.
    fn process_transactions<Client: TransactionClient + Send + 'static>(
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

            // Check if rooted
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

            // Check durable nonce expiration
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

            // Check if expired by block height
            if transaction_info.last_valid_block_height < root_bank.block_height() {
                info!("Dropping expired transaction: {signature}");
                result.expired += 1;
                stats.expired_transactions.fetch_add(1, Ordering::Relaxed);
                return false;
            }

            let max_retries =
                transaction_info.get_max_retries(default_max_retries, service_max_retries);

            // Check if max retries reached
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
                            // Transaction sent before is unknown to the working bank
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

        // Send batched transactions
        if !batched_transactions.is_empty() {
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

        // Remove transactions that exceeded retries
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

    pub fn new_with_crossbeam<Client: TransactionClient + Clone + Send + 'static>(
        bank_forks: &Arc<RwLock<BankForks>>,
        crossbeam_receiver: CrossbeamReceiver<TransactionInfo>,
        client: Client,
        config: Config,
        exit: Arc<AtomicBool>,
    ) -> Self {
        // Create a tokio channel
        let (tokio_sender, tokio_receiver) = mpsc::channel(1000);

        // Bridge crossbeam to tokio in a background thread
        std::thread::spawn(move || {
            while let Ok(transaction_info) = crossbeam_receiver.recv() {
                // Block on sending to tokio channel
                if tokio_sender.blocking_send(transaction_info).is_err() {
                    // Tokio receiver dropped, exit bridge
                    break;
                }
            }
            debug!("Crossbeam-to-tokio bridge terminated");
        });

        // Create the service with the tokio channel
        Self::new(bank_forks, tokio_receiver, client, config, exit)
    }

    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.receive_txn_task.await?;
        self.exit.store(true, Ordering::Relaxed);
        self.retry_task.await?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            test_utils::{CreateClient, Stoppable},
            transaction_client::TpuClientNextClient,
        },
        crossbeam_channel::unbounded,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn crossbeam_bridge_compatibility() {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = unbounded();

        let client = TpuClientNextClient::create_client(
            Some(Handle::current()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            1,
        );

        let send_transaction_service = SendTransactionServiceNext::new_with_crossbeam(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 100,
                ..Config::default()
            },
            Arc::new(AtomicBool::new(false)),
        );

        // Drop sender to close channel
        drop(sender);
        send_transaction_service.join().await.unwrap();
        client.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn service_exit() {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = mpsc::channel(100);

        let client = TpuClientNextClient::create_client(
            Some(Handle::current()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            1,
        );

        let send_transaction_service = SendTransactionServiceNext::new(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 100,
                ..Config::default()
            },
            Arc::new(AtomicBool::new(false)),
        );

        // Drop sender to close channel
        drop(sender);
        send_transaction_service.join().await.unwrap();
        client.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validator_exit() {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = mpsc::channel(100);

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
        let client = TpuClientNextClient::create_client(
            Some(Handle::current()),
            "127.0.0.1:0".parse().unwrap(),
            None,
            1,
        );

        let send_transaction_service = SendTransactionServiceNext::new(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 100,
                ..Config::default()
            },
            exit.clone(),
        );

        sender.send(dummy_tx_info()).await.unwrap();

        // Send a few more transactions
        for _ in 0..5 {
            let _ = sender.send(dummy_tx_info()).await;
        }

        // Signal exit
        exit.store(true, Ordering::Relaxed);

        // Drop sender to trigger shutdown
        drop(sender);

        // Wait for service to exit with timeout
        let timeout_result =
            tokio::time::timeout(Duration::from_secs(5), send_transaction_service.join()).await;

        assert!(
            timeout_result.is_ok(),
            "Service did not exit within timeout"
        );
        timeout_result.unwrap().unwrap();

        client.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn process_transactions() {
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

        let client = TpuClientNextClient::create_client(
            Some(Handle::current()),
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );

        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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

        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_durable_nonce_transactions() {
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
        let client = TpuClientNextClient::create_client(
            Some(Handle::current()),
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionServiceNext::process_transactions(
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
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionServiceNext::process_transactions(
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
        let result = SendTransactionServiceNext::process_transactions(
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

        // Advance nonce
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
        let result = SendTransactionServiceNext::process_transactions(
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
}
